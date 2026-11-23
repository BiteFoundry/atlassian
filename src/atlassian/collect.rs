//! Walk-every-page helpers.
//!
//! A cache prune may run only on evidence that a listing is complete. [Collected::complete]
//! is that evidence: it is true only when the server said there is nothing more. Reaching a
//! cap, a page that adds nothing new, or a missing cursor all leave it false.

use std::{collections::HashSet, future::Future, hash::Hash};

use anyhow::Result;

use super::AtlassianClient;
use crate::models::{
    AttachmentItem, Collected, ConfluenceComment, IssueComment, IssueSummary, ListPage, PageSummary,
    SpaceSummary, SprintSummary,
};

/// Issues of one board or sprint kept by a sync or a board load.
pub const SYNC_BOARD_ISSUE_CAP: usize = 1000;
/// "My open issues" kept by a sync.
pub const SYNC_MY_ISSUE_CAP: usize = 500;
/// Recently updated issues of the default project kept by a sync.
pub const SYNC_PROJECT_ISSUE_CAP: usize = 500;
/// Root pages (and child pages of one parent) of a space kept by a sync or a tree load.
pub const SYNC_ROOT_PAGE_CAP: usize = 1000;
/// Active and future sprints of one board.
pub const SYNC_SPRINT_CAP: usize = 500;
/// Spaces of one site.
pub const SPACE_CAP: usize = 5000;
/// Comments of one issue or page.
pub const COMMENT_CAP: usize = 1000;
/// Attachments of one page.
pub const ATTACHMENT_CAP: usize = 1000;

/// Calls "fetch" with the cursor of each next page until the server reports the end,
/// de-duplicating by "key". Stops early, with "complete = false", at "cap" items, on a
/// page that adds nothing, or when the server gives no cursor for the rest.
pub(crate) async fn walk_pages<T, K, F, Fut>(
    cap: usize,
    key: impl Fn(&T) -> K,
    mut fetch: F,
) -> Result<Collected<T>>
where
    K: Eq + Hash,
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<ListPage<T>>>,
{
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fetch(cursor.take()).await?;
        let mut added = 0usize;
        for item in page.items {
            if !seen.insert(key(&item)) {
                continue;
            }
            if items.len() >= cap {
                return Ok(Collected { items, complete: false });
            }
            items.push(item);
            added += 1;
        }
        if page.has_more == Some(false) {
            return Ok(Collected { items, complete: true });
        }
        if added == 0 || items.len() >= cap {
            return Ok(Collected { items, complete: false });
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(Collected { items, complete: false }),
        }
    }
}

fn offset_of(cursor: Option<&str>) -> usize {
    cursor.and_then(|c| c.trim().parse().ok()).unwrap_or(0)
}

