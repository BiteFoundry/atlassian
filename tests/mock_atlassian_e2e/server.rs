//! Full in-process mock Atlassian Cloud + Data Center E2E test suite.
//!
//! Covers:
//! 1. Cloud Jira + Agile REST v1 + `POST /rest/api/3/search/jql` (`nextPageToken`) + ADF + attachments + SQLite `issues_fts` (Korean 2-char search).
//! 2. Cloud Confluence v2 + stringified ADF + TOC + `PROJ-101` cross-ref + `version.number` increment + `pages_fts` + `search_unified`.
//! 3. Data Center PAT (`Bearer`) compatibility across `/rest/api/2/myself`, `/rest/api/2/search` (`startAt`), `/rest/api/2/issue` (Wiki Markup), `/rest/api/space` + `/rest/api/content` (Storage XHTML `<ac:structured-macro>`).
//! 4. Local HTTP API (`serve::build_router`), stdio MCP (`mcp::handle_rpc_line` including `confirm: true` guard), inline terminal image preview (`img_preview`), and self-updater (`updater::check_or_apply_update_at`).

use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once, OnceLock, RwLock,
    },
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use image::{ImageBuffer, ImageFormat, Rgba, RgbaImage};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::net::TcpListener;

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: OnceLock<TempDir> = OnceLock::new();

pub(crate) fn ensure_isolated_env() {
    INIT_TEST_ENV.call_once(|| {
        let dir = tempfile::tempdir().expect("create shared test config tempdir");
        // Safety: initialized once at test start before any config/auth reads.
        std::env::set_var("ATLASSIAN_DISABLE_KEYRING", "1");
        std::env::set_var("ATLASSIAN_CONFIG_DIR", dir.path());
        std::env::remove_var("ATLASSIAN_SITE_URL");
        std::env::remove_var("ATLASSIAN_CONFLUENCE_URL");
        std::env::remove_var("ATLASSIAN_EMAIL");
        std::env::remove_var("ATLASSIAN_API_TOKEN");
        std::env::remove_var("ATLASSIAN_PAT");
        std::env::remove_var("ATLASSIAN_DEPLOYMENT");
        let _ = TEST_CONFIG_DIR.set(dir);
    });
}

pub(crate) fn generate_test_png_bytes() -> Vec<u8> {
    let mut img: RgbaImage = ImageBuffer::new(16, 16);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let r = if x < 8 { 220 } else { 40 };
        let g = if y < 8 { 180 } else { 90 };
        let b = ((x + y) * 8) as u8;
        *pixel = Rgba([r, g, b, 255]);
    }
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png)
        .expect("encode PNG bytes");
    out.into_inner()
}

pub(crate) fn check_basic_auth(
    headers: &HeaderMap,
    expected_email: &str,
    expected_token: &str,
) -> Result<(), Box<Response>> {
    let expected_raw = format!("{expected_email}:{expected_token}");
    let expected_header = format!("Basic {}", BASE64_STANDARD.encode(expected_raw.as_bytes()));
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if actual == Some(expected_header.as_str()) {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "errorMessages": ["Invalid Cloud Basic credentials"] })),
            )
                .into_response(),
        ))
    }
}

pub(crate) fn check_bearer_auth(headers: &HeaderMap, expected_pat: &str) -> Result<(), Box<Response>> {
    let expected_header = format!("Bearer {expected_pat}");
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if actual == Some(expected_header.as_str()) {
        Ok(())
    } else {
        Err(Box::new(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "errorMessages": ["Invalid Data Center Bearer PAT"] })),
            )
                .into_response(),
        ))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MockAttachment {
    pub(crate) id: String,
    pub(crate) filename: String,
    pub(crate) mime_type: String,
    pub(crate) bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct MockIssueRecord {
    pub(crate) id: String,
    pub(crate) key: String,
    pub(crate) project_key: String,
    pub(crate) summary: String,
    pub(crate) status_id: String,
    pub(crate) status_name: String,
    pub(crate) status_category_key: String,
    pub(crate) issue_type: String,
    pub(crate) priority: String,
    pub(crate) assignee_id: Option<String>,
    pub(crate) assignee_name: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) description_cloud_adf: Value,
    pub(crate) description_dc_wiki: String,
    pub(crate) comments: Vec<Value>,
    pub(crate) attachments: Vec<MockAttachment>,
    pub(crate) subtasks: Vec<Value>,
    pub(crate) remote_links: Vec<Value>,
    pub(crate) updated: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MockPageRecord {
    pub(crate) id: String,
    pub(crate) space_id: String,
    pub(crate) space_key: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) title: String,
    pub(crate) version: i32,
    pub(crate) adf_str: String,
    pub(crate) storage_xhtml: String,
    pub(crate) labels: Vec<String>,
    pub(crate) comments: Vec<Value>,
    pub(crate) updated_at: String,
}

