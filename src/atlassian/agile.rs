//! Jira Agile REST API v1 (`/rest/agile/1.0/board`, `/configuration`, `/sprint`, `/issue`).

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::{
    atlassian::{
        client::{first_nonempty_str, json_id_to_string, HttpStatusError},
        jira::normalize_issue_summary,
        AtlassianClient,
    },
    models::{BoardColumn, BoardSummary, IssueSummary, SprintSummary, StatusCategory},
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
        let mut confirmed = Vec::with_capacity(keys.len());
        for key in keys {
            let issue = self
                .get_agile_issue_fields(key, "sprint")
                .await
                .with_context(|| {
                    format!("Jira accepted the move to sprint {sprint_id}. Reading {key} failed")
                })?;
            if issue.pointer("/fields/sprint/id").and_then(Value::as_i64) != Some(sprint_id) {
                bail!("Jira accepted the move to sprint {sprint_id}. The read did not confirm membership for {key}");
            }
            confirmed.push(
                issue
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_owned(),
            );
        }
        Ok(confirmed)
    }

    pub async fn list_boards(&self, project_key: Option<&str>) -> Result<Vec<BoardSummary>> {
        let base = self.require_site_url()?;
        let mut url = format!("{base}/rest/agile/1.0/board?maxResults=50");
        if let Some(pk) = project_key.map(str::trim).filter(|s| !s.is_empty()) {
            url.push_str("&projectKeyOrId=");
            url.push_str(&urlencoding::encode(pk));
        }

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("values")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(arr
            .iter()
            .map(|b| {
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

                BoardSummary {
                    id: b.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                    name: first_nonempty_str(b, &["name"]),
                    board_type: first_nonempty_str(b, &["type"]).to_ascii_lowercase(),
                    project_key,
                    project_name,
                    columns: Vec::new(),
                }
            })
            .collect())
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

    pub async fn list_board_sprints(&self, board_id: i64) -> Result<Vec<SprintSummary>> {
        let base = self.require_site_url()?;
        let url = format!("{base}/rest/agile/1.0/board/{board_id}/sprint?state=active,future");

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
                    return Ok(Vec::new());
                }
                return Err(err);
            }
        };

        let arr = val
            .get("values")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(arr
            .iter()
            .map(|s| SprintSummary {
                id: s.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
                board_id: s
                    .get("originBoardId")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(board_id),
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
            .collect())
    }

    pub async fn list_board_issues(
        &self,
        board_id: i64,
        sprint_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<IssueSummary>> {
        let base = self.require_site_url()?;
        let max_results = limit.clamp(1, 100);
        let url = match sprint_id {
            Some(sid) => {
                format!("{base}/rest/agile/1.0/sprint/{sid}/issue?maxResults={max_results}")
            }
            None => {
                format!("{base}/rest/agile/1.0/board/{board_id}/issue?maxResults={max_results}")
            }
        };

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("issues")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(arr
            .iter()
            .map(|issue| normalize_issue_summary(issue, &base, self.deployment()))
            .collect())
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
}
