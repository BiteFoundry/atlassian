//! Jira Agile REST API v1 (`/rest/agile/1.0/board`, `/configuration`, `/sprint`, `/issue`).

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use futures::{stream, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::{
    atlassian::{
        client::{first_nonempty_str, json_id_to_string, offset_page_state, HttpStatusError},
        jira::normalize_issue_summary,
        AtlassianClient,
    },
    models::{BoardColumn, BoardSummary, IssueSummary, ListPage, SprintSummary, StatusCategory},
};

impl AtlassianClient {
    pub(crate) async fn get_agile_issue_fields(&self, key: &str, fields: &str) -> Result<Value> {
        let base = self.require_site_url()?;
        let key = urlencoding::encode(key);
        let fields = urlencoding::encode(fields);
        self.get_json(&format!(
            "{base}/rest/agile/1.0/issue/{key}?fields={fields}"
        ))
        .await
    }

    pub async fn move_issues_to_sprint(
        &self,
        sprint_id: i64,
        issues: &[String],
    ) -> Result<Vec<String>> {
        let keys: Vec<_> = issues.iter().map(|key| key.trim()).collect();
        let mut seen = HashSet::new();
        if sprint_id <= 0
            || !(1..=50).contains(&keys.len())
            || !keys
                .iter()
                .all(|key| !key.is_empty() && seen.insert(key.to_ascii_uppercase()))
        {
            bail!("provide a positive sprint ID and 1 to 50 distinct issue keys");
        }
        let base = self.require_site_url()?;
        let url = format!("{base}/rest/agile/1.0/sprint/{sprint_id}/issue");
        self.post_json_empty(&url, &json!({"issues": keys})).await?;
        const VERIFY_CONCURRENCY: usize = 6;
        let verified: Vec<Result<String>> = stream::iter(keys.iter().copied())
            .map(|key| async move {
                let issue = self
                    .get_agile_issue_fields(key, "sprint")
                    .await
                    .with_context(|| {
                        format!("Jira accepted the move to sprint {sprint_id}. Reading {key} failed")
                    })?;
                if issue.pointer("/fields/sprint/id").and_then(Value::as_i64) != Some(sprint_id) {
                    bail!("Jira accepted the move to sprint {sprint_id}. The read did not confirm membership for {key}");
                }
                Ok(issue
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_owned())
            })
            .buffered(VERIFY_CONCURRENCY)
            .collect()
            .await;
        let mut confirmed = Vec::with_capacity(keys.len());
        for res in verified {
            confirmed.push(res?);
        }
        Ok(confirmed)
    }

    pub async fn list_boards(&self, project_key: Option<&str>) -> Result<Vec<BoardSummary>> {
        let base = self.require_site_url()?;
        let encoded_project = project_key
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|pk| urlencoding::encode(pk).into_owned());
        let mut start = 0usize;
        let mut all = Vec::new();
        let mut seen = HashSet::new();

        loop {
            let mut url = format!("{base}/rest/agile/1.0/board?maxResults=50");
            if start > 0 {
                url.push_str(&format!("&startAt={start}"));
            }
            if let Some(ref pk) = encoded_project {
                url.push_str("&projectKeyOrId=");
                url.push_str(pk);
            }

            let val: Value = self.get_json(&url).await?;
            let arr = val
                .get("values")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let got = arr.len();
            let mut added = 0usize;

            for b in arr {
                let loc = b.get("location").unwrap_or(&Value::Null);
                let project_key = loc
                    .get("projectKey")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let project_name = loc
                    .get("projectName")
                    .or_else(|| loc.get("name"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);

                let board = BoardSummary {
                    id: b.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                    name: first_nonempty_str(b, &["name"]),
                    board_type: first_nonempty_str(b, &["type"]).to_ascii_lowercase(),
                    project_key,
                    project_name,
                    columns: Vec::new(),
                };
                if seen.insert(board.id) {
                    all.push(board);
                    added += 1;
                }
            }
            if added == 0 {
                break;
            }
            let (has_more, _) = offset_page_state(&val, start, got, 50);
            if has_more == Some(false) {
                break;
            }
            start += got;
        }

        Ok(all)
    }

    pub async fn get_board_columns(&self, board_id: i64) -> Result<Vec<BoardColumn>> {
        let base = self.require_site_url()?;
        let url = format!("{base}/rest/agile/1.0/board/{board_id}/configuration");

        let val: Value = match self.get_json(&url).await {
            Ok(v) => v,
            Err(err) => {
                let is_unavailable = err
                    .downcast_ref::<HttpStatusError>()
                    .map(|e| matches!(e.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND))
                    .unwrap_or(false);
                if is_unavailable {
                    return Ok(default_board_columns());
                }
                return Err(err);
            }
        };

        let cols = val
            .get("columnConfig")
            .and_then(|cc| cc.get("columns"))
            .and_then(|c| c.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        if cols.is_empty() {
            return Ok(default_board_columns());
        }

        Ok(cols
            .iter()
            .map(|col| {
                let name = first_nonempty_str(col, &["name"]);
                let statuses = col
                    .get("statuses")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let status_ids = statuses
                    .iter()
                    .map(|s| json_id_to_string(s.get("id")))
                    .filter(|id| !id.is_empty())
                    .collect();
                let status_names = statuses
                    .iter()
                    .filter_map(|s| {
                        s.get("name")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|n| !n.is_empty())
                            .map(str::to_string)
                    })
                    .collect();

                BoardColumn {
                    category_hint: map_column_name_to_category(&name),
                    name,
                    status_ids,
                    status_names,
                }
            })
            .collect())
    }

    pub async fn list_board_sprints_page(
        &self,
        board_id: i64,
        limit: usize,
        start: usize,
    ) -> Result<ListPage<SprintSummary>> {
        let base = self.require_site_url()?;
        let max_results = limit.clamp(1, 50);
        let mut url = format!(
            "{base}/rest/agile/1.0/board/{board_id}/sprint?state=active,future&maxResults={max_results}"
        );
        if start > 0 {
            url.push_str(&format!("&startAt={start}"));
        }

        let val: Value = match self.get_json(&url).await {
            Ok(v) => v,
            Err(err) => {
                let is_kanban_no_sprints = err
                    .downcast_ref::<HttpStatusError>()
                    .map(|e| {
                        e.status == StatusCode::BAD_REQUEST
                            && (e.body.to_ascii_lowercase().contains("does not support sprints")
                                || e.message.to_ascii_lowercase().contains("sprint")
                                || e.body.trim().is_empty())
                            || e.status == StatusCode::NOT_FOUND
                    })
                    .unwrap_or(false);
                if is_kanban_no_sprints {
                    return Ok(ListPage {
                        items: Vec::new(),
                        has_more: Some(false),
                        next_cursor: None,
                    });
                }
                return Err(err);
            }
        };

        let arr = val
            .get("values")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let items: Vec<SprintSummary> = arr
            .iter()
            .map(|s| SprintSummary {
                id: s.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                board_id,
                origin_board_id: s.get("originBoardId").and_then(|v| v.as_i64()),
                name: first_nonempty_str(s, &["name"]),
                state: first_nonempty_str(s, &["state"]).to_ascii_lowercase(),
                start_date: s
                    .get("startDate")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                end_date: s
                    .get("endDate")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                goal: s.get("goal").and_then(|v| v.as_str()).map(str::to_string),
            })
            .collect();
        let (has_more, next_cursor) = offset_page_state(&val, start, items.len(), max_results);
        Ok(ListPage {
            items,
            has_more,
            next_cursor,
        })
    }

    /// Every active and future sprint of the board (all pages, up to the sync cap).
    pub async fn list_board_sprints(&self, board_id: i64) -> Result<Vec<SprintSummary>> {
        Ok(self
            .list_all_board_sprints(board_id, super::collect::SYNC_SPRINT_CAP)
            .await?
            .items)
    }

    pub async fn list_board_issues_page(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        limit: usize,
        start: usize,
    ) -> Result<ListPage<IssueSummary>> {
        let base = self.require_site_url()?;
        let max_results = limit.clamp(1, 100);
        let mut url = match sprint_id {
            Some(sid) => {
                format!("{base}/rest/agile/1.0/sprint/{sid}/issue?maxResults={max_results}")
            }
            None => {
                format!("{base}/rest/agile/1.0/board/{board_id}/issue?maxResults={max_results}")
            }
        };
        if start > 0 {
            url.push_str(&format!("&startAt={start}"));
        }

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("issues")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let items: Vec<IssueSummary> = arr
            .iter()
            .map(|issue| normalize_issue_summary(issue, &base, self.deployment()))
            .collect();
        let (has_more, next_cursor) = offset_page_state(&val, start, items.len(), max_results);
        Ok(ListPage {
            items,
            has_more,
            next_cursor,
        })
    }

    pub async fn list_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        Ok(self
            .list_board_issues_page(board_id, sprint_id, limit, 0)
            .await?
            .items)
    }
}