pub(crate) struct MockAtlassianState {
    pub(crate) cloud_email: String,
    pub(crate) cloud_token: String,
    pub(crate) dc_pat: String,
    pub(crate) base_url: RwLock<String>,
    pub(crate) seq: RwLock<u64>,
    pub(crate) issues: RwLock<HashMap<String, MockIssueRecord>>,
    pub(crate) pages: RwLock<HashMap<String, MockPageRecord>>,
    pub(crate) attachments: RwLock<HashMap<String, MockAttachment>>,
    pub(crate) captured_cloud_jql_requests: RwLock<Vec<Value>>,
    pub(crate) captured_cloud_create_issue_bodies: RwLock<Vec<Value>>,
    pub(crate) captured_cloud_create_page_bodies: RwLock<Vec<Value>>,
    pub(crate) captured_dc_search_requests: RwLock<Vec<Value>>,
    pub(crate) captured_dc_create_issue_bodies: RwLock<Vec<Value>>,
    pub(crate) captured_dc_create_page_bodies: RwLock<Vec<Value>>,
    pub(crate) remotelink_requests: AtomicUsize,
}

impl MockAtlassianState {
    pub(crate) fn new(cloud_email: &str, cloud_token: &str, dc_pat: &str) -> Arc<Self> {
        let mut issues = HashMap::new();
        issues.insert(
            "PROJ-101".to_string(),
            MockIssueRecord {
                id: "10101".to_string(),
                key: "PROJ-101".to_string(),
                project_key: "PROJ".to_string(),
                summary: "배포 파이프라인 자동화 점검".to_string(),
                status_id: "11".to_string(),
                status_name: "In Progress".to_string(),
                status_category_key: "indeterminate".to_string(),
                issue_type: "Story".to_string(),
                priority: "High".to_string(),
                assignee_id: Some("acc-alice".to_string()),
                assignee_name: Some("Alice Kim".to_string()),
                labels: vec!["deploy".to_string(), "release".to_string()],
                description_cloud_adf: json!({
                    "type": "doc",
                    "version": 1,
                    "content": [
                        {
                            "type": "heading",
                            "attrs": { "level": 1 },
                            "content": [{ "type": "text", "text": "배포 점검 목록" }]
                        },
                        {
                            "type": "paragraph",
                            "content": [{ "type": "text", "text": "프로덕션 배포 전 검증을 수행합니다." }]
                        }
                    ]
                }),
                description_dc_wiki: "h1. 배포 점검 목록\n프로덕션 배포 전 검증을 수행합니다.".to_string(),
                comments: vec![],
                attachments: vec![],
                subtasks: vec![json!({
                    "id": "10102",
                    "key": "PROJ-102",
                    "fields": {
                        "summary": "배포 체크리스트 서명",
                        "status": {
                            "id": "10",
                            "name": "To Do",
                            "statusCategory": { "key": "new" }
                        },
                        "issuetype": { "id": "10003", "name": "Sub-task", "subtask": true },
                        "assignee": { "accountId": "acc-alice", "displayName": "Alice Kim" }
                    }
                })],
                remote_links: vec![json!({
                    "id": 9001,
                    "globalId": "appId=conf-1&pageId=2001",
                    "application": {
                        "type": "com.atlassian.confluence",
                        "name": "System Confluence"
                    },
                    "object": {
                        "url": "https://acme.atlassian.net/wiki/spaces/ENG/pages/2001/Release+Guide",
                        "title": "Release Guide"
                    }
                })],
                updated: "2026-11-15T10:00:00.000+0000".to_string(),
            },
        );
        issues.insert(
            "PROJ-103".to_string(),
            MockIssueRecord {
                id: "10103".to_string(),
                key: "PROJ-103".to_string(),
                project_key: "PROJ".to_string(),
                summary: "모바일 푸시 알림 지연 조사".to_string(),
                status_id: "10".to_string(),
                status_name: "To Do".to_string(),
                status_category_key: "new".to_string(),
                issue_type: "Bug".to_string(),
                priority: "Medium".to_string(),
                assignee_id: None,
                assignee_name: None,
                labels: vec!["mobile".to_string()],
                description_cloud_adf: json!({
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "iOS/Android 알림 지연 로그 확인" }]
                    }]
                }),
                description_dc_wiki: "iOS/Android 알림 지연 로그 확인".to_string(),
                comments: vec![],
                attachments: vec![],
                subtasks: vec![],
                remote_links: vec![],
                updated: "2026-11-15T09:00:00.000+0000".to_string(),
            },
        );

        let mut pages = HashMap::new();
        let root_adf = json!({
            "type": "doc",
            "version": 1,
            "content": [
                {
                    "type": "heading",
                    "attrs": { "level": 1 },
                    "content": [{ "type": "text", "text": "배포 아키텍처 개요" }]
                },
                {
                    "type": "paragraph",
                    "content": [{ "type": "text", "text": "관련 티켓: PROJ-101 배포 파이프라인을 참고하세요." }]
                },
                {
                    "type": "heading",
                    "attrs": { "level": 2 },
                    "content": [{ "type": "text", "text": "체크리스트" }]
                }
            ]
        });
        pages.insert(
            "2001".to_string(),
            MockPageRecord {
                id: "2001".to_string(),
                space_id: "5001".to_string(),
                space_key: "ENG".to_string(),
                parent_id: None,
                title: "엔지니어링 배포 가이드".to_string(),
                version: 1,
                adf_str: serde_json::to_string(&root_adf).unwrap(),
                storage_xhtml: r#"<h1>배포 아키텍처 개요</h1><p>관련 티켓: PROJ-101</p><ac:structured-macro ac:name="info"><ac:parameter ac:name="title">알림</ac:parameter><ac:rich-text-body><p>Data Center 배포 안내</p></ac:rich-text-body></ac:structured-macro>"#.to_string(),
                labels: vec!["deploy".to_string(), "runbook".to_string()],
                comments: vec![],
                updated_at: "2026-11-15T10:30:00.000Z".to_string(),
            },
        );
        pages.insert(
            "2002".to_string(),
            MockPageRecord {
                id: "2002".to_string(),
                space_id: "5001".to_string(),
                space_key: "ENG".to_string(),
                parent_id: Some("2001".to_string()),
                title: "롤백 절차".to_string(),
                version: 1,
                adf_str: serde_json::to_string(&json!({
                    "type": "doc",
                    "version": 1,
                    "content": [{
                        "type": "paragraph",
                        "content": [{ "type": "text", "text": "문제 발생 시 즉시 이전 버전으로 롤백합니다." }]
                    }]
                })).unwrap(),
                storage_xhtml: "<p>문제 발생 시 즉시 이전 버전으로 롤백합니다.</p>".to_string(),
                labels: vec!["rollback".to_string()],
                comments: vec![],
                updated_at: "2026-11-15T10:35:00.000Z".to_string(),
            },
        );

        Arc::new(Self {
            cloud_email: cloud_email.to_string(),
            cloud_token: cloud_token.to_string(),
            dc_pat: dc_pat.to_string(),
            base_url: RwLock::new(String::new()),
            seq: RwLock::new(300),
            issues: RwLock::new(issues),
            pages: RwLock::new(pages),
            attachments: RwLock::new(HashMap::new()),
            captured_cloud_jql_requests: RwLock::new(Vec::new()),
            captured_cloud_create_issue_bodies: RwLock::new(Vec::new()),
            captured_cloud_create_page_bodies: RwLock::new(Vec::new()),
            captured_dc_search_requests: RwLock::new(Vec::new()),
            captured_dc_create_issue_bodies: RwLock::new(Vec::new()),
            captured_dc_create_page_bodies: RwLock::new(Vec::new()),
            remotelink_requests: AtomicUsize::new(0),
        })
    }

    pub(crate) fn next_num(&self) -> u64 {
        let mut s = self.seq.write().unwrap();
        *s += 1;
        *s
    }

    pub(crate) fn issue_to_jira_json(&self, rec: &MockIssueRecord, is_cloud: bool) -> Value {
        let base = self.base_url.read().unwrap().clone();
        let assignee = rec.assignee_id.as_ref().map(|aid| {
            let dname = rec.assignee_name.as_deref().unwrap_or(aid.as_str());
            if is_cloud {
                json!({ "accountId": aid, "displayName": dname, "active": true })
            } else {
                json!({ "name": aid, "key": aid, "displayName": dname, "active": true })
            }
        });
        let attachments_json: Vec<Value> = rec
            .attachments
            .iter()
            .map(|att| {
                json!({
                    "id": att.id,
                    "filename": att.filename,
                    "mimeType": att.mime_type,
                    "size": att.bytes.len(),
                    "created": "2026-11-15T11:00:00.000+0000",
                    "author": { "displayName": "Alice Kim" },
                    "content": format!("{base}/rest/api/3/attachment/content/{}", att.id)
                })
            })
            .collect();
        let desc_val = if is_cloud {
            rec.description_cloud_adf.clone()
        } else {
            Value::String(rec.description_dc_wiki.clone())
        };
        json!({
            "id": rec.id,
            "key": rec.key,
            "fields": {
                "project": { "key": rec.project_key, "name": "Main Project" },
                "summary": rec.summary,
                "description": desc_val,
                "status": {
                    "id": rec.status_id,
                    "name": rec.status_name,
                    "statusCategory": { "key": rec.status_category_key }
                },
                "priority": { "id": "2", "name": rec.priority },
                "issuetype": { "id": "10001", "name": rec.issue_type, "subtask": false },
                "assignee": assignee,
                "reporter": { "accountId": "acc-alice", "displayName": "Alice Kim", "active": true },
                "labels": rec.labels,
                "customfield_10016": 5.0,
                "customfield_10020": [{ "id": 101, "name": "Sprint 42", "state": "active" }],
                "subtasks": rec.subtasks,
                "attachment": attachments_json,
                "comment": {
                    "total": rec.comments.len(),
                    "comments": rec.comments
                },
                "created": "2026-11-15T08:00:00.000+0000",
                "updated": rec.updated
            }
        })
    }

    pub(crate) fn page_to_cloud_v2_json(&self, rec: &MockPageRecord, include_body: bool) -> Value {
        let mut obj = json!({
            "id": rec.id,
            "spaceId": rec.space_id,
            "spaceKey": rec.space_key,
            "parentId": rec.parent_id,
            "status": "current",
            "title": rec.title,
            "position": 1,
            "authorId": "acc-alice",
            "createdAt": "2026-11-15T08:00:00.000Z",
            "version": {
                "number": rec.version,
                "createdAt": rec.updated_at,
                "authorId": "acc-alice"
            },
            "_links": {
                "webui": format!("/spaces/{}/pages/{}", rec.space_key, rec.id)
            }
        });
        if include_body {
            obj["body"] = json!({
                "atlas_doc_format": {
                    "representation": "atlas_doc_format",
                    "value": rec.adf_str
                }
            });
        }
        obj
    }

    pub(crate) fn page_to_dc_v1_json(&self, rec: &MockPageRecord, include_body: bool) -> Value {
        let mut obj = json!({
            "id": rec.id,
            "type": "page",
            "status": "current",
            "title": rec.title,
            "space": { "id": 5001, "key": rec.space_key, "name": "Engineering" },
            "version": {
                "number": rec.version,
                "when": rec.updated_at,
                "by": { "displayName": "Alice Kim" }
            },
            "ancestors": rec.parent_id.as_ref().map(|pid| vec![json!({ "id": pid, "title": "Parent" })]).unwrap_or_default(),
            "metadata": {
                "labels": {
                    "results": rec.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>()
                }
            },
            "children": {
                "attachment": { "results": [] }
            },
            "_links": {
                "webui": format!("/display/{}/{}", rec.space_key, rec.id)
            }
        });
        if include_body {
            obj["body"] = json!({
                "storage": {
                    "representation": "storage",
                    "value": rec.storage_xhtml
                }
            });
        }
        obj
    }
}

