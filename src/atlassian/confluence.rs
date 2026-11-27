//! Confluence REST API (Cloud v2 `/wiki/api/v2` + CQL `/wiki/rest/api/search`, Data Center v1 `/rest/api/content`).

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    adf::{
        blocks_to_markdown, extract_cross_refs, extract_toc, markdown_to_adf,
        markdown_to_storage_xhtml, parse_adf_str, parse_storage_xhtml,
    },
    atlassian::{
        body::{body_change, same_markdown, BodyEdit, BodyEditError},
        client::{
            first_nonempty_str, join_url, json_id_to_string, next_link_page_state,
            parse_attachment_item, AnonymousSignal, HttpStatusError,
        },
        AtlassianClient,
    },
    config::DeploymentKind,
    models::{
        parse_offset_cursor, ConfluenceComment, CrossRefTarget, ListPage, OutgoingPage,
        PageAncestor, PageDetail, PageSummary, PageUpdate, SpaceSummary, UserSummary,
    },
};

/// Subresource fetch health flags returned alongside [`PageDetail`] so callers can
/// preserve previously cached labels, attachments, comments, or ancestors on transient subresource failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageSubresourceStatus {
    pub labels_ok: bool,
    pub attachments_ok: bool,
    pub comments_ok: bool,
    pub ancestors_ok: bool,
}