impl AtlassianClient {
    /// Every issue of a board (or of one sprint), up to "cap".
    pub async fn list_all_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        cap: usize,
    ) -> Result<Collected<IssueSummary>> {
        walk_pages(
            cap,
            |issue: &IssueSummary| issue.key.clone(),
            |cursor| async move {
                self.list_board_issues_page(board_id, sprint_id, 100, offset_of(cursor.as_deref()))
                    .await
            },
        )
        .await
    }

    /// Every active and future sprint of a board, up to "cap".
    pub async fn list_all_board_sprints(
        &self,
        board_id: i64,
        cap: usize,
    ) -> Result<Collected<SprintSummary>> {
        walk_pages(
            cap,
            |sprint: &SprintSummary| sprint.id,
            |cursor| async move {
                self.list_board_sprints_page(board_id, 50, offset_of(cursor.as_deref()))
                    .await
            },
        )
        .await
    }

    /// Every root page of a space, up to "cap".
    pub async fn list_all_space_root_pages(
        &self,
        space_id_or_key: &str,
        cap: usize,
    ) -> Result<Collected<PageSummary>> {
        walk_pages(
            cap,
            |page: &PageSummary| page.id.clone(),
            |cursor| async move {
                self.list_space_root_pages_page(space_id_or_key, 250, cursor.as_deref())
                    .await
            },
        )
        .await
    }

    /// Every direct child of a page, up to "cap".
    pub async fn list_all_child_pages(
        &self,
        page_id: &str,
        cap: usize,
    ) -> Result<Collected<PageSummary>> {
        walk_pages(
            cap,
            |page: &PageSummary| page.id.clone(),
            |cursor| async move { self.list_child_pages_page(page_id, 250, cursor.as_deref()).await },
        )
        .await
    }

    /// Every issue matching "jql", up to "cap" (Cloud page tokens or Data Center offsets).
    pub async fn search_all_issues_jql(&self, jql: &str, cap: usize) -> Result<Collected<IssueSummary>> {
        walk_pages(
            cap,
            |issue: &IssueSummary| issue.key.clone(),
            |cursor| async move {
                // "s<offset>" for Data Center, "t<token>" for Cloud.
                let (token, start) = match cursor.as_deref() {
                    Some(c) if c.starts_with('s') => (None, c[1..].parse::<usize>().ok()),
                    Some(c) if c.starts_with('t') => (Some(&c[1..]), None),
                    _ => (None, None),
                };
                let page = self.search_issues_jql(jql, 100, token, start).await?;
                let next_cursor = page
                    .next_page_token
                    .map(|token| format!("t{token}"))
                    .or_else(|| page.next_start_at.map(|start| format!("s{start}")));
                Ok(ListPage {
                    items: page.issues,
                    has_more: Some(next_cursor.is_some()),
                    next_cursor,
                })
            },
        )
        .await
    }

    /// Every space of the site, up to [SPACE_CAP].
    pub async fn list_all_spaces_collected(&self) -> Result<Collected<SpaceSummary>> {
        walk_pages(
            SPACE_CAP,
            |space: &SpaceSummary| space.id.clone(),
            |cursor| async move { self.list_spaces_page(250, cursor.as_deref()).await },
        )
        .await
    }

    /// Every comment of an issue, up to [COMMENT_CAP], plus the total the server reported.
    pub async fn list_issue_comments_collected(
        &self,
        key: &str,
    ) -> Result<(Collected<IssueComment>, Option<usize>)> {
        // A Mutex (not a Cell) keeps the returned future Send.
        let total = std::sync::Mutex::new(None::<usize>);
        let total_ref = &total;
        let collected = walk_pages(
            COMMENT_CAP,
            |comment: &IssueComment| comment.id.clone(),
            |cursor| async move {
                let (page, page_total) = self
                    .list_issue_comments_page(key, 100, offset_of(cursor.as_deref()))
                    .await?;
                if page_total.is_some() {
                    if let Ok(mut slot) = total_ref.lock() {
                        *slot = page_total;
                    }
                }
                Ok(page)
            },
        )
        .await?;
        let total = total.into_inner().unwrap_or(None);
        Ok((collected, total))
    }

    /// Every footer comment of a page, up to [COMMENT_CAP].
    pub async fn list_page_comments_collected(&self, page_id: &str) -> Result<Collected<ConfluenceComment>> {
        walk_pages(
            COMMENT_CAP,
            |comment: &ConfluenceComment| comment.id.clone(),
            |cursor| async move { self.list_page_comments_cursor_page(page_id, cursor.as_deref()).await },
        )
        .await
    }

    /// Every attachment of a page, up to [ATTACHMENT_CAP].
    pub async fn list_page_attachments_collected(&self, page_id: &str) -> Result<Collected<AttachmentItem>> {
        walk_pages(
            ATTACHMENT_CAP,
            |attachment: &AttachmentItem| attachment.id.clone(),
            |cursor| async move { self.list_page_attachments_page(page_id, cursor.as_deref()).await },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves "total" numbers in pages of "size", as an offset-paged server would.
    async fn numbers(total: usize, size: usize, cursor: Option<String>, say_last: bool) -> Result<ListPage<usize>> {
        let start = offset_of(cursor.as_deref());
        let items: Vec<usize> = (start..total.min(start + size)).collect();
        let end = start + items.len();
        Ok(ListPage {
            has_more: if end >= total {
                Some(false)
            } else if say_last {
                Some(true)
            } else {
                None
            },
            next_cursor: (end < total).then(|| end.to_string()),
            items,
        })
    }

    #[tokio::test]
    async fn walk_pages_collects_every_page_and_reports_complete() {
        let calls = AtomicUsize::new(0);
        let all = walk_pages(1000, |n: &usize| *n, |cursor| {
            calls.fetch_add(1, Ordering::SeqCst);
            numbers(120, 50, cursor, true)
        })
        .await
        .unwrap();
        assert_eq!(all.items, (0..120).collect::<Vec<_>>());
        assert!(all.complete);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // "Unknown" has_more with a cursor is followed until the server says it is the end.
        let unknown = walk_pages(1000, |n: &usize| *n, |cursor| numbers(100, 50, cursor, false))
            .await
            .unwrap();
        assert_eq!(unknown.items.len(), 100);
        assert!(unknown.complete);

        let empty = walk_pages(1000, |n: &usize| *n, |cursor| numbers(0, 50, cursor, true))
            .await
            .unwrap();
        assert!(empty.items.is_empty() && empty.complete);
    }

    #[tokio::test]
    async fn walk_pages_is_incomplete_at_the_cap() {
        let capped = walk_pages(100, |n: &usize| *n, |cursor| numbers(120, 50, cursor, true))
            .await
            .unwrap();
        assert_eq!(capped.items.len(), 100);
        assert!(!capped.complete, "a capped walk is never evidence of a complete listing");

        // Exactly at the cap and the server says it is the end: complete.
        let exact = walk_pages(100, |n: &usize| *n, |cursor| numbers(100, 50, cursor, true))
            .await
            .unwrap();
        assert_eq!(exact.items.len(), 100);
        assert!(exact.complete);

        // Mid-page cap.
        let mid = walk_pages(70, |n: &usize| *n, |cursor| numbers(120, 50, cursor, true))
            .await
            .unwrap();
        assert_eq!(mid.items.len(), 70);
        assert!(!mid.complete);
    }

    #[tokio::test]
    async fn walk_pages_stops_when_a_page_adds_nothing_or_has_no_cursor() {
        // A server that ignores the cursor and repeats page one forever.
        let calls = AtomicUsize::new(0);
        let repeated = walk_pages(1000, |n: &usize| *n, |_cursor| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(ListPage {
                    items: vec![1usize, 2, 3],
                    has_more: Some(true),
                    next_cursor: Some("3".into()),
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(repeated.items, vec![1, 2, 3]);
        assert!(!repeated.complete);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // "More" without a cursor cannot be followed.
        let no_cursor = walk_pages(1000, |n: &usize| *n, |_cursor| async {
            Ok(ListPage {
                items: vec![1usize],
                has_more: Some(true),
                next_cursor: None,
            })
        })
        .await
        .unwrap();
        assert_eq!(no_cursor.items, vec![1]);
        assert!(!no_cursor.complete);
    }

    #[tokio::test]
    async fn walk_pages_returns_the_error_of_a_failed_page() {
        let err = walk_pages(1000, |n: &usize| *n, |cursor| async move {
            if cursor.is_some() {
                anyhow::bail!("page 2 failed");
            }
            numbers(120, 50, cursor, true).await
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("page 2 failed"));
    }
}