fn extract_multipart_file(body: &[u8]) -> Option<(String, Vec<u8>)> {
    let text_prefix = String::from_utf8_lossy(&body[..body.len().min(2048)]);
    let fname_idx = text_prefix.find("filename=\"")?;
    let after_fname = &text_prefix[fname_idx + 10..];
    let end_quote = after_fname.find('"')?;
    let filename = after_fname[..end_quote].to_string();

    let header_end = body
        .windows(4)
        .position(|w| w == b"\r\n\r\n")?;
    let content_start = header_end + 4;
    let rest = &body[content_start..];
    // Find trailing "\r\n--" boundary marker
    let content_end = rest
        .windows(4)
        .rposition(|w| w == b"\r\n--")
        .unwrap_or(rest.len());
    Some((filename, rest[..content_end].to_vec()))
}

pub(crate) fn build_mock_atlassian_router(state: Arc<MockAtlassianState>) -> Router {
    Router::new()
        // ---------------- Cloud Jira v3 + Agile v1 ----------------
        .route(
            "/rest/api/3/myself",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "accountId": "acc-alice",
                    "displayName": "Alice Kim",
                    "emailAddress": st.cloud_email,
                    "active": true
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/3/project/search",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "values": [{
                        "id": "10000",
                        "key": "PROJ",
                        "name": "Main Project",
                        "projectTypeKey": "software",
                        "lead": { "displayName": "Alice Kim" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "values": [{
                        "id": 11,
                        "name": "PROJ Kanban & Sprint Board",
                        "type": "scrum",
                        "location": { "projectKey": "PROJ", "projectName": "Main Project" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/configuration",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "columnConfig": {
                        "columns": [
                            { "name": "To Do", "statuses": [{ "id": "10", "name": "To Do" }] },
                            { "name": "In Progress", "statuses": [{ "id": "11", "name": "In Progress" }] },
                            { "name": "Done", "statuses": [{ "id": "12", "name": "Done" }] }
                        ]
                    }
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/sprint",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "values": [{
                        "id": 101,
                        "originBoardId": 11,
                        "name": "Sprint 42",
                        "state": "active",
                        "goal": "Release v0.1.0"
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/sprint/{sprint_id}/issue",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.issues.read().unwrap();
                let mut list: Vec<Value> = guard
                    .values()
                    .map(|rec| st.issue_to_jira_json(rec, true))
                    .collect();
                list.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
                Json(json!({ "issues": list })).into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/issue",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.issues.read().unwrap();
                let list: Vec<Value> = guard
                    .values()
                    .map(|rec| st.issue_to_jira_json(rec, true))
                    .collect();
                Json(json!({ "issues": list })).into_response()
            }),
        )
        // Legacy v3 search MUST NOT be used (410 Gone)
        .route(
            "/rest/api/3/search",
            post(|| async { (StatusCode::GONE, "410 Gone: use /rest/api/3/search/jql") }),
        )
        .route(
            "/rest/api/3/search/jql",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.captured_cloud_jql_requests.write().unwrap().push(body.clone());
                let guard = st.issues.read().unwrap();
                let mut sorted_keys: Vec<String> = guard.keys().cloned().collect();
                sorted_keys.sort();
                let page_tok = body.get("nextPageToken").and_then(|v| v.as_str());
                if page_tok.is_none() {
                    let first = sorted_keys.first().and_then(|k| guard.get(k));
                    let issues: Vec<Value> = first
                        .into_iter()
                        .map(|r| st.issue_to_jira_json(r, true))
                        .collect();
                    Json(json!({
                        "issues": issues,
                        "nextPageToken": "page-2-token",
                        "total": sorted_keys.len()
                    }))
                    .into_response()
                } else {
                    let rest: Vec<Value> = sorted_keys
                        .iter()
                        .skip(1)
                        .filter_map(|k| guard.get(k))
                        .map(|r| st.issue_to_jira_json(r, true))
                        .collect();
                    Json(json!({
                        "issues": rest,
                        "total": sorted_keys.len()
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/rest/api/3/issue",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.captured_cloud_create_issue_bodies.write().unwrap().push(body.clone());
                let fields = body.get("fields").cloned().unwrap_or(Value::Null);
                let project_key = fields
                    .pointer("/project/key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("PROJ")
                    .to_string();
                let num = st.next_num();
                let key = format!("{project_key}-{num}");
                let rec = MockIssueRecord {
                    id: format!("10{num}"),
                    key: key.clone(),
                    project_key,
                    summary: fields.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    status_id: "10".to_string(),
                    status_name: "To Do".to_string(),
                    status_category_key: "new".to_string(),
                    issue_type: fields.pointer("/issuetype/name").and_then(|v| v.as_str()).unwrap_or("Task").to_string(),
                    priority: fields.pointer("/priority/name").and_then(|v| v.as_str()).unwrap_or("Medium").to_string(),
                    assignee_id: fields.pointer("/assignee/id").and_then(|v| v.as_str()).map(str::to_string),
                    assignee_name: None,
                    labels: fields
                        .get("labels")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                    description_cloud_adf: fields.get("description").cloned().unwrap_or(Value::Null),
                    description_dc_wiki: String::new(),
                    comments: vec![],
                    attachments: vec![],
                    subtasks: vec![],
                    remote_links: vec![],
                    updated: "2026-11-15T11:30:00.000+0000".to_string(),
                };
                let id = rec.id.clone();
                st.issues.write().unwrap().insert(key.clone(), rec);
                (StatusCode::CREATED, Json(json!({ "id": id, "key": key }))).into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.issues.read().unwrap();
                match guard.get(&key) {
                    Some(rec) => Json(st.issue_to_jira_json(rec, true)).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({ "errorMessages": ["Issue not found"] }))).into_response(),
                }
            })
            .delete(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.issues.write().unwrap().remove(&key);
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/remotelink",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.remotelink_requests.fetch_add(1, Ordering::SeqCst);
                let guard = st.issues.read().unwrap();
                let links = guard
                    .get(&key)
                    .map(|r| r.remote_links.clone())
                    .unwrap_or_default();
                Json(Value::Array(links)).into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/transitions",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "transitions": [
                        { "id": "21", "name": "Start Progress", "to": { "id": "11", "name": "In Progress", "statusCategory": { "key": "indeterminate" } } },
                        { "id": "31", "name": "Done", "to": { "id": "12", "name": "Done", "statusCategory": { "key": "done" } } }
                    ]
                }))
                .into_response()
            })
            .post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let tid = body.pointer("/transition/id").and_then(|v| v.as_str()).unwrap_or("");
                let mut guard = st.issues.write().unwrap();
                if let Some(rec) = guard.get_mut(&key) {
                    if tid == "31" {
                        rec.status_id = "12".into();
                        rec.status_name = "Done".into();
                        rec.status_category_key = "done".into();
                    } else if tid == "21" {
                        rec.status_id = "11".into();
                        rec.status_name = "In Progress".into();
                        rec.status_category_key = "indeterminate".into();
                    }
                }
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/assignee",
            put(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let aid = body.get("accountId").and_then(|v| v.as_str()).map(str::to_string);
                let mut guard = st.issues.write().unwrap();
                if let Some(rec) = guard.get_mut(&key) {
                    rec.assignee_name = aid.as_ref().map(|_| "Alice Kim".to_string());
                    rec.assignee_id = aid;
                }
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/comment",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let cid = format!("c-{}", st.next_num());
                let comment_json = json!({
                    "id": cid,
                    "author": { "accountId": "acc-alice", "displayName": "Alice Kim", "active": true },
                    "body": body.get("body").cloned().unwrap_or(Value::Null),
                    "created": "2026-11-15T12:00:00.000+0000",
                    "updated": "2026-11-15T12:00:00.000+0000"
                });
                if let Some(rec) = st.issues.write().unwrap().get_mut(&key) {
                    rec.comments.push(comment_json.clone());
                }
                (StatusCode::CREATED, Json(comment_json)).into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/attachments",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>, body: Bytes| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let xsrf = headers
                    .get("X-Atlassian-Token")
                    .and_then(|v| v.to_str().ok());
                if xsrf != Some("no-check") {
                    return (StatusCode::FORBIDDEN, "Missing X-Atlassian-Token: no-check").into_response();
                }
                let (filename, file_bytes) = extract_multipart_file(&body)
                    .unwrap_or_else(|| ("upload.bin".to_string(), body.to_vec()));
                let aid = format!("att-{}", st.next_num());
                let mime = if filename.ends_with(".png") {
                    "image/png"
                } else {
                    "application/octet-stream"
                };
                let att = MockAttachment {
                    id: aid.clone(),
                    filename: filename.clone(),
                    mime_type: mime.to_string(),
                    bytes: file_bytes.clone(),
                };
                let base = st.base_url.read().unwrap().clone();
                let resp_json = json!([{
                    "id": aid,
                    "filename": filename,
                    "mimeType": mime,
                    "size": file_bytes.len(),
                    "created": "2026-11-15T12:10:00.000+0000",
                    "author": { "displayName": "Alice Kim" },
                    "content": format!("{base}/rest/api/3/attachment/content/{aid}")
                }]);
                st.attachments.write().unwrap().insert(aid, att.clone());
                if let Some(rec) = st.issues.write().unwrap().get_mut(&key) {
                    rec.attachments.push(att);
                }
                Json(resp_json).into_response()
            }),
        )
        .route(
            "/rest/api/3/attachment/content/{id}",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.attachments.read().unwrap();
                match guard.get(&id) {
                    Some(att) => ([(header::CONTENT_TYPE, att.mime_type.clone())], att.bytes.clone()).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        // ---------------- Cloud Confluence v2 + CQL ----------------
        .route(
            "/wiki/api/v2/spaces",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({
                    "results": [{
                        "id": "5001",
                        "key": "ENG",
                        "name": "Engineering",
                        "type": "global",
                        "homepageId": "2001",
                        "description": { "plain": { "value": "Engineering docs" } },
                        "_links": { "webui": "/spaces/ENG/overview" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/wiki/api/v2/spaces/{space_id}/pages",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(space_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                let mut results: Vec<Value> = guard
                    .values()
                    .filter(|p| p.space_id == space_id && p.parent_id.is_none())
                    .map(|p| st.page_to_cloud_v2_json(p, false))
                    .collect();
                results.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                Json(json!({ "results": results })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.captured_cloud_create_page_bodies.write().unwrap().push(body.clone());
                let pid = format!("{}", 2000 + st.next_num());
                let adf_str = body.pointer("/body/value").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let rec = MockPageRecord {
                    id: pid.clone(),
                    space_id: body.get("spaceId").and_then(|v| v.as_str()).unwrap_or("5001").to_string(),
                    space_key: "ENG".to_string(),
                    parent_id: body.get("parentId").and_then(|v| v.as_str()).map(str::to_string),
                    title: body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    version: 1,
                    adf_str,
                    storage_xhtml: "<p>created</p>".to_string(),
                    labels: vec![],
                    comments: vec![],
                    updated_at: "2026-11-15T12:20:00.000Z".to_string(),
                };
                let resp = st.page_to_cloud_v2_json(&rec, true);
                st.pages.write().unwrap().insert(pid, rec);
                (StatusCode::OK, Json(resp)).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                match guard.get(&page_id) {
                    Some(rec) => Json(st.page_to_cloud_v2_json(rec, true)).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            })
            .put(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let mut guard = st.pages.write().unwrap();
                let Some(rec) = guard.get_mut(&page_id) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let incoming_ver = body.pointer("/version/number").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                if incoming_ver != rec.version + 1 {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "message": format!("expected version {}, got {}", rec.version + 1, incoming_ver) })),
                    )
                        .into_response();
                }
                if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
                    rec.title = title.to_string();
                }
                if let Some(adf_str) = body.pointer("/body/value").and_then(|v| v.as_str()) {
                    rec.adf_str = adf_str.to_string();
                }
                rec.version = incoming_ver;
                Json(st.page_to_cloud_v2_json(rec, true)).into_response()
            })
            .delete(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                st.pages.write().unwrap().remove(&page_id);
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/children",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                let results: Vec<Value> = guard
                    .values()
                    .filter(|p| p.parent_id.as_deref() == Some(page_id.as_str()))
                    .map(|p| st.page_to_cloud_v2_json(p, false))
                    .collect();
                Json(json!({ "results": results })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/labels",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                let labels: Vec<Value> = guard
                    .get(&page_id)
                    .map(|p| p.labels.iter().map(|l| json!({ "name": l })).collect())
                    .unwrap_or_default();
                Json(json!({ "results": labels })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/attachments",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                Json(json!({ "results": [] })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/footer-comments",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                let results = guard.get(&page_id).map(|p| p.comments.clone()).unwrap_or_default();
                Json(json!({ "results": results })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/footer-comments",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let Some(page_id) = body.get("pageId").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                    return (StatusCode::BAD_REQUEST, Json(json!({ "message": "pageId required" }))).into_response();
                };
                let cid = format!("pc-{}", st.next_num());
                let comment = json!({
                    "id": cid,
                    "version": {
                        "number": 1,
                        "authorId": "acc-alice",
                        "createdAt": "2026-11-15T12:25:00.000Z"
                    },
                    "body": {
                        "atlas_doc_format": body.get("body").cloned().unwrap_or(Value::Null)
                    }
                });
                if let Some(p) = st.pages.write().unwrap().get_mut(page_id) {
                    p.comments.push(comment.clone());
                }
                Json(comment).into_response()
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return *resp;
                }
                let cql = q.get("cql").cloned().unwrap_or_default();
                let guard = st.pages.read().unwrap();
                let results: Vec<Value> = guard
                    .values()
                    .filter(|p| cql.is_empty() || p.title.contains("배포") || cql.contains(&p.title) || cql.contains("배포"))
                    .map(|p| {
                        json!({
                            "content": {
                                "id": p.id,
                                "type": "page",
                                "title": p.title,
                                "space": { "id": 5001, "key": p.space_key },
                                "version": { "number": p.version, "when": p.updated_at },
                                "_links": { "webui": format!("/spaces/{}/pages/{}", p.space_key, p.id) }
                            }
                        })
                    })
                    .collect();
                Json(json!({ "results": results })).into_response()
            }),
        )
        // ---------------- Data Center Jira v2 + Confluence v1 ----------------
        .route(
            "/rest/api/2/myself",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                Json(json!({
                    "name": "dc-admin",
                    "key": "JIRAUSER200",
                    "displayName": "DC Admin",
                    "emailAddress": "admin@corp.local",
                    "active": true
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/2/search",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                st.captured_dc_search_requests.write().unwrap().push(body.clone());
                let start_at = body.get("startAt").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let guard = st.issues.read().unwrap();
                let mut keys: Vec<String> = guard.keys().cloned().collect();
                keys.sort();
                let total = keys.len();
                let page_issues: Vec<Value> = keys
                    .into_iter()
                    .skip(start_at)
                    .take(1)
                    .filter_map(|k| guard.get(&k))
                    .map(|r| st.issue_to_jira_json(r, false))
                    .collect();
                Json(json!({
                    "startAt": start_at,
                    "maxResults": 1,
                    "total": total,
                    "issues": page_issues
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/2/issue",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                st.captured_dc_create_issue_bodies.write().unwrap().push(body.clone());
                let fields = body.get("fields").cloned().unwrap_or(Value::Null);
                let num = st.next_num();
                let key = format!("DC-{num}");
                let desc_str = fields.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let rec = MockIssueRecord {
                    id: format!("20{num}"),
                    key: key.clone(),
                    project_key: "DC".into(),
                    summary: fields.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    status_id: "10".into(),
                    status_name: "Open".into(),
                    status_category_key: "new".into(),
                    issue_type: fields.pointer("/issuetype/name").and_then(|v| v.as_str()).unwrap_or("Task").to_string(),
                    priority: "Medium".into(),
                    assignee_id: None,
                    assignee_name: None,
                    labels: vec![],
                    description_cloud_adf: Value::Null,
                    description_dc_wiki: desc_str,
                    comments: vec![],
                    attachments: vec![],
                    subtasks: vec![],
                    remote_links: vec![],
                    updated: "2026-11-15T12:30:00.000+0000".into(),
                };
                let id = rec.id.clone();
                st.issues.write().unwrap().insert(key.clone(), rec);
                (StatusCode::CREATED, Json(json!({ "id": id, "key": key }))).into_response()
            }),
        )
        .route(
            "/rest/api/2/issue/{key}",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                let guard = st.issues.read().unwrap();
                match guard.get(&key) {
                    Some(rec) => Json(st.issue_to_jira_json(rec, false)).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .route(
            "/rest/api/2/issue/{key}/remotelink",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                st.remotelink_requests.fetch_add(1, Ordering::SeqCst);
                Json(json!([])).into_response()
            }),
        )
        .route(
            "/rest/api/space",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                Json(json!({
                    "results": [{
                        "id": 5001,
                        "key": "ENG",
                        "name": "Engineering DC",
                        "type": "global",
                        "homepage": { "id": "2001" },
                        "description": { "plain": { "value": "On-premise wiki" } },
                        "_links": { "webui": "/display/ENG" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/content",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                st.captured_dc_create_page_bodies.write().unwrap().push(body.clone());
                let pid = format!("{}", 3000 + st.next_num());
                let storage_val = body.pointer("/body/storage/value").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let rec = MockPageRecord {
                    id: pid.clone(),
                    space_id: "5001".into(),
                    space_key: body.pointer("/space/key").and_then(|v| v.as_str()).unwrap_or("ENG").to_string(),
                    parent_id: None,
                    title: body.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    version: 1,
                    adf_str: String::new(),
                    storage_xhtml: storage_val,
                    labels: vec![],
                    comments: vec![],
                    updated_at: "2026-11-15T12:35:00.000Z".into(),
                };
                let resp = st.page_to_dc_v1_json(&rec, true);
                st.pages.write().unwrap().insert(pid, rec);
                Json(resp).into_response()
            }),
        )
        .route(
            "/rest/api/content/{page_id}",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                let guard = st.pages.read().unwrap();
                match guard.get(&page_id) {
                    Some(rec) => Json(st.page_to_dc_v1_json(rec, true)).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .route(
            "/rest/api/content/{page_id}/child/comment",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return *resp;
                }
                Json(json!({ "results": [] })).into_response()
            }),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

pub(crate) async fn spawn_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