fn map_column_name_to_category(name: &str) -> StatusCategory {
    let lower = name.trim().to_ascii_lowercase();
    if lower.contains("to do")
        || lower == "todo"
        || lower.contains("backlog")
        || lower == "open"
        || lower == "new"
        || lower.contains("할 일")
    {
        StatusCategory::Todo
    } else if lower.contains("done")
        || lower.contains("closed")
        || lower.contains("complete")
        || lower.contains("resolved")
        || lower.contains("완료")
    {
        StatusCategory::Done
    } else {
        StatusCategory::InProgress
    }
}

fn default_board_columns() -> Vec<BoardColumn> {
    vec![
        BoardColumn {
            name: "To Do".into(),
            status_ids: Vec::new(),
            status_names: vec!["To Do".into()],
            category_hint: StatusCategory::Todo,
        },
        BoardColumn {
            name: "In Progress".into(),
            status_ids: Vec::new(),
            status_names: vec!["In Progress".into()],
            category_hint: StatusCategory::InProgress,
        },
        BoardColumn {
            name: "Done".into(),
            status_ids: Vec::new(),
            status_names: vec!["Done".into()],
            category_hint: StatusCategory::Done,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{AuthManager, StoredCredentials},
        config::DeploymentKind,
    };
    use axum::{http::StatusCode as AxumStatus, routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_agile_boards_columns_kanban_sprint_fallback_and_issues() {
        let app = Router::new()
            .route(
                "/rest/agile/1.0/board",
                get(|| async {
                    Json(json!({
                        "values": [
                            { "id": 10, "name": "Scrum Board", "type": "scrum", "location": { "projectKey": "ENG", "projectName": "Engineering" } },
                            { "id": 20, "name": "Kanban Board", "type": "kanban", "location": { "projectKey": "OPS" } }
                        ]
                    }))
                }),
            )
            .route(
                "/rest/agile/1.0/board/10/configuration",
                get(|| async {
                    Json(json!({
                        "columnConfig": {
                            "columns": [
                                { "name": "Backlog", "statuses": [{ "id": "1" }] },
                                { "name": "Code Review", "statuses": [{ "id": "3" }] },
                                { "name": "Done", "statuses": [{ "id": "5" }] }
                            ]
                        }
                    }))
                }),
            )
            .route(
                "/rest/agile/1.0/board/20/configuration",
                get(|| async { AxumStatus::FORBIDDEN }),
            )
            .route(
                "/rest/agile/1.0/board/10/sprint",
                get(|| async {
                    Json(json!({
                        "values": [
                            { "id": 101, "originBoardId": 10, "name": "Sprint 1", "state": "active", "goal": "Ship API" }
                        ]
                    }))
                }),
            )
            .route(
                "/rest/agile/1.0/board/20/sprint",
                get(|| async {
                    (
                        AxumStatus::BAD_REQUEST,
                        Json(json!({ "errorMessages": ["The board does not support sprints"] })),
                    )
                }),
            )
            .route(
                "/rest/agile/1.0/sprint/101/issue",
                get(|| async {
                    Json(json!({
                        "issues": [{
                            "id": "55",
                            "key": "ENG-55",
                            "fields": {
                                "summary": "Sprint issue",
                                "status": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                                "issuetype": { "id": "1", "name": "Task", "subtask": false }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/rest/agile/1.0/board/20/issue",
                get(|| async {
                    Json(json!({
                        "issues": [{
                            "id": "88",
                            "key": "OPS-88",
                            "fields": {
                                "summary": "Kanban issue",
                                "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                                "issuetype": { "id": "1", "name": "Task", "subtask": false }
                            }
                        }]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let boards = client.list_boards(Some("ENG")).await.unwrap();
        assert_eq!(boards.len(), 2);
        assert_eq!(boards[0].board_type, "scrum");

        let cols10 = client.get_board_columns(10).await.unwrap();
        assert_eq!(cols10.len(), 3);
        assert_eq!(cols10[0].category_hint, StatusCategory::Todo);
        assert_eq!(cols10[1].category_hint, StatusCategory::InProgress);
        assert_eq!(cols10[2].category_hint, StatusCategory::Done);

        // 403 returns 3-column default
        let cols20 = client.get_board_columns(20).await.unwrap();
        assert_eq!(cols20.len(), 3);
        assert_eq!(cols20[0].name, "To Do");

        let sprints10 = client.list_board_sprints(10).await.unwrap();
        assert_eq!(sprints10.len(), 1);
        assert_eq!(sprints10[0].id, 101);

        // Kanban 400 returns empty Vec gracefully
        let sprints20 = client.list_board_sprints(20).await.unwrap();
        assert!(sprints20.is_empty());

        let sprint_issues = client.list_board_issues(10, Some(101), 50).await.unwrap();
        assert_eq!(sprint_issues[0].key, "ENG-55");

        let kanban_issues = client.list_board_issues(20, None, 50).await.unwrap();
        assert_eq!(kanban_issues[0].key, "OPS-88");
    }

    #[tokio::test]
    async fn board_issues_page_reports_has_more_and_cursor() {
        use axum::extract::Query;
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };

        let seen_max: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let sm = seen_max.clone();
        let all_issues: Vec<Value> = (1..=120)
            .map(|i| {
                json!({
                    "id": format!("{i}"),
                    "key": format!("ENG-{i}"),
                    "fields": {
                        "summary": format!("Issue {i}"),
                        "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                        "issuetype": { "id": "1", "name": "Task", "subtask": false }
                    }
                })
            })
            .collect();

        let app = Router::new()
            .route(
                "/rest/agile/1.0/board/7/issue",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let sm = sm.clone();
                    let all_issues = all_issues.clone();
                    async move {
                        let max = q
                            .get("maxResults")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(50);
                        let start = q
                            .get("startAt")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        sm.lock().unwrap().push(max);
                        let slice: Vec<Value> =
                            all_issues.iter().skip(start).take(max).cloned().collect();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": all_issues.len(),
                            "issues": slice
                        }))
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board/8/issue",
                get(|| async { Json(json!({ "total": 0, "issues": [] })) }),
            );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let p1 = client.list_board_issues_page(7, None, 500, 0).await.unwrap();
        assert_eq!(p1.items.len(), 100);
        assert_eq!(p1.has_more, Some(true));
        assert_eq!(p1.next_cursor.as_deref(), Some("100"));
        assert_eq!(seen_max.lock().unwrap()[0], 100);

        let p2 = client
            .list_board_issues_page(7, None, 500, 100)
            .await
            .unwrap();
        assert_eq!(p2.items.len(), 20);
        assert_eq!(p2.has_more, Some(false));
        assert_eq!(p2.next_cursor, None);

        let empty = client.list_board_issues_page(8, None, 50, 0).await.unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.has_more, Some(false));
        assert_eq!(empty.next_cursor, None);
    }

    #[tokio::test]
    async fn list_boards_and_sprints_follow_is_last() {
        use axum::extract::Query;
        use std::{
            collections::HashMap,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
        };

        let board_calls = Arc::new(AtomicUsize::new(0));
        let bc = board_calls.clone();
        let all_boards: Vec<Value> = (1..=120)
            .map(|i| {
                json!({
                    "id": i,
                    "name": format!("Board {i}"),
                    "type": "scrum",
                    "location": { "projectKey": "ENG", "projectName": "Engineering" }
                })
            })
            .collect();
        let all_sprints: Vec<Value> = (1..=60)
            .map(|i| {
                json!({
                    "id": i,
                    "originBoardId": 1,
                    "name": format!("Sprint {i}"),
                    "state": if i == 1 { "active" } else { "future" }
                })
            })
            .collect();

        let app = Router::new()
            .route(
                "/rest/agile/1.0/board",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let bc = bc.clone();
                    let all_boards = all_boards.clone();
                    async move {
                        bc.fetch_add(1, Ordering::SeqCst);
                        let start = q
                            .get("startAt")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let max = q
                            .get("maxResults")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(50);
                        let slice: Vec<Value> =
                            all_boards.iter().skip(start).take(max).cloned().collect();
                        let is_last = start + slice.len() >= all_boards.len();
                        Json(json!({ "isLast": is_last, "values": slice }))
                    }
                }),
            )
            .route(
                "/rest/agile/1.0/board/1/sprint",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let all_sprints = all_sprints.clone();
                    async move {
                        let start = q
                            .get("startAt")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let max = q
                            .get("maxResults")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(50);
                        let slice: Vec<Value> =
                            all_sprints.iter().skip(start).take(max).cloned().collect();
                        let is_last = start + slice.len() >= all_sprints.len();
                        Json(json!({ "isLast": is_last, "values": slice }))
                    }
                }),
            );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let boards = client.list_boards(None).await.unwrap();
        assert_eq!(boards.len(), 120);
        assert_eq!(board_calls.load(Ordering::SeqCst), 3);

        let sp1 = client.list_board_sprints_page(1, 50, 0).await.unwrap();
        assert_eq!(sp1.items.len(), 50);
        assert_eq!(sp1.has_more, Some(true));
        assert_eq!(sp1.next_cursor.as_deref(), Some("50"));

        let sp2 = client.list_board_sprints_page(1, 50, 50).await.unwrap();
        assert_eq!(sp2.items.len(), 10);
        assert_eq!(sp2.has_more, Some(false));
        assert_eq!(sp2.next_cursor, None);
    }

    #[tokio::test]
    async fn sprint_board_id_is_queried_board_and_origin_is_kept() {
        let app = Router::new().route(
            "/rest/agile/1.0/board/20/sprint",
            get(|| async {
                Json(json!({
                    "isLast": true,
                    "values": [
                        { "id": 7, "name": "Shared sprint", "state": "ACTIVE", "originBoardId": 10 },
                        { "id": 8, "name": "Own sprint", "state": "future", "originBoardId": 20 },
                        { "id": 9, "name": "No origin", "state": "future" }
                    ]
                }))
            }),
        );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let page = client.list_board_sprints_page(20, 50, 0).await.unwrap();
        assert_eq!(page.items.len(), 3);
        // Every sprint is reported under the board that was queried, shared or not.
        assert!(page.items.iter().all(|s| s.board_id == 20));
        assert_eq!(page.items[0].id, 7);
        assert_eq!(page.items[0].origin_board_id, Some(10));
        assert_eq!(page.items[0].state, "active");
        assert_eq!(page.items[1].origin_board_id, Some(20));
        assert_eq!(page.items[2].origin_board_id, None);
    }

    #[tokio::test]
    async fn move_issues_to_sprint_verifies_memberships_concurrently_in_order() {
        use std::{sync::Arc, time::Duration};
        use axum::{extract::Path, http::StatusCode, routing::post};
        use tokio::sync::Barrier;

        let barrier = Arc::new(Barrier::new(3));
        let b = barrier.clone();
        let app = Router::new()
            .route(
                "/rest/agile/1.0/sprint/42/issue",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/rest/agile/1.0/issue/{key}",
                get(move |Path(key): Path<String>| {
                    let b = b.clone();
                    async move {
                        b.wait().await;
                        Json(json!({ "key": key, "fields": { "sprint": { "id": 42 } } }))
                    }
                }),
            );

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "sprint-par",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let keys = vec!["ENG-1".into(), "ENG-2".into(), "ENG-3".into()];
        let confirmed = tokio::time::timeout(
            Duration::from_secs(3),
            client.move_issues_to_sprint(42, &keys),
        )
        .await
        .expect("move_issues_to_sprint must verify issues concurrently")
        .unwrap();
        assert_eq!(confirmed, keys);
    }
}