impl AtlassianClient {
    pub async fn list_spaces_page(
        &self,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ListPage<SpaceSummary>> {
        let lim = limit.clamp(1, 250);
        let cloud = self.deployment() == DeploymentKind::Cloud;
        let start = if cloud {
            0
        } else {
            parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?
        };
        let conf_root = self.require_confluence_root()?;

        let url = match self.deployment() {
            DeploymentKind::Cloud => {
                let mut u = format!("{conf_root}/api/v2/spaces?limit={lim}");
                if let Some(c) = cursor.map(str::trim).filter(|s| !s.is_empty()) {
                    u.push_str("&cursor=");
                    u.push_str(&urlencoding::encode(c));
                }
                u
            }
            DeploymentKind::DataCenter => {
                let mut u = format!(
                    "{conf_root}/rest/api/space?limit={lim}&expand=description.plain,homepage"
                );
                if start > 0 {
                    u.push_str(&format!("&start={start}"));
                }
                u
            }
        };

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("results")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let spaces: Vec<SpaceSummary> = arr
            .iter()
            .map(|s| parse_space_summary(s, &conf_root))
            .collect();
        if let Ok(mut cache) = self.space_id_cache.write() {
            for s in &spaces {
                if !s.key.trim().is_empty() && !s.id.trim().is_empty() {
                    cache.insert(s.key.to_ascii_lowercase(), (s.id.clone(), s.key.clone()));
                }
            }
        }
        let (has_more, next_cursor) = next_link_page_state(&val, cloud, start, spaces.len());
        Ok(ListPage {
            items: spaces,
            has_more,
            next_cursor,
        })
    }

    pub async fn list_spaces(&self, limit: usize) -> Result<Vec<SpaceSummary>> {
        Ok(self.list_spaces_page(limit, None).await?.items)
    }

    /// Every space of the site (all pages, up to the space cap).
    pub async fn list_all_spaces(&self) -> Result<Vec<SpaceSummary>> {
        Ok(self.list_all_spaces_collected().await?.items)
    }

    pub async fn list_space_root_pages_page(
        &self,
        space_id_or_key: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ListPage<PageSummary>> {
        let lim = limit.clamp(1, 250);
        let input = space_id_or_key.trim();
        if input.is_empty() {
            bail!("Please provide a Confluence space key or ID.");
        }

        match self.deployment() {
            DeploymentKind::Cloud => {
                let conf_root = self.require_confluence_root()?;
                let (space_id, known_key) = self.resolve_cloud_space_id(input).await?;
                let encoded_id = urlencoding::encode(&space_id);
                let mut url =
                    format!("{conf_root}/api/v2/spaces/{encoded_id}/pages?depth=root&limit={lim}");
                if let Some(c) = cursor.map(str::trim).filter(|s| !s.is_empty()) {
                    url.push_str("&cursor=");
                    url.push_str(&urlencoding::encode(c));
                }
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let got = arr.len();

                let items: Vec<PageSummary> = arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_cloud_page_summary(p, &conf_root);
                        if summary.space_key.is_empty() {
                            if let Some(ref k) = known_key {
                                summary.space_key = k.clone();
                            }
                        }
                        summary
                    })
                    .filter(|p| p.parent_id.is_none())
                    .collect();
                let (has_more, next_cursor) = next_link_page_state(&val, true, 0, got);
                Ok(ListPage {
                    items,
                    has_more,
                    next_cursor,
                })
            }
            DeploymentKind::DataCenter => {
                let start = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
                let conf_root = self.require_confluence_root()?;
                let encoded_key = urlencoding::encode(input);
                let mut url = format!(
                    "{conf_root}/rest/api/space/{encoded_key}/content/page?depth=root&limit={lim}&expand=version,space,children.page"
                );
                if start > 0 {
                    url.push_str(&format!("&start={start}"));
                }
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .or_else(|| val.get("page").and_then(|p| p.get("results")))
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let got = arr.len();

                let items: Vec<PageSummary> = arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_dc_page_summary(p, &conf_root);
                        if summary.space_key.is_empty() {
                            summary.space_key = input.to_string();
                        }
                        summary
                    })
                    .collect();
                let (has_more, next_cursor) = next_link_page_state(&val, false, start, got);
                Ok(ListPage {
                    items,
                    has_more,
                    next_cursor,
                })
            }
        }
    }

    pub async fn list_space_root_pages(
        &self,
        space_id_or_key: &str,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        Ok(self
            .list_space_root_pages_page(space_id_or_key, limit, None)
            .await?
            .items)
    }

    pub async fn list_child_pages_page(
        &self,
        page_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ListPage<PageSummary>> {
        let lim = limit.clamp(1, 250);
        let parent_id = page_id.trim();
        let encoded_id = urlencoding::encode(parent_id);

        match self.deployment() {
            DeploymentKind::Cloud => {
                let conf_root = self.require_confluence_root()?;
                let mut url = format!("{conf_root}/api/v2/pages/{encoded_id}/children?limit={lim}");
                if let Some(c) = cursor.map(str::trim).filter(|s| !s.is_empty()) {
                    url.push_str("&cursor=");
                    url.push_str(&urlencoding::encode(c));
                }
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let got = arr.len();
                let items: Vec<PageSummary> = arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_cloud_page_summary(p, &conf_root);
                        if summary
                            .parent_id
                            .as_deref()
                            .map(str::trim)
                            .unwrap_or("")
                            .is_empty()
                        {
                            summary.parent_id = Some(parent_id.to_string());
                        }
                        if summary.space_key.trim().is_empty() && !summary.space_id.trim().is_empty()
                        {
                            if let Ok(cache) = self.space_id_cache.read() {
                                if let Some((_, key)) = cache
                                    .values()
                                    .find(|(id, _)| id == summary.space_id.trim())
                                {
                                    summary.space_key = key.clone();
                                }
                            }
                        }
                        summary
                    })
                    .collect();
                let (has_more, next_cursor) = next_link_page_state(&val, true, 0, got);
                Ok(ListPage {
                    items,
                    has_more,
                    next_cursor,
                })
            }
            DeploymentKind::DataCenter => {
                let start = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
                let conf_root = self.require_confluence_root()?;
                let mut url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}/child/page?limit={lim}&expand=version,space,children.page"
                );
                if start > 0 {
                    url.push_str(&format!("&start={start}"));
                }
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let got = arr.len();
                let items: Vec<PageSummary> = arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_dc_page_summary(p, &conf_root);
                        if summary
                            .parent_id
                            .as_deref()
                            .map(str::trim)
                            .unwrap_or("")
                            .is_empty()
                        {
                            summary.parent_id = Some(parent_id.to_string());
                        }
                        summary
                    })
                    .collect();
                let (has_more, next_cursor) = next_link_page_state(&val, false, start, got);
                Ok(ListPage {
                    items,
                    has_more,
                    next_cursor,
                })
            }
        }
    }

    pub async fn list_child_pages(&self, page_id: &str) -> Result<Vec<PageSummary>> {
        Ok(self.list_child_pages_page(page_id, 100, None).await?.items)
    }

    /// The user the Confluence credential belongs to ("{conf_root}/rest/api/user/current";
    /// for Cloud the root already ends in "/wiki"). An anonymous or empty answer is
    /// reported as the 401-style anonymous error, never as a user.
    pub async fn get_confluence_current_user(&self) -> Result<UserSummary> {
        let conf_root = self.require_confluence_root()?;
        let url = format!("{conf_root}/rest/api/user/current");
        let val: Value = self.get_json(&url).await?;
        // Cloud has only accountId; on Data Center the username is the id (as for Jira).
        let id = first_nonempty_str(&val, &["accountId", "username", "userKey"]);
        let username = first_nonempty_str(&val, &["username"]);
        let display_name = first_nonempty_str(&val, &["displayName", "publicName", "username"]);
        let is_anonymous_type = val
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("anonymous"));
        if is_anonymous_type || (id.is_empty() && username.is_empty() && display_name.is_empty()) {
            return Err(self.anonymous_error(
                &self.resolve_api_url(&url),
                &AnonymousSignal::AnonymousPayload,
            ));
        }
        let email = val
            .get("email")
            .or_else(|| val.get("emailAddress"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        Ok(UserSummary {
            id,
            display_name,
            email,
            avatar_url: None,
            active: true,
        })
    }

    /// Lightweight single-request Confluence page revision probe (version, title, updated_at;
    /// excludes body, labels, attachments, and comments).
    pub async fn get_page_revision(&self, page_id: &str) -> Result<PageSummary> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!("{conf_root}/api/v2/pages/{encoded_id}");
                let val: Value = self.get_json(&url).await?;
                Ok(parse_cloud_page_summary(&val, &conf_root))
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}?expand=version,space,ancestors"
                );
                let val: Value = self.get_json(&url).await?;
                Ok(parse_dc_page_summary(&val, &conf_root))
            }
        }
    }

    /// Every attachment of the page (all pages, up to the attachment cap).
    pub async fn list_page_attachments(
        &self,
        page_id: &str,
    ) -> Result<Vec<crate::models::AttachmentItem>> {
        Ok(self.list_page_attachments_collected(page_id).await?.items)
    }

    /// One page of a Confluence page's attachments.
    pub async fn list_page_attachments_page(
        &self,
        page_id: &str,
        cursor: Option<&str>,
    ) -> Result<ListPage<crate::models::AttachmentItem>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let cloud = self.deployment() == DeploymentKind::Cloud;
        let mut start = 0usize;
        let url = if cloud {
            let mut url = format!("{conf_root}/api/v2/pages/{encoded_id}/attachments?limit=250");
            if let Some(c) = cursor.map(str::trim).filter(|s| !s.is_empty()) {
                url.push_str("&cursor=");
                url.push_str(&urlencoding::encode(c));
            }
            url
        } else {
            start = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
            let mut url =
                format!("{conf_root}/rest/api/content/{encoded_id}/child/attachment?limit=100");
            if start > 0 {
                url.push_str(&format!("&start={start}"));
            }
            url
        };
        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("results")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let items: Vec<crate::models::AttachmentItem> = arr
            .iter()
            .map(|a| parse_attachment_item(a, &conf_root))
            .collect();
        let (has_more, next_cursor) = next_link_page_state(&val, cloud, start, arr.len());
        Ok(ListPage {
            items,
            has_more,
            next_cursor,
        })
    }

    pub async fn get_page(&self, page_id: &str) -> Result<PageDetail> {
        let (detail, _) = self.get_page_with_meta(page_id).await?;
        Ok(detail)
    }

    pub async fn get_page_with_meta(
        &self,
        page_id: &str,
    ) -> Result<(PageDetail, PageSubresourceStatus)> {
        self.fetch_page(page_id, false, None).await
    }

    /// Fetches full page detail and subresource health, using the site-scoped SQLite cache first
    /// when resolving Cloud ancestor titles.
    pub async fn get_page_with_meta_in_cache(
        &self,
        page_id: &str,
        site_id: &str,
        storage: &crate::storage::Storage,
    ) -> Result<(PageDetail, PageSubresourceStatus)> {
        self.fetch_page(page_id, false, Some((site_id, storage)))
            .await
    }

    async fn fetch_page(
        &self,
        page_id: &str,
        require_native_body: bool,
        title_cache: Option<(&str, &crate::storage::Storage)>,
    ) -> Result<(PageDetail, PageSubresourceStatus)> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let adf_url = format!(
                    "{conf_root}/api/v2/pages/{encoded_id}?body-format=atlas_doc_format"
                );
                let mut val: Value = self.get_json(&adf_url).await?;
                let mut body_raw = val
                    .get("body")
                    .and_then(|b| b.get("atlas_doc_format"))
                    .and_then(|a| a.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let (body_representation, blocks) = if !body_raw.trim().is_empty() {
                    ("atlas_doc_format".to_string(), parse_adf_str(&body_raw))
                } else {
                    let storage_url =
                        format!("{conf_root}/api/v2/pages/{encoded_id}?body-format=storage");
                    let storage_val: Value = self.get_json(&storage_url).await?;
                    let storage_body = storage_val
                        .pointer("/body/storage/value")
                        .and_then(Value::as_str);
                    if require_native_body && storage_body.is_none() {
                        return Err(
                            BodyEditError("Confluence did not return the original body").into()
                        );
                    }
                    body_raw = storage_body.unwrap_or("").to_owned();
                    if require_native_body || val.get("title").is_none() {
                        val = storage_val;
                    }
                    ("storage".to_string(), parse_storage_xhtml(&body_raw))
                };

                let summary = parse_cloud_page_summary(&val, &conf_root);
                if require_native_body {
                    validate_source_version(&val)?;
                }

                let labels_fut = Box::pin(async {
                    match self
                        .get_json::<Value>(&format!(
                            "{conf_root}/api/v2/pages/{encoded_id}/labels?limit=250"
                        ))
                        .await
                    {
                        Ok(v) => (
                            v.get("results")
                                .and_then(|r| r.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|l| {
                                            l.get("name").and_then(|n| n.as_str()).map(str::to_string)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                            true,
                            v.pointer("/_links/next").and_then(Value::as_str).is_some(),
                        ),
                        Err(_) => (Vec::new(), false, false),
                    }
                });

                let attachments_fut = Box::pin(async {
                    match self
                        .get_json::<Value>(&format!(
                            "{conf_root}/api/v2/pages/{encoded_id}/attachments?limit=250"
                        ))
                        .await
                    {
                        Ok(v) => (
                            v.get("results")
                                .and_then(|r| r.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .map(|a| parse_attachment_item(a, &conf_root))
                                        .collect()
                                })
                                .unwrap_or_default(),
                            true,
                            v.pointer("/_links/next").and_then(Value::as_str).is_some(),
                        ),
                        Err(_) => (Vec::new(), false, false),
                    }
                });

                let comments_fut = Box::pin(async {
                    match self.list_page_comments_page(page_id).await {
                        Ok((c, more)) => (c, true, more),
                        Err(_) => (Vec::new(), false, false),
                    }
                });

                let need_ancestors = !require_native_body
                    && !summary
                        .parent_id
                        .as_deref()
                        .map(str::trim)
                        .unwrap_or("")
                        .is_empty();
                let ancestors_fut = Box::pin(async {
                    if !need_ancestors {
                        (Vec::new(), true)
                    } else {
                        self.fetch_cloud_page_ancestors(&conf_root, page_id.trim(), title_cache)
                            .await
                    }
                });

                let (
                    (labels, labels_ok, labels_more),
                    (attachments, attachments_ok, attachments_more),
                    (comments, comments_ok, comments_more),
                    (ancestors, ancestors_ok),
                ) = tokio::join!(labels_fut, attachments_fut, comments_fut, ancestors_fut);
                let body_markdown = blocks_to_markdown(&blocks);
                let toc = extract_toc(&blocks);
                let linked_issue_keys = extract_linked_issue_keys(&blocks, &body_raw);
                let mut incomplete = Vec::new();
                if !labels_ok || labels_more {
                    incomplete.push("labels".to_string());
                }
                if !attachments_ok || attachments_more {
                    incomplete.push("attachments".to_string());
                }
                if !comments_ok || comments_more {
                    incomplete.push("comments".to_string());
                }
                if !ancestors_ok {
                    incomplete.push("ancestors".to_string());
                }

                Ok((
                    PageDetail {
                        summary,
                        ancestors,
                        labels,
                        body_raw,
                        body_representation,
                        body_markdown,
                        blocks,
                        toc,
                        attachments,
                        comments,
                        linked_issue_keys,
                        incomplete,
                    },
                    PageSubresourceStatus {
                        labels_ok,
                        attachments_ok,
                        comments_ok,
                        ancestors_ok,
                    },
                ))
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}?expand=body.storage,version,space,ancestors,metadata.labels,children.attachment"
                );
                let (val_res, comments_res) = tokio::join!(
                    Box::pin(self.get_json::<Value>(&url)),
                    Box::pin(self.list_page_comments_page(page_id)),
                );
                let val: Value = val_res?;
                let summary = parse_dc_page_summary(&val, &conf_root);
                if require_native_body {
                    validate_source_version(&val)?;
                }

                let ancestors = val
                    .get("ancestors")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|a| PageAncestor {
                                id: json_id_to_string(a.get("id")),
                                title: first_nonempty_str(a, &["title"]),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let labels_more = val
                    .pointer("/metadata/labels/_links/next")
                    .and_then(Value::as_str)
                    .is_some();
                let labels = val
                    .get("metadata")
                    .and_then(|m| m.get("labels"))
                    .and_then(|l| l.get("results"))
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                let attachments_more = val
                    .pointer("/children/attachment/_links/next")
                    .and_then(Value::as_str)
                    .is_some();
                let attachments = val
                    .get("children")
                    .and_then(|c| c.get("attachment"))
                    .and_then(|a| a.get("results"))
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|a| parse_attachment_item(a, &conf_root))
                            .collect()
                    })
                    .unwrap_or_default();

                let storage_body = val.pointer("/body/storage/value").and_then(Value::as_str);
                if require_native_body && storage_body.is_none() {
                    return Err(BodyEditError("Confluence did not return the original body").into());
                }
                let body_raw = storage_body.unwrap_or("").to_owned();
                let blocks = parse_storage_xhtml(&body_raw);
                let body_markdown = blocks_to_markdown(&blocks);
                let toc = extract_toc(&blocks);
                let linked_issue_keys = extract_linked_issue_keys(&blocks, &body_raw);
                let (comments, comments_ok, comments_more) = match comments_res {
                    Ok((c, more)) => (c, true, more),
                    Err(_) => (Vec::new(), false, false),
                };
                let mut incomplete = Vec::new();
                if labels_more {
                    incomplete.push("labels".to_string());
                }
                if attachments_more {
                    incomplete.push("attachments".to_string());
                }
                if !comments_ok || comments_more {
                    incomplete.push("comments".to_string());
                }

                Ok((
                    PageDetail {
                        summary,
                        ancestors,
                        labels,
                        body_raw,
                        body_representation: "storage".to_string(),
                        body_markdown,
                        blocks,
                        toc,
                        attachments,
                        comments,
                        linked_issue_keys,
                        incomplete,
                    },
                    PageSubresourceStatus {
                        labels_ok: true,
                        attachments_ok: true,
                        comments_ok,
                        ancestors_ok: true,
                    },
                ))
            }
        }
    }

    async fn fetch_cloud_page_ancestors(
        &self,
        conf_root: &str,
        page_id: &str,
        title_cache: Option<(&str, &crate::storage::Storage)>,
    ) -> (Vec<PageAncestor>, bool) {
        const ANCESTOR_LIMIT: usize = 250;
        const MAX_SEGMENTS: usize = 20;

        #[derive(Clone)]
        struct RawAncestor {
            id: String,
            kind: String,
            inline_title: String,
        }

        let parse_batch = |val: &Value| -> Vec<RawAncestor> {
            val.get("results")
                .and_then(|r| r.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let id = json_id_to_string(item.get("id"));
                            let trimmed = id.trim();
                            if trimmed.is_empty() {
                                return None;
                            }
                            let kind = first_nonempty_str(item, &["type"]).to_ascii_lowercase();
                            let inline_title = first_nonempty_str(item, &["title"]);
                            Some(RawAncestor {
                                id: trimmed.to_string(),
                                kind,
                                inline_title,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        let first_url = format!(
            "{conf_root}/api/v2/pages/{}/ancestors?limit={ANCESTOR_LIMIT}",
            urlencoding::encode(page_id)
        );
        let first_val = match self.get_json::<Value>(&first_url).await {
            Ok(v) => v,
            Err(_) => return (Vec::new(), false),
        };
        let mut ordered: Vec<RawAncestor> = parse_batch(&first_val);
        let mut last_batch_len = ordered.len();
        let mut complete = true;
        let mut queried_roots: HashSet<String> = HashSet::new();
        queried_roots.insert(page_id.to_string());

        for _ in 0..MAX_SEGMENTS {
            if last_batch_len < ANCESTOR_LIMIT {
                break;
            }
            let Some(head) = ordered.first().cloned() else {
                break;
            };
            if !queried_roots.insert(head.id.clone()) {
                break;
            }
            let seg_url = match head.kind.as_str() {
                "folder" => format!(
                    "{conf_root}/api/v2/folders/{}/ancestors?limit={ANCESTOR_LIMIT}",
                    urlencoding::encode(&head.id)
                ),
                _ => format!(
                    "{conf_root}/api/v2/pages/{}/ancestors?limit={ANCESTOR_LIMIT}",
                    urlencoding::encode(&head.id)
                ),
            };
            match self.get_json::<Value>(&seg_url).await {
                Ok(v) => {
                    let upper = parse_batch(&v);
                    last_batch_len = upper.len();
                    if upper.is_empty() {
                        break;
                    }
                    let mut combined = Vec::with_capacity(upper.len() + ordered.len());
                    combined.extend(upper);
                    combined.extend(ordered);
                    ordered = combined;
                }
                Err(_) => {
                    complete = false;
                    break;
                }
            }
        }

        // Deduplicate while preserving top-most -> direct-parent order.
        let mut seen = HashSet::new();
        let mut deduped = Vec::with_capacity(ordered.len());
        for item in ordered {
            if seen.insert(item.id.clone()) {
                deduped.push(item);
            }
        }

        // Resolve titles: inline title -> site-scoped SQLite cache -> lightweight metadata endpoint -> ID fallback.
        let missing_ids: Vec<String> = deduped
            .iter()
            .filter(|a| a.inline_title.is_empty())
            .map(|a| a.id.clone())
            .collect();
        let cached_titles: HashMap<String, String> = match title_cache {
            Some((site_id, storage)) if !missing_ids.is_empty() => storage
                .get_page_titles_for_site(site_id, &missing_ids)
                .unwrap_or_default(),
            _ => HashMap::new(),
        };

        const ANCESTOR_TITLE_CONCURRENCY: usize = 4;
        let to_fetch: Vec<(String, String)> = deduped
            .iter()
            .filter(|item| item.inline_title.is_empty() && !cached_titles.contains_key(&item.id))
            .map(|item| (item.id.clone(), item.kind.clone()))
            .collect();
        let fetched_titles: HashMap<String, String> = if to_fetch.is_empty() {
            HashMap::new()
        } else {
            stream::iter(to_fetch)
                .map(|(id, kind)| async move {
                    let fetched = self.fetch_cloud_ancestor_title(conf_root, &id, &kind).await;
                    (id, fetched)
                })
                .buffer_unordered(ANCESTOR_TITLE_CONCURRENCY)
                .filter_map(|(id, title)| async move { title.map(|t| (id, t)) })
                .collect()
                .await
        };

        let mut ancestors = Vec::with_capacity(deduped.len());
        for item in deduped {
            let title = if !item.inline_title.is_empty() {
                item.inline_title
            } else if let Some(cached) = cached_titles.get(&item.id) {
                cached.clone()
            } else if let Some(fetched) = fetched_titles.get(&item.id) {
                fetched.clone()
            } else {
                item.id.clone()
            };
            ancestors.push(PageAncestor { id: item.id, title });
        }

        (ancestors, complete)
    }

    async fn fetch_cloud_ancestor_title(
        &self,
        conf_root: &str,
        id: &str,
        kind: &str,
    ) -> Option<String> {
        let encoded = urlencoding::encode(id);
        let segment = match kind {
            "folder" => "folders",
            "whiteboard" => "whiteboards",
            "database" => "databases",
            "embed" => "embeds",
            _ => "pages",
        };
        let url = format!("{conf_root}/api/v2/{segment}/{encoded}");
        let val: Value = self.get_json(&url).await.ok()?;
        let title = first_nonempty_str(&val, &["title"]);
        (!title.is_empty()).then_some(title)
    }

    pub async fn create_page(&self, req: &OutgoingPage) -> Result<PageDetail> {
        let conf_root = self.require_confluence_root()?;

        let created_id = match self.deployment() {
            DeploymentKind::Cloud => {
                let (space_id, _) = self.resolve_cloud_space_id(&req.space_id_or_key).await?;
                let adf_str = serde_json::to_string(&markdown_to_adf(&req.body_markdown))
                    .context("Failed to serialize ADF")?;
                let mut payload = Map::new();
                payload.insert("spaceId".into(), Value::String(space_id));
                payload.insert("status".into(), Value::String("current".into()));
                payload.insert("title".into(), Value::String(req.title.clone()));
                if let Some(pid) = req
                    .parent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload.insert("parentId".into(), Value::String(pid.to_string()));
                }
                payload.insert(
                    "body".into(),
                    json!({
                        "representation": "atlas_doc_format",
                        "value": adf_str,
                    }),
                );

                let resp: Value = self
                    .post_json(&format!("{conf_root}/api/v2/pages"), &Value::Object(payload))
                    .await?;
                json_id_to_string(resp.get("id"))
            }
            DeploymentKind::DataCenter => {
                let storage_html = markdown_to_storage_xhtml(&req.body_markdown);
                let mut payload = Map::new();
                payload.insert("type".into(), Value::String("page".into()));
                payload.insert("title".into(), Value::String(req.title.clone()));
                payload.insert("space".into(), json!({ "key": req.space_id_or_key }));
                if let Some(ref pid) = req
                    .parent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload.insert("ancestors".into(), json!([{ "id": pid }]));
                }
                payload.insert(
                    "body".into(),
                    json!({
                        "storage": {
                            "value": storage_html,
                            "representation": "storage",
                        }
                    }),
                );

                let resp: Value = self
                    .post_json(
                        &format!("{conf_root}/rest/api/content"),
                        &Value::Object(payload),
                    )
                    .await?;
                json_id_to_string(resp.get("id"))
            }
        };

        if created_id.is_empty() {
            bail!("Page creation response is missing an id.");
        }
        self.get_page(&created_id).await
    }

    pub(crate) async fn edit_page(
        &self,
        page_id: &str,
        title: Option<String>,
        body: Option<BodyEdit<'_>>,
        prev_version: Option<i32>,
        version_message: Option<String>,
    ) -> Result<PageDetail> {
        let (current, patch) = self
            .prepare_page_edit(page_id, title, body, prev_version, version_message)
            .await?;
        match patch {
            Some(patch) => self.update_page(page_id, &patch).await,
            None => Ok(current),
        }
    }

    pub(crate) async fn preview_page_edit(
        &self,
        page_id: &str,
        title: Option<String>,
        body: Option<BodyEdit<'_>>,
        prev_version: Option<i32>,
        version_message: Option<String>,
    ) -> Result<Value> {
        let (current, patch) = self
            .prepare_page_edit(page_id, title, body, prev_version, version_message)
            .await?;
        let mut changes = Map::new();
        let prev_version = current.summary.version_number;
        let mut next_version = prev_version;
        if let Some(patch) = &patch {
            let payload = self.page_update_payload(page_id, patch)?;
            next_version = prev_version + 1;
            if patch.title != current.summary.title {
                changes.insert("title".into(), payload["title"].clone());
            }
            if patch.version_message.is_some() {
                changes.insert(
                    "version_message".into(),
                    payload["version"]["message"].clone(),
                );
            }
            let body = match self.deployment() {
                DeploymentKind::Cloud => &payload["body"],
                DeploymentKind::DataCenter => &payload["body"]["storage"],
            };
            let representation = body["representation"].as_str().unwrap_or_default();
            let raw = body["value"].as_str().unwrap_or_default();
            if raw != current.body_raw || representation != current.body_representation {
                let native_body = |format: &str, raw: &str| {
                    if format == "atlas_doc_format" {
                        serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
                    } else {
                        json!(raw)
                    }
                };
                changes.insert(
                    "body".into(),
                    body_change(
                        representation,
                        &native_body(&current.body_representation, &current.body_raw),
                        &native_body(representation, raw),
                    ),
                );
            }
        }
        Ok(json!({
            "page_id": page_id,
            "changed": patch.is_some(),
            "prev_version": prev_version,
            "next_version": next_version,
            "changes": changes,
        }))
    }

    async fn prepare_page_edit(
        &self,
        page_id: &str,
        title: Option<String>,
        body: Option<BodyEdit<'_>>,
        prev_version: Option<i32>,
        version_message: Option<String>,
    ) -> Result<(PageDetail, Option<PageUpdate>)> {
        if title.is_none() && body.is_none() && version_message.is_none() {
            return Err(BodyEditError("at least one page change must be provided").into());
        }
        if title.as_ref().is_some_and(|title| title.trim().is_empty()) {
            return Err(BodyEditError("title must not be empty").into());
        }
        if body.is_some() && prev_version.unwrap_or(0) <= 0 {
            return Err(BodyEditError(
                "prev_version (>= 1) is required for body edits. Read the current page first and use its version_number.",
            ).into());
        }

        if matches!(body, Some(BodyEdit::Section(_))) && self.deployment() != DeploymentKind::Cloud
        {
            return Err(
                BodyEditError("section replacement requires a Confluence Cloud ADF page").into(),
            );
        }
        let (current, _) = self.fetch_page(page_id, true, None).await?;
        let version = current.summary.version_number;
        let prev_version = prev_version.filter(|&v| v > 0).unwrap_or(version);
        check_page_base_version(prev_version, version)?;

        let title = title.unwrap_or_else(|| current.summary.title.clone());
        let mut patch = PageUpdate {
            title,
            prev_version,
            version_message,
            body_raw_passthrough: Some(current.body_raw.clone()),
            body_representation: Some(current.body_representation.clone()),
            ..Default::default()
        };
        let body_changed = match body {
            None => false,
            Some(BodyEdit::Full(md)) if !same_markdown(md, &current.body_markdown) => {
                patch.body_markdown = md.to_owned();
                patch.body_raw_passthrough = None;
                patch.body_representation = None;
                true
            }
            Some(BodyEdit::Full(_)) => false,
            Some(BodyEdit::Append(md)) if md.trim().is_empty() => false,
            Some(edit) => {
                let updated = match current.body_representation.as_str() {
                    "atlas_doc_format" => edit
                        .adf(
                            serde_json::from_str(&current.body_raw)
                                .map_err(|_| BodyEditError("Confluence returned malformed ADF"))?,
                        )?
                        .map(|doc| serde_json::to_string(&doc))
                        .transpose()?,
                    "storage" => edit.storage(&current.body_raw)?,
                    _ => {
                        return Err(
                            BodyEditError("unsupported Confluence body representation").into()
                        )
                    }
                };
                if let Some(raw) = updated {
                    patch.body_raw_passthrough = Some(raw);
                    true
                } else {
                    false
                }
            }
        };
        if !body_changed && patch.title == current.summary.title && patch.version_message.is_none()
        {
            return Ok((current, None));
        }
        Ok((current, Some(patch)))
    }

    pub async fn update_page(&self, page_id: &str, patch: &PageUpdate) -> Result<PageDetail> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{conf_root}/api/v2/pages/{encoded_id}"),
            DeploymentKind::DataCenter => format!("{conf_root}/rest/api/content/{encoded_id}"),
        };
        let payload = self.page_update_payload(page_id, patch)?;
        let _: Value = self.put_json(&url, &payload).await?;
        self.get_page(page_id).await
    }

    fn page_update_payload(&self, page_id: &str, patch: &PageUpdate) -> Result<Value> {
        let next_version = patch.prev_version.checked_add(1).ok_or(BodyEditError(
            "prev_version exceeds the supported version range",
        ))?;
        let version_message = patch.version_message.clone().unwrap_or_default();

        match self.deployment() {
            DeploymentKind::Cloud => {
                let (representation, body_value) = match patch.body_raw_passthrough.as_deref() {
                    Some(raw) => {
                        let repr = patch
                            .body_representation
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| matches!(*s, "atlas_doc_format" | "storage"))
                            .unwrap_or("atlas_doc_format");
                        (repr, raw.to_string())
                    }
                    None => (
                        "atlas_doc_format",
                        serde_json::to_string(&markdown_to_adf(&patch.body_markdown))
                            .context("Failed to serialize ADF")?,
                    ),
                };
                Ok(json!({
                    "id": page_id,
                    "status": "current",
                    "title": patch.title,
                    "body": {
                        "representation": representation,
                        "value": body_value,
                    },
                    "version": {
                        "number": next_version,
                        "message": version_message,
                    }
                }))
            }
            DeploymentKind::DataCenter => {
                let storage_html = match patch.body_raw_passthrough.as_deref() {
                    Some(raw) => raw.to_string(),
                    None => markdown_to_storage_xhtml(&patch.body_markdown),
                };
                Ok(json!({
                    "id": page_id,
                    "type": "page",
                    "title": patch.title,
                    "version": {
                        "number": next_version,
                        "message": version_message,
                    },
                    "body": {
                        "storage": {
                            "value": storage_html,
                            "representation": "storage",
                        }
                    }
                }))
            }
        }
    }

    pub async fn delete_page(&self, page_id: &str) -> Result<()> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{conf_root}/api/v2/pages/{encoded_id}"),
            DeploymentKind::DataCenter => format!("{conf_root}/rest/api/content/{encoded_id}"),
        };
        self.delete_req(&url).await
    }

    pub async fn move_page(
        &self,
        page_id: &str,
        parent_id: &str,
        prev_version: Option<i32>,
    ) -> Result<PageSummary> {
        let (page_id, parent_id) = (page_id.trim(), parent_id.trim());
        if page_id.is_empty() || parent_id.is_empty() || page_id == parent_id {
            bail!("provide a page ID and a different parent page ID");
        }
        if prev_version.is_some_and(|version| version <= 0) {
            bail!("prev_version must be positive");
        }
        let conf_root = self.require_confluence_root()?;
        let page = urlencoding::encode(page_id);
        let parent = urlencoding::encode(parent_id);
        match self.deployment() {
            DeploymentKind::Cloud => {
                let current = self.get_page_revision(page_id).await?;
                if let Some(expected) = prev_version {
                    check_page_base_version(expected, current.version_number)?;
                }
                if current.parent_id.as_deref() == Some(parent_id) {
                    return Ok(current);
                }
                self.put_without_body(&format!(
                    "{conf_root}/rest/api/content/{page}/move/append/{parent}"
                ))
                .await?;
            }
            DeploymentKind::DataCenter => {
                let expected = prev_version.context(
                    "provide the current prev_version for a Confluence Data Center move",
                )?;
                let url = format!("{conf_root}/rest/api/content/{page}");
                let source: Value = self
                    .get_json(&format!("{url}?expand=version,space,ancestors"))
                    .await?;
                if source["type"] != "page" {
                    bail!("Confluence Data Center move requires a page as the source");
                }
                validate_source_version(&source)?;
                let current = parse_dc_page_summary(&source, &conf_root);
                check_page_base_version(expected, current.version_number)?;
                if current.parent_id.as_deref() == Some(parent_id) {
                    return Ok(current);
                }
                let target = self.get_page_revision(parent_id).await?;
                if current.space_key.is_empty() || target.space_key != current.space_key {
                    bail!(
                        "Confluence Data Center page moves require a parent in the current space"
                    );
                }
                let next = current
                    .version_number
                    .checked_add(1)
                    .context("page version exceeds supported range")?;
                self.put_json_empty(
                    &url,
                    &json!({
                        "id": page_id, "type": "page", "title": current.title,
                        "space": {"key": current.space_key},
                        "ancestors": [{"id": parent_id}],
                        "version": {"number": next}
                    }),
                )
                .await?;
            }
        }
        let after = self
            .get_page_revision(page_id)
            .await
            .context("Confluence accepted the page move. Reading it back failed")?;
        if after.parent_id.as_deref() != Some(parent_id) {
            bail!("Confluence accepted the page move. The read did not confirm parent {parent_id}");
        }
        Ok(after)
    }

    pub async fn add_page_labels(&self, page_id: &str, labels: &[String]) -> Result<Vec<String>> {
        let mut seen = HashSet::new();
        if labels.is_empty() {
            bail!("provide at least one label");
        }
        let names: Vec<_> = labels
            .iter()
            .map(|label| validate_page_label(label))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|name| seen.insert(name.to_ascii_lowercase()))
            .map(str::to_owned)
            .collect();
        let url = self.page_label_url(page_id)?;
        let body: Vec<_> = names
            .iter()
            .map(|name| json!({"prefix": "global", "name": name}))
            .collect();
        self.post_json_empty(&url, &body).await?;
        Ok(names)
    }

    pub async fn remove_page_label(&self, page_id: &str, label: &str) -> Result<()> {
        let label = urlencoding::encode(validate_page_label(label)?);
        let url = self.page_label_url(page_id)?;
        self.delete_req(&format!("{url}?name={label}")).await
    }

    fn page_label_url(&self, page_id: &str) -> Result<String> {
        if page_id.trim().is_empty() {
            bail!("page ID must not be empty");
        }
        let root = self.require_confluence_root()?;
        let page = urlencoding::encode(page_id.trim());
        Ok(format!("{root}/rest/api/content/{page}/label"))
    }

    /// First page of a page's footer comments and whether more exist (single request, used
    /// on the page-detail hot path).
    pub async fn list_page_comments_page(
        &self,
        page_id: &str,
    ) -> Result<(Vec<ConfluenceComment>, bool)> {
        let page = self.list_page_comments_cursor_page(page_id, None).await?;
        let has_more = page.has_more == Some(true);
        Ok((page.items, has_more))
    }

    /// One page of a page's footer comments, starting at "cursor".
    pub async fn list_page_comments_cursor_page(
        &self,
        page_id: &str,
        cursor: Option<&str>,
    ) -> Result<ListPage<ConfluenceComment>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let cloud = self.deployment() == DeploymentKind::Cloud;
        let mut start = 0usize;
        let url = if cloud {
            let mut url = format!(
                "{conf_root}/api/v2/pages/{encoded_id}/footer-comments?body-format=atlas_doc_format&limit=100"
            );
            if let Some(c) = cursor.map(str::trim).filter(|s| !s.is_empty()) {
                url.push_str("&cursor=");
                url.push_str(&urlencoding::encode(c));
            }
            url
        } else {
            start = parse_offset_cursor(cursor).map_err(anyhow::Error::msg)?;
            let mut url = format!(
                "{conf_root}/rest/api/content/{encoded_id}/child/comment?expand=body.storage,version&limit=100"
            );
            if start > 0 {
                url.push_str(&format!("&start={start}"));
            }
            url
        };
        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("results")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let items: Vec<ConfluenceComment> = arr
            .iter()
            .map(|c| parse_confluence_comment(c, page_id))
            .collect();
        let (has_more, next_cursor) = next_link_page_state(&val, cloud, start, arr.len());
        Ok(ListPage {
            items,
            has_more,
            next_cursor,
        })
    }

    /// Every footer comment of the page (all pages, up to the comment cap).
    pub async fn list_page_comments(&self, page_id: &str) -> Result<Vec<ConfluenceComment>> {
        Ok(self.list_page_comments_collected(page_id).await?.items)
    }

    pub async fn add_page_comment(
        &self,
        page_id: &str,
        body_markdown: &str,
    ) -> Result<ConfluenceComment> {
        let conf_root = self.require_confluence_root()?;
        let page_id = page_id.trim();

        let resp: Value = match self.deployment() {
            DeploymentKind::Cloud => {
                let adf_str = serde_json::to_string(&markdown_to_adf(body_markdown))
                    .context("Failed to serialize ADF")?;
                let url = format!("{conf_root}/api/v2/footer-comments");
                let payload = json!({
                    "pageId": page_id,
                    "body": {
                        "representation": "atlas_doc_format",
                        "value": adf_str,
                    }
                });
                self.post_json(&url, &payload).await?
            }
            DeploymentKind::DataCenter => {
                let url = format!("{conf_root}/rest/api/content");
                let payload = json!({
                    "type": "comment",
                    "container": {
                        "id": page_id,
                        "type": "page",
                    },
                    "body": {
                        "storage": {
                            "value": markdown_to_storage_xhtml(body_markdown),
                            "representation": "storage",
                        }
                    }
                });
                self.post_json(&url, &payload).await?
            }
        };

        Ok(parse_confluence_comment(&resp, page_id))
    }

    pub async fn search_confluence_cql(
        &self,
        cql: &str,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        let conf_root = self.require_confluence_root()?;
        let lim = limit.clamp(1, 100);
        let encoded_cql = urlencoding::encode(cql.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!("{conf_root}/rest/api/search?cql={encoded_cql}&limit={lim}");
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                let mut out = Vec::new();
                for item in arr {
                    let content = item.get("content").unwrap_or(item);
                    let kind = content
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("page");
                    if !matches!(kind, "page" | "blogpost" | "") {
                        continue;
                    }
                    let mut summary = parse_dc_page_summary(content, &conf_root);
                    if summary.title.is_empty() {
                        summary.title = first_nonempty_str(item, &["title"]);
                    }
                    if summary.web_url.is_empty() {
                        let raw_url = first_nonempty_str(item, &["url"]);
                        summary.web_url = join_url(&conf_root, &raw_url);
                    }
                    if summary.space_key.is_empty() {
                        summary.space_key = item
                            .get("resultGlobalContainer")
                            .and_then(|c| c.get("displayUrl"))
                            .and_then(|v| v.as_str())
                            .and_then(extract_space_key_from_webui)
                            .or_else(|| extract_space_key_from_webui(&summary.web_url))
                            .unwrap_or_default();
                    }
                    if summary.updated_at.is_empty() {
                        summary.updated_at = first_nonempty_str(item, &["lastModified"]);
                    }
                    if !summary.id.is_empty() || !summary.title.is_empty() {
                        out.push(summary);
                    }
                }
                Ok(out)
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/search?cql={encoded_cql}&limit={lim}&expand=space,version"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|p| parse_dc_page_summary(p, &conf_root))
                    .collect())
            }
        }
    }

    async fn resolve_cloud_space_id(&self, space_id_or_key: &str) -> Result<(String, Option<String>)> {
        let trimmed = space_id_or_key.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Ok((trimmed.to_string(), None));
        }
        if let Ok(cache) = self.space_id_cache.read() {
            if let Some((id, key)) = cache.get(&trimmed.to_ascii_lowercase()) {
                return Ok((id.clone(), Some(key.clone())));
            }
        }
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        loop {
            let page = self.list_spaces_page(250, cursor.as_deref()).await?;
            if let Some(found) = page
                .items
                .iter()
                .find(|s| s.key.eq_ignore_ascii_case(trimmed) || s.id == trimmed)
            {
                return Ok((found.id.clone(), Some(found.key.clone())));
            }
            let mut added = 0usize;
            for space in &page.items {
                if seen.insert(space.id.clone()) {
                    added += 1;
                }
            }
            if added == 0 {
                break;
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        bail!("Confluence space '{}' not found.", trimmed)
    }
}

fn parse_space_summary(val: &Value, conf_root: &str) -> SpaceSummary {
    let key = first_nonempty_str(val, &["key"]);
    let id = {
        let raw = json_id_to_string(val.get("id"));
        if raw.is_empty() {
            key.clone()
        } else {
            raw
        }
    };
    let homepage_id = val
        .get("homepageId")
        .or_else(|| val.get("homepage").and_then(|h| h.get("id")))
        .map(|v| json_id_to_string(Some(v)))
        .filter(|s| !s.is_empty());

    let description = val
        .get("description")
        .and_then(|d| {
            d.get("plain")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.as_str())
                .or_else(|| d.as_str())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);

    SpaceSummary {
        id,
        key,
        name: first_nonempty_str(val, &["name"]),
        space_type: first_nonempty_str(val, &["type"]).to_ascii_lowercase(),
        homepage_id,
        description,
        web_url,
        pinned: false,
    }
}

fn validate_source_version(page: &Value) -> Result<()> {
    page.pointer("/version/number")
        .and_then(Value::as_i64)
        .and_then(|version| i32::try_from(version).ok())
        .filter(|&version| version > 0)
        .ok_or_else(|| BodyEditError("Confluence did not return a valid current version").into())
        .map(|_| ())
}

fn check_page_base_version(expected: i32, actual: i32) -> Result<()> {
    if expected != actual {
        return Err(HttpStatusError {
            status: reqwest::StatusCode::CONFLICT,
            message: format!(
                "Confluence version conflict: expected {expected}, current version is {actual}"
            ),
            body: String::new(),
        }
        .into());
    }
    Ok(())
}

fn validate_page_label(label: &str) -> Result<&str> {
    let label = label.trim();
    if label.is_empty() || label.chars().any(char::is_whitespace) {
        bail!("label names must be nonempty and contain no whitespace");
    }
    Ok(label)
}

fn parse_cloud_page_summary(val: &Value, conf_root: &str) -> PageSummary {
    let id = json_id_to_string(val.get("id"));
    let space_id = json_id_to_string(val.get("spaceId"));
    let parent_id = val
        .get("parentId")
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                let s = json_id_to_string(Some(v));
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
        });
    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);
    let space_key = first_nonempty_str(val, &["spaceKey"])
        .if_empty(|| extract_space_key_from_webui(webui).unwrap_or_default());

    let version = val.get("version").unwrap_or(&Value::Null);
    let version_number = version
        .get("number")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let author_name = version
        .get("authorId")
        .or_else(|| val.get("authorId"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let updated_at = first_nonempty_str(version, &["createdAt"])
        .if_empty(|| first_nonempty_str(val, &["createdAt"]));
    let position = val
        .get("position")
        .or_else(|| val.get("childPosition"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    PageSummary {
        id,
        space_id,
        space_key,
        parent_id,
        title: first_nonempty_str(val, &["title"]),
        status: first_nonempty_str(val, &["status"]).if_empty(|| "current".into()),
        version_number,
        author_name,
        updated_at,
        position,
        has_children: None,
        web_url,
    }
}

fn parse_dc_page_summary(val: &Value, conf_root: &str) -> PageSummary {
    let id = json_id_to_string(val.get("id"));
    let space = val.get("space").unwrap_or(&Value::Null);
    let space_id = json_id_to_string(space.get("id"));
    let space_key = first_nonempty_str(space, &["key"]);
    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);
    let space_key = if space_key.is_empty() {
        extract_space_key_from_webui(webui).unwrap_or_default()
    } else {
        space_key
    };

    let parent_id = val
        .get("ancestors")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .map(|a| json_id_to_string(a.get("id")))
        .filter(|s| !s.is_empty());

    let version = val.get("version").unwrap_or(&Value::Null);
    let version_number = version
        .get("number")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let author_name = version
        .get("by")
        .and_then(|b| b.get("displayName"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let updated_at = first_nonempty_str(version, &["when"]);
    let position = val
        .get("extensions")
        .and_then(|e| e.get("position"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    PageSummary {
        id,
        space_id,
        space_key,
        parent_id,
        title: first_nonempty_str(val, &["title"]),
        status: first_nonempty_str(val, &["status"]).if_empty(|| "current".into()),
        version_number,
        author_name,
        updated_at,
        position,
        has_children: parse_dc_has_children(val),
        web_url,
    }
}

/// Reads `children.page` of a Data Center content object (present when the request asked for
/// `expand=children.page`): `Some(true)` when it lists or counts a child, `Some(false)` when it is
/// an expanded, empty listing, `None` when the expansion is absent and nothing is known.
fn parse_dc_has_children(val: &Value) -> Option<bool> {
    let listing = val.get("children")?.get("page")?;
    if !listing.is_object() {
        return None;
    }
    let size = listing.get("size").and_then(Value::as_u64);
    let results = listing.get("results").and_then(Value::as_array);
    if size.is_some_and(|n| n > 0) || results.is_some_and(|r| !r.is_empty()) {
        return Some(true);
    }
    if size == Some(0) || results.is_some() {
        return Some(false);
    }
    None
}

fn parse_confluence_comment(val: &Value, page_id: &str) -> ConfluenceComment {
    let id = json_id_to_string(val.get("id"));
    let version = val.get("version").unwrap_or(&Value::Null);
    let author_name = version
        .get("by")
        .and_then(|b| b.get("displayName"))
        .and_then(|v| v.as_str())
        .or_else(|| version.get("authorId").and_then(|v| v.as_str()))
        .or_else(|| val.get("authorId").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let created_at = first_nonempty_str(version, &["createdAt", "when"])
        .if_empty(|| first_nonempty_str(val, &["createdAt"]));

    let body = val.get("body").unwrap_or(&Value::Null);
    let adf_raw = body
        .get("atlas_doc_format")
        .and_then(|a| a.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let storage_raw = body
        .get("storage")
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let (body_raw, blocks) = if !adf_raw.trim().is_empty() {
        (adf_raw.to_string(), parse_adf_str(adf_raw))
    } else if !storage_raw.trim().is_empty() {
        (storage_raw.to_string(), parse_storage_xhtml(storage_raw))
    } else {
        (String::new(), Vec::new())
    };
    let body_markdown = blocks_to_markdown(&blocks);

    ConfluenceComment {
        id,
        page_id: page_id.to_string(),
        author_name,
        body_raw,
        body_markdown,
        blocks,
        created_at,
    }
}

fn extract_linked_issue_keys(blocks: &[crate::models::DocBlock], raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for xref in extract_cross_refs(blocks, raw) {
        if let CrossRefTarget::JiraIssue { key, .. } = xref {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
    }
    keys
}

fn extract_space_key_from_webui(webui: &str) -> Option<String> {
    Regex::new(r"/spaces/([^/?#]+)")
        .ok()
        .and_then(|re| re.captures(webui))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

trait StrFallback {
    fn if_empty<F: FnOnce() -> String>(self, f: F) -> String;
}

impl StrFallback for String {
    fn if_empty<F: FnOnce() -> String>(self, f: F) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredCredentials};
    use axum::{
        extract::Query,
        http::StatusCode as AxumStatus,
        routing::{get, post},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn make_client(site_url: &str, conf_url: &str, deployment: DeploymentKind) -> AtlassianClient {
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: site_url.to_string(),
                token: "tok".into(),
                deployment,
            },
        );
        AtlassianClient::with_base_urls(site_url, conf_url, deployment, auth).unwrap()
    }

    #[test]
    fn dc_page_summary_reads_children_page_expansion() {
        let has_children = |children: Value| {
            let mut val = json!({
                "id": "10",
                "title": "Space Home",
                "status": "current",
                "space": { "id": 1, "key": "ENG" },
                "version": { "number": 3 }
            });
            if !children.is_null() {
                val["children"] = children;
            }
            parse_dc_page_summary(&val, "https://wiki.example.com").has_children
        };

        // size > 0, or listed results (size omitted): has children.
        assert_eq!(
            has_children(json!({ "page": { "results": [], "start": 0, "limit": 0, "size": 4 } })),
            Some(true)
        );
        assert_eq!(
            has_children(json!({ "page": { "results": [{ "id": "11", "title": "Kid" }] } })),
            Some(true)
        );
        // Expanded and empty: confirmed leaf.
        assert_eq!(
            has_children(json!({ "page": { "results": [], "start": 0, "limit": 25, "size": 0 } })),
            Some(false)
        );
        // Not expanded: unknown.
        assert_eq!(has_children(Value::Null), None);
        assert_eq!(
            has_children(json!({ "attachment": { "results": [], "size": 0 } })),
            None
        );
        assert_eq!(
            has_children(json!({ "_expandable": { "page": "/rest/api/content/10/child/page" } })),
            None
        );
    }

    #[tokio::test]
    async fn dc_tree_listings_expand_children_page_and_report_has_children() {
        let expands: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (roots_seen, kids_seen) = (expands.clone(), expands.clone());
        let app = Router::new()
            .route(
                "/rest/api/space/ENG/content/page",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let seen = roots_seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push(q.get("expand").cloned().unwrap_or_default());
                        Json(json!({
                            "results": [
                                { "id": "1", "title": "Space Home", "space": { "key": "ENG" },
                                  "children": { "page": { "results": [], "size": 3 } } },
                                { "id": "2", "title": "Archive", "space": { "key": "ENG" },
                                  "children": { "page": { "results": [], "size": 0 } } }
                            ],
                            "size": 2,
                            "_links": {}
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/content/1/child/page",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let seen = kids_seen.clone();
                    async move {
                        seen.lock()
                            .unwrap()
                            .push(q.get("expand").cloned().unwrap_or_default());
                        Json(json!({
                            "results": [
                                { "id": "3", "title": "Guide", "space": { "key": "ENG" },
                                  "children": { "page": { "results": [{ "id": "5" }] } } },
                                { "id": "4", "title": "FAQ", "space": { "key": "ENG" },
                                  "children": { "page": { "results": [], "size": 0 } } }
                            ],
                            "size": 2,
                            "_links": {}
                        }))
                    }
                }),
            );
        let base_url = spawn_router(app).await;
        let dc = make_client(&base_url, &base_url, DeploymentKind::DataCenter);

        let roots = dc.list_space_root_pages_page("ENG", 50, None).await.unwrap();
        assert_eq!(
            roots.items.iter().map(|p| p.has_children).collect::<Vec<_>>(),
            vec![Some(true), Some(false)]
        );
        let kids = dc.list_child_pages_page("1", 50, None).await.unwrap();
        assert_eq!(
            kids.items.iter().map(|p| p.has_children).collect::<Vec<_>>(),
            vec![Some(true), Some(false)]
        );
        assert!(kids.items.iter().all(|p| p.parent_id.as_deref() == Some("1")));

        let expands = expands.lock().unwrap().clone();
        assert_eq!(expands.len(), 2, "{expands:?}");
        assert!(
            expands
                .iter()
                .all(|e| e.split(',').any(|part| part == "children.page")),
            "both DC tree listings must expand children.page: {expands:?}"
        );
    }

    #[tokio::test]
    async fn test_confluence_cloud_v2_spaces_pages_crud_comments_and_cql() {
        let updated_version: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
        let uv = updated_version.clone();

        let app = Router::new()
            .route(
                "/wiki/api/v2/spaces",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "999",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "homepageId": "100",
                            "_links": { "webui": "/spaces/ENG/overview" }
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/spaces/999/pages",
                get(|| async {
                    Json(json!({
                        "results": [
                            {
                                "id": "100",
                                "spaceId": "999",
                                "parentId": null,
                                "title": "Root Home",
                                "status": "current",
                                "version": { "number": 1, "createdAt": "2026-11-01T00:00:00Z" },
                                "_links": { "webui": "/spaces/ENG/pages/100/Root+Home" }
                            },
                            {
                                "id": "101",
                                "spaceId": "999",
                                "parentId": "100",
                                "title": "Child Filtered Out",
                                "status": "current",
                                "version": { "number": 1 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/100/children",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "101",
                            "spaceId": "999",
                            "parentId": "100",
                            "title": "Child Page",
                            "status": "current",
                            "version": { "number": 2 }
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["spaceId"], "999");
                    assert_eq!(body["body"]["representation"], "atlas_doc_format");
                    Json(json!({ "id": "200" }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/200",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let fmt = q.get("body-format").map(String::as_str).unwrap_or("");
                    if fmt == "atlas_doc_format" {
                        Json(json!({
                            "id": "200",
                            "spaceId": "999",
                            "title": "New Spec",
                            "status": "current",
                            "version": { "number": 3, "createdAt": "2026-11-15T12:00:00Z" },
                            "_links": { "webui": "/spaces/ENG/pages/200/New+Spec" },
                            "body": {
                                "atlas_doc_format": {
                                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}"
                                }
                            }
                        }))
                    } else {
                        Json(json!({ "id": "200", "body": { "storage": { "value": "<p>fallback</p>" } } }))
                    }
                })
                .put(move |Json(body): Json<Value>| {
                    let uv = uv.clone();
                    async move {
                        *uv.lock().unwrap() = body["version"]["number"].as_i64();
                        Json(json!({ "id": "200" }))
                    }
                })
                .delete(|| async { AxumStatus::NO_CONTENT }),
            )
            .route(
                "/wiki/api/v2/pages/200/labels",
                get(|| async { Json(json!({ "results": [{ "name": "architecture" }] })) }),
            )
            .route(
                "/wiki/api/v2/pages/200/attachments",
                get(|| async { Json(json!({ "results": [] })) }),
            )
            .route(
                "/wiki/api/v2/pages/200/footer-comments",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "cm-1",
                            "version": { "authorId": "acc-1", "createdAt": "2026-11-15T12:01:00Z" },
                            "body": {
                                "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/footer-comments",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["pageId"], "200");
                    assert_eq!(body["body"]["representation"], "atlas_doc_format");
                    Json(json!({
                        "id": "cm-2",
                        "version": { "authorId": "acc-1", "createdAt": "2026-11-15T12:02:00Z" },
                        "body": body["body"]
                    }))
                }),
            )
            .route(
                "/wiki/rest/api/search",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert!(q.get("cql").unwrap().contains("Spec"));
                    Json(json!({
                        "results": [{
                            "content": {
                                "id": "200",
                                "type": "page",
                                "title": "New Spec",
                                "space": { "key": "ENG" },
                                "version": { "number": 3 },
                                "_links": { "webui": "/spaces/ENG/pages/200/New+Spec" }
                            }
                        }]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, "", DeploymentKind::Cloud);

        let spaces = client.list_spaces(25).await.unwrap();
        assert_eq!(spaces[0].id, "999");
        assert_eq!(spaces[0].key, "ENG");

        let roots = client.list_space_root_pages("ENG", 25).await.unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, "100");
        assert_eq!(roots[0].space_key, "ENG");

        let children = client.list_child_pages("100").await.unwrap();
        assert_eq!(children[0].id, "101");

        let created = client
            .create_page(&OutgoingPage {
                space_id_or_key: "ENG".into(),
                parent_id: Some("100".into()),
                title: "New Spec".into(),
                body_markdown: "# Hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(created.summary.id, "200");
        assert_eq!(created.labels, vec!["architecture"]);
        assert_eq!(created.comments.len(), 1);

        let _updated = client
            .update_page(
                "200",
                &PageUpdate {
                    title: "Updated Spec".into(),
                    body_markdown: "Updated body".into(),
                    prev_version: 3,
                    version_message: Some("bump".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(*updated_version.lock().unwrap(), Some(4));

        let comment = client.add_page_comment("200", "LGTM").await.unwrap();
        assert_eq!(comment.id, "cm-2");

        let hits = client
            .search_confluence_cql("title ~ \"Spec\"", 10)
            .await
            .unwrap();
        assert_eq!(hits[0].id, "200");
        assert_eq!(hits[0].space_key, "ENG");

        client.delete_page("200").await.unwrap();
    }

    #[tokio::test]
    async fn test_confluence_datacenter_v1_fallback() {
        let app = Router::new()
            .route(
                "/rest/api/space",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "key": "DC",
                            "name": "DC Space",
                            "type": "global",
                            "_links": { "webui": "/display/DC" }
                        }]
                    }))
                }),
            )
            .route(
                "/rest/api/space/DC/content/page",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "501",
                            "title": "DC Home",
                            "space": { "key": "DC" },
                            "version": { "number": 2, "when": "2026-11-10T00:00:00Z" },
                            "_links": { "webui": "/display/DC/DC+Home" }
                        }]
                    }))
                }),
            )
            .route(
                "/rest/api/content/501",
                get(|| async {
                    Json(json!({
                        "id": "501",
                        "title": "DC Home",
                        "space": { "key": "DC" },
                        "version": { "number": 2, "by": { "displayName": "Admin" }, "when": "2026-11-10T00:00:00Z" },
                        "ancestors": [{ "id": "500", "title": "Top" }],
                        "metadata": { "labels": { "results": [{ "name": "ops" }] } },
                        "children": { "attachment": { "results": [] } },
                        "body": { "storage": { "value": "<p>DC body</p>" } }
                    }))
                })
                .put(|Json(body): Json<Value>| async move {
                    assert_eq!(body["version"]["number"], 3);
                    assert_eq!(body["body"]["storage"]["representation"], "storage");
                    Json(json!({ "id": "501" }))
                }),
            )
            .route(
                "/rest/api/content/501/child/comment",
                get(|| async { Json(json!({ "results": [] })) }),
            )
            .route(
                "/rest/api/content/search",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "501",
                            "title": "DC Home",
                            "space": { "key": "DC" },
                            "version": { "number": 2 }
                        }]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, &base_url, DeploymentKind::DataCenter);

        let spaces = client.list_spaces(10).await.unwrap();
        assert_eq!(spaces[0].key, "DC");

        let roots = client.list_space_root_pages("DC", 10).await.unwrap();
        assert_eq!(roots[0].id, "501");

        let page = client.get_page("501").await.unwrap();
        assert_eq!(page.body_representation, "storage");
        assert_eq!(page.ancestors[0].id, "500");
        assert_eq!(page.labels, vec!["ops"]);

        let updated = client
            .update_page(
                "501",
                &PageUpdate {
                    title: "DC Home v3".into(),
                    body_markdown: "Updated".into(),
                    prev_version: 2,
                    version_message: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.summary.id, "501");

        let hits = client.search_confluence_cql("text ~ \"DC\"", 10).await.unwrap();
        assert_eq!(hits[0].id, "501");
    }

    #[tokio::test]
    async fn confluence_pages_follow_next_links_cloud_and_dc() {
        let app = Router::new()
            .route(
                "/wiki/api/v2/spaces/999/pages",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q.get("cursor").map(String::as_str) == Some("r2=") {
                        Json(json!({
                            "results": [
                                { "id": "203", "spaceId": "999", "parentId": null, "title": "Root 3", "status": "current", "version": { "number": 1 } }
                            ]
                        }))
                    } else {
                        Json(json!({
                            "results": [
                                { "id": "201", "spaceId": "999", "parentId": null, "title": "Root 1", "status": "current", "version": { "number": 1 } },
                                { "id": "202", "spaceId": "999", "parentId": null, "title": "Root 2", "status": "current", "version": { "number": 1 } }
                            ],
                            "_links": { "next": "/wiki/api/v2/spaces/999/pages?depth=root&limit=2&cursor=r2%3D" }
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/space/ENG/content/page",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let start = q
                        .get("start")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    if start == 0 {
                        Json(json!({
                            "results": [
                                { "id": "201", "title": "Root 1", "space": { "key": "ENG" }, "version": { "number": 1 } },
                                { "id": "202", "title": "Root 2", "space": { "key": "ENG" }, "version": { "number": 1 } }
                            ],
                            "_links": { "next": "/rest/api/space/ENG/content/page?depth=root&limit=2&start=2" }
                        }))
                    } else {
                        Json(json!({
                            "results": [
                                { "id": "203", "title": "Root 3", "space": { "key": "ENG" }, "version": { "number": 1 } }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/100/children",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q.get("cursor").map(String::as_str) == Some("abc=") {
                        Json(json!({
                            "results": [
                                { "id": "103", "spaceId": "999", "parentId": "100", "title": "Child 3", "status": "current", "version": { "number": 1 } }
                            ]
                        }))
                    } else {
                        Json(json!({
                            "results": [
                                { "id": "101", "spaceId": "999", "parentId": "100", "title": "Child 1", "status": "current", "version": { "number": 1 } },
                                { "id": "102", "spaceId": "999", "parentId": "100", "title": "Child 2", "status": "current", "version": { "number": 1 } }
                            ],
                            "_links": { "next": "/wiki/api/v2/pages/100/children?limit=2&cursor=abc%3D" }
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/content/100/child/page",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let start = q
                        .get("start")
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    if start == 0 {
                        Json(json!({
                            "results": [
                                { "id": "101", "title": "Child 1", "space": { "key": "ENG" }, "version": { "number": 1 } },
                                { "id": "102", "title": "Child 2", "space": { "key": "ENG" }, "version": { "number": 1 } }
                            ],
                            "_links": { "next": "/rest/api/content/100/child/page?limit=2&start=2" }
                        }))
                    } else {
                        Json(json!({
                            "results": [
                                { "id": "103", "title": "Child 3", "space": { "key": "ENG" }, "version": { "number": 1 } }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/spaces",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    match q.get("cursor").map(String::as_str) {
                        None => Json(json!({
                            "results": [{ "id": "1", "key": "S1", "name": "Space 1", "type": "global" }],
                            "_links": { "next": "/wiki/api/v2/spaces?limit=250&cursor=p2" }
                        })),
                        Some("p2") => Json(json!({
                            "results": [{ "id": "2", "key": "S2", "name": "Space 2", "type": "global" }],
                            "_links": { "next": "/wiki/api/v2/spaces?limit=250&cursor=p3" }
                        })),
                        _ => Json(json!({
                            "results": [{ "id": "3", "key": "S3", "name": "Space 3", "type": "global" }]
                        })),
                    }
                }),
            )
            .route(
                "/rest/api/space",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    match q.get("start").and_then(|s| s.parse::<usize>().ok()).unwrap_or(0) {
                        0 => Json(json!({
                            "results": [{ "id": "1", "key": "S1", "name": "Space 1", "type": "global" }],
                            "_links": { "next": "/rest/api/space?limit=250&start=1" }
                        })),
                        1 => Json(json!({
                            "results": [{ "id": "2", "key": "S2", "name": "Space 2", "type": "global" }],
                            "_links": { "next": "/rest/api/space?limit=250&start=2" }
                        })),
                        _ => Json(json!({
                            "results": [{ "id": "3", "key": "S3", "name": "Space 3", "type": "global" }]
                        })),
                    }
                }),
            );

        let base_url = spawn_router(app).await;
        let cloud_client = make_client(&base_url, &format!("{base_url}/wiki"), DeploymentKind::Cloud);
        let dc_client = make_client(&base_url, &base_url, DeploymentKind::DataCenter);

        let c1 = cloud_client
            .list_child_pages_page("100", 2, None)
            .await
            .unwrap();
        assert_eq!(c1.items.len(), 2);
        assert_eq!(c1.has_more, Some(true));
        assert_eq!(c1.next_cursor.as_deref(), Some("abc="));
        assert!(c1.items.iter().all(|p| p.has_children.is_none()));

        let c2 = cloud_client
            .list_child_pages_page("100", 2, c1.next_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(c2.items.len(), 1);
        assert_eq!(c2.items[0].id, "103");
        assert_eq!(c2.has_more, Some(false));
        assert_eq!(c2.next_cursor, None);

        let d1 = dc_client
            .list_child_pages_page("100", 2, None)
            .await
            .unwrap();
        assert_eq!(d1.items.len(), 2);
        assert_eq!(d1.has_more, Some(true));
        assert_eq!(d1.next_cursor.as_deref(), Some("2"));

        let d2 = dc_client
            .list_child_pages_page("100", 2, d1.next_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(d2.items.len(), 1);
        assert_eq!(d2.items[0].id, "103");
        assert_eq!(d2.has_more, Some(false));
        assert_eq!(d2.next_cursor, None);

        let cloud_spaces = cloud_client.list_all_spaces().await.unwrap();
        assert_eq!(
            cloud_spaces.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            vec!["S1", "S2", "S3"]
        );
        let dc_spaces = dc_client.list_all_spaces().await.unwrap();
        assert_eq!(
            dc_spaces.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
            vec!["S1", "S2", "S3"]
        );

        let cr1 = cloud_client
            .list_space_root_pages_page("999", 2, None)
            .await
            .unwrap();
        assert_eq!(cr1.items.len(), 2);
        assert_eq!(cr1.has_more, Some(true));
        assert_eq!(cr1.next_cursor.as_deref(), Some("r2="));
        let cr2 = cloud_client
            .list_space_root_pages_page("999", 2, cr1.next_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(cr2.items.len(), 1);
        assert_eq!(cr2.items[0].id, "203");
        assert_eq!(cr2.has_more, Some(false));
        assert_eq!(cr2.next_cursor, None);

        let dr1 = dc_client
            .list_space_root_pages_page("ENG", 2, None)
            .await
            .unwrap();
        assert_eq!(dr1.items.len(), 2);
        assert_eq!(dr1.has_more, Some(true));
        assert_eq!(dr1.next_cursor.as_deref(), Some("2"));
        let dr2 = dc_client
            .list_space_root_pages_page("ENG", 2, dr1.next_cursor.as_deref())
            .await
            .unwrap();
        assert_eq!(dr2.items.len(), 1);
        assert_eq!(dr2.items[0].id, "203");
        assert_eq!(dr2.has_more, Some(false));
        assert_eq!(dr2.next_cursor, None);
    }

    #[tokio::test]
    async fn cloud_space_key_resolution_follows_next_pages() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let space_requests = Arc::new(AtomicUsize::new(0));
        let sr = space_requests.clone();

        let app = Router::new()
            .route(
                "/wiki/api/v2/spaces",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let sr = sr.clone();
                    async move {
                        sr.fetch_add(1, Ordering::SeqCst);
                        match q.get("cursor").map(String::as_str) {
                            None => Json(json!({
                                "results": [{ "id": "101", "key": "ENG", "name": "Engineering", "type": "global" }],
                                "_links": { "next": "/wiki/api/v2/spaces?limit=250&cursor=page2" }
                            })),
                            Some("page2") => Json(json!({
                                "results": [{ "id": "202", "key": "LATE", "name": "Late Space", "type": "global" }]
                            })),
                            _ => Json(json!({ "results": [] })),
                        }
                    }
                }),
            )
            .route(
                "/wiki/api/v2/spaces/101/pages",
                get(|| async {
                    Json(json!({
                        "results": [
                            { "id": "1001", "spaceId": "101", "parentId": null, "title": "Eng Home", "status": "current", "version": { "number": 1 } }
                        ]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/spaces/202/pages",
                get(|| async {
                    Json(json!({
                        "results": [
                            { "id": "2001", "spaceId": "202", "parentId": null, "title": "Late Home", "status": "current", "version": { "number": 1 } }
                        ]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let wiki_url = format!("{base_url}/wiki");

        // Page-1 key resolves in exactly 1 spaces request.
        let eng_client = make_client(&base_url, &wiki_url, DeploymentKind::Cloud);
        let eng_pages = eng_client
            .list_space_root_pages_page("ENG", 50, None)
            .await
            .unwrap();
        assert_eq!(eng_pages.items.len(), 1);
        assert_eq!(eng_pages.items[0].id, "1001");
        assert_eq!(space_requests.load(Ordering::SeqCst), 1);

        // Page-2 key follows cursor and resolves successfully.
        space_requests.store(0, Ordering::SeqCst);
        let late_client = make_client(&base_url, &wiki_url, DeploymentKind::Cloud);
        let late_pages = late_client
            .list_space_root_pages_page("LATE", 50, None)
            .await
            .unwrap();
        assert_eq!(late_pages.items.len(), 1);
        assert_eq!(late_pages.items[0].id, "2001");
        assert_eq!(late_pages.items[0].space_key, "LATE");
        assert_eq!(space_requests.load(Ordering::SeqCst), 2);

        // Missing key returns the not-found error.
        let missing_err = late_client
            .list_space_root_pages_page("NOPE", 50, None)
            .await
            .unwrap_err();
        assert!(
            missing_err
                .to_string()
                .contains("Confluence space 'NOPE' not found."),
            "{missing_err}"
        );
    }

    #[tokio::test]
    async fn cloud_fetch_page_loads_subresources_and_ancestor_titles_concurrently() {
        use std::time::Duration;
        use tokio::sync::Barrier;

        let sub_barrier = Arc::new(Barrier::new(4));
        let anc_barrier = Arc::new(Barrier::new(3));
        let b_labels = sub_barrier.clone();
        let b_atts = sub_barrier.clone();
        let b_comments = sub_barrier.clone();
        let b_ancs = sub_barrier.clone();
        let a10 = anc_barrier.clone();
        let a20 = anc_barrier.clone();
        let a30 = anc_barrier.clone();

        let app = Router::new()
            .route(
                "/wiki/api/v2/pages/500",
                get(|| async {
                    Json(json!({
                        "id": "500",
                        "spaceId": "101",
                        "spaceKey": "ENG",
                        "parentId": "30",
                        "title": "Leaf",
                        "status": "current",
                        "version": { "number": 2 },
                        "body": {
                            "atlas_doc_format": {
                                "representation": "atlas_doc_format",
                                "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}]}]}"
                            }
                        }
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/500/labels",
                get(move || {
                    let b = b_labels.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "results": [{ "name": "runbook" }] }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/500/attachments",
                get(move || {
                    let b = b_atts.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "results": [] }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/500/footer-comments",
                get(move || {
                    let b = b_comments.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "results": [] }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/500/ancestors",
                get(move || {
                    let b = b_ancs.clone();
                    async move {
                        b.wait().await;
                        Json(json!({
                            "results": [
                                { "id": "10", "type": "page" },
                                { "id": "20", "type": "page" },
                                { "id": "30", "type": "page" }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/10",
                get(move || {
                    let b = a10.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "id": "10", "title": "Root" }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/20",
                get(move || {
                    let b = a20.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "id": "20", "title": "Middle" }))
                    }
                }),
            )
            .route(
                "/wiki/api/v2/pages/30",
                get(move || {
                    let b = a30.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "id": "30", "title": "Parent" }))
                    }
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, &format!("{base_url}/wiki"), DeploymentKind::Cloud);
        let (detail, meta) =
            tokio::time::timeout(Duration::from_secs(3), client.get_page_with_meta("500"))
                .await
                .expect("Cloud fetch_page must overlap subresources and ancestor title fetches")
                .unwrap();
        assert!(meta.labels_ok && meta.attachments_ok && meta.comments_ok && meta.ancestors_ok);
        assert_eq!(detail.labels, vec!["runbook".to_string()]);
        let titles: Vec<&str> = detail.ancestors.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["Root", "Middle", "Parent"]);
    }

    #[tokio::test]
    async fn dc_fetch_page_loads_content_and_comments_concurrently() {
        use std::time::Duration;
        use tokio::sync::Barrier;

        let barrier = Arc::new(Barrier::new(2));
        let b1 = barrier.clone();
        let b2 = barrier.clone();
        let app = Router::new()
            .route(
                "/rest/api/content/701",
                get(move || {
                    let b = b1.clone();
                    async move {
                        b.wait().await;
                        Json(json!({
                            "id": "701",
                            "type": "page",
                            "title": "DC Page",
                            "space": { "id": 1, "key": "ENG" },
                            "version": { "number": 3 },
                            "body": { "storage": { "value": "<p>DC body</p>" } },
                            "metadata": { "labels": { "results": [] } },
                            "children": { "attachment": { "results": [] } }
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/content/701/child/comment",
                get(move || {
                    let b = b2.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "results": [] }))
                    }
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, &base_url, DeploymentKind::DataCenter);
        let (detail, meta) =
            tokio::time::timeout(Duration::from_secs(3), client.get_page_with_meta("701"))
                .await
                .expect("DC fetch_page must overlap content and comments requests")
                .unwrap();
        assert_eq!(detail.summary.title, "DC Page");
        assert!(meta.comments_ok);
    }
}
