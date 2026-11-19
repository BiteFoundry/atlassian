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
    sync::{Arc, Once, OnceLock, RwLock},
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
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpListener;

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{AppConfig, DeploymentKind},
    img_preview, mcp,
    models::{
        DocBlock, OutgoingIssue, OutgoingPage, PageUpdate, SearchHitKind, StatusCategory,
    },
    serve,
    storage::Storage,
    updater,
};

static INIT_TEST_ENV: Once = Once::new();
static TEST_CONFIG_DIR: OnceLock<TempDir> = OnceLock::new();

fn ensure_isolated_env() {
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

fn generate_test_png_bytes() -> Vec<u8> {
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

fn check_basic_auth(headers: &HeaderMap, expected_email: &str, expected_token: &str) -> Result<(), Response> {
    let expected_raw = format!("{expected_email}:{expected_token}");
    let expected_header = format!("Basic {}", BASE64_STANDARD.encode(expected_raw.as_bytes()));
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if actual == Some(expected_header.as_str()) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "errorMessages": ["Invalid Cloud Basic credentials"] })),
        )
            .into_response())
    }
}

fn check_bearer_auth(headers: &HeaderMap, expected_pat: &str) -> Result<(), Response> {
    let expected_header = format!("Bearer {expected_pat}");
    let actual = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if actual == Some(expected_header.as_str()) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "errorMessages": ["Invalid Data Center Bearer PAT"] })),
        )
            .into_response())
    }
}

#[derive(Debug, Clone)]
struct MockAttachment {
    id: String,
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
struct MockIssueRecord {
    id: String,
    key: String,
    project_key: String,
    summary: String,
    status_id: String,
    status_name: String,
    status_category_key: String,
    issue_type: String,
    priority: String,
    assignee_id: Option<String>,
    assignee_name: Option<String>,
    labels: Vec<String>,
    description_cloud_adf: Value,
    description_dc_wiki: String,
    comments: Vec<Value>,
    attachments: Vec<MockAttachment>,
    subtasks: Vec<Value>,
    remote_links: Vec<Value>,
    updated: String,
}

#[derive(Debug, Clone)]
struct MockPageRecord {
    id: String,
    space_id: String,
    space_key: String,
    parent_id: Option<String>,
    title: String,
    version: i32,
    adf_str: String,
    storage_xhtml: String,
    labels: Vec<String>,
    comments: Vec<Value>,
    updated_at: String,
}

struct MockAtlassianState {
    cloud_email: String,
    cloud_token: String,
    dc_pat: String,
    base_url: RwLock<String>,
    seq: RwLock<u64>,
    issues: RwLock<HashMap<String, MockIssueRecord>>,
    pages: RwLock<HashMap<String, MockPageRecord>>,
    attachments: RwLock<HashMap<String, MockAttachment>>,
    captured_cloud_jql_requests: RwLock<Vec<Value>>,
    captured_cloud_create_issue_bodies: RwLock<Vec<Value>>,
    captured_cloud_create_page_bodies: RwLock<Vec<Value>>,
    captured_dc_search_requests: RwLock<Vec<Value>>,
    captured_dc_create_issue_bodies: RwLock<Vec<Value>>,
    captured_dc_create_page_bodies: RwLock<Vec<Value>>,
}

impl MockAtlassianState {
    fn new(cloud_email: &str, cloud_token: &str, dc_pat: &str) -> Arc<Self> {
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
        })
    }

    fn next_num(&self) -> u64 {
        let mut s = self.seq.write().unwrap();
        *s += 1;
        *s
    }

    fn issue_to_jira_json(&self, rec: &MockIssueRecord, is_cloud: bool) -> Value {
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

    fn page_to_cloud_v2_json(&self, rec: &MockPageRecord, include_body: bool) -> Value {
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

    fn page_to_dc_v1_json(&self, rec: &MockPageRecord, include_body: bool) -> Value {
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

fn build_mock_atlassian_router(state: Arc<MockAtlassianState>) -> Router {
    Router::new()
        // ---------------- Cloud Jira v3 + Agile v1 ----------------
        .route(
            "/rest/api/3/myself",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
                }
                let guard = st.issues.read().unwrap();
                match guard.get(&key) {
                    Some(rec) => Json(st.issue_to_jira_json(rec, true)).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({ "errorMessages": ["Issue not found"] }))).into_response(),
                }
            })
            .delete(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
                }
                st.issues.write().unwrap().remove(&key);
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/rest/api/3/issue/{key}/remotelink",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(key): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
                }
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
                }
                let guard = st.pages.read().unwrap();
                match guard.get(&page_id) {
                    Some(rec) => Json(st.page_to_cloud_v2_json(rec, true)).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            })
            .put(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
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
                    return resp;
                }
                st.pages.write().unwrap().remove(&page_id);
                StatusCode::NO_CONTENT.into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/children",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
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
                    return resp;
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
                    return resp;
                }
                Json(json!({ "results": [] })).into_response()
            }),
        )
        .route(
            "/wiki/api/v2/pages/{page_id}/footer-comments",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
                }
                let guard = st.pages.read().unwrap();
                let results = guard.get(&page_id).map(|p| p.comments.clone()).unwrap_or_default();
                Json(json!({ "results": results })).into_response()
            })
            .post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Path(page_id): Path<String>, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
                }
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
                if let Some(p) = st.pages.write().unwrap().get_mut(&page_id) {
                    p.comments.push(comment.clone());
                }
                Json(comment).into_response()
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
                }
                Json(json!([])).into_response()
            }),
        )
        .route(
            "/rest/api/space",
            get(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, &st.dc_pat) {
                    return resp;
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
                    return resp;
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
                    return resp;
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
                    return resp;
                }
                Json(json!({ "results": [] })).into_response()
            }),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

async fn spawn_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn e2e_cloud_jira_agile_and_sqlite_fts() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let creds = StoredCredentials::ApiToken {
        site_url: base_url.clone(),
        email: "alice@acme.com".into(),
        api_token: "cloud-api-tok-123".into(),
        deployment: DeploymentKind::Cloud,
    };
    assert!(creds.authorization_header().starts_with("Basic "));

    let auth = AuthManager::with_credentials("cloud-e2e", creds);
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("build cloud client");

    // 1. Myself, projects, agile boards/columns/sprints/issues
    let me = client.get_myself().await.expect("get_myself");
    assert_eq!(me.id, "acc-alice");
    assert_eq!(me.display_name, "Alice Kim");

    let projects = client.list_projects().await.expect("list_projects");
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].key, "PROJ");

    let boards = client.list_boards(Some("PROJ")).await.expect("list_boards");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].id, 11);

    let cols = client.get_board_columns(11).await.expect("get_board_columns");
    assert_eq!(cols.len(), 3);
    assert_eq!(cols[1].category_hint, StatusCategory::InProgress);

    let sprints = client.list_board_sprints(11).await.expect("list_board_sprints");
    assert_eq!(sprints.len(), 1);
    assert_eq!(sprints[0].id, 101);

    let board_issues = client.list_board_issues(11, Some(101), 50).await.expect("list_board_issues");
    assert!(board_issues.iter().any(|i| i.key == "PROJ-101"));

    // 2. Cloud POST /rest/api/3/search/jql with nextPageToken pagination
    let page1 = client
        .search_issues_jql("project = PROJ ORDER BY key ASC", 1, None, None)
        .await
        .expect("search_issues_jql page 1");
    assert_eq!(page1.issues.len(), 1);
    assert_eq!(page1.next_page_token.as_deref(), Some("page-2-token"));

    let page2 = client
        .search_issues_jql(
            "project = PROJ ORDER BY key ASC",
            1,
            page1.next_page_token.as_deref(),
            None,
        )
        .await
        .expect("search_issues_jql page 2");
    assert!(!page2.issues.is_empty());
    assert!(page2.next_page_token.is_none());
    assert_eq!(state.captured_cloud_jql_requests.read().unwrap().len(), 2);

    // 3. create_issue (Markdown -> ADF JSON wire verification)
    let created = client
        .create_issue(&OutgoingIssue {
            project_key: "PROJ".into(),
            issue_type: "Task".into(),
            summary: "신규 배포 체크".into(),
            description_markdown: "# 배포 확인\n\n- **스테이징** 검증".into(),
            priority: Some("High".into()),
            assignee_id: Some("acc-alice".into()),
            labels: vec!["deploy".into()],
            parent_key: None,
        })
        .await
        .expect("create_issue");
    assert!(created.summary.key.starts_with("PROJ-"));
    {
        let bodies = state.captured_cloud_create_issue_bodies.read().unwrap();
        let desc = &bodies[0]["fields"]["description"];
        assert_eq!(desc["type"], "doc");
        assert_eq!(desc["version"], 1);
        assert!(desc["content"].is_array());
    }

    // 4. get_issue with remote Confluence link + subtasks
    let detail = client.get_issue("PROJ-101").await.expect("get_issue PROJ-101");
    assert_eq!(detail.subtasks.len(), 1);
    assert_eq!(detail.subtasks[0].key, "PROJ-102");
    assert!(detail.linked_pages.iter().any(|p| p.page_id == "2001"));

    // 5. transition_issue, assign_issue, add_issue_comment
    client
        .transition_issue("PROJ-101", "Done", Some("배포 완료 확인"))
        .await
        .expect("transition_issue");
    client
        .assign_issue(&created.summary.key, Some("acc-alice"))
        .await
        .expect("assign_issue");
    let comment = client
        .add_issue_comment("PROJ-101", "체크 완료했습니다.")
        .await
        .expect("add_issue_comment");
    assert!(!comment.id.is_empty());

    let updated_101 = client.get_issue("PROJ-101").await.expect("re-fetch PROJ-101");
    assert_eq!(updated_101.summary.status.name, "Done");
    assert_eq!(updated_101.comments.len(), 2);

    // 6. upload_issue_attachment (X-Atlassian-Token: no-check) & download_attachment
    let tmp = tempfile::tempdir().expect("tempdir");
    let png_path = tmp.path().join("preview.png");
    let png_bytes = generate_test_png_bytes();
    std::fs::write(&png_path, &png_bytes).expect("write test png");

    let uploaded = client
        .upload_issue_attachment("PROJ-101", &png_path)
        .await
        .expect("upload_issue_attachment");
    assert_eq!(uploaded.len(), 1);
    assert_eq!(uploaded[0].filename, "preview.png");

    let dl_dir = tmp.path().join("downloads");
    let downloaded = client
        .download_attachment(&uploaded[0], &dl_dir)
        .await
        .expect("download_attachment");
    assert_eq!(std::fs::read(&downloaded.path).unwrap(), png_bytes);

    // 7. SQLite Storage + issues_fts (including 2-char Korean search "배포")
    let storage = Storage::open_in_memory().expect("open in-memory storage");
    storage.upsert_projects(&projects).expect("upsert_projects");
    storage.upsert_boards(&boards).expect("upsert_boards");
    storage.upsert_sprints(&sprints).expect("upsert_sprints");
    storage.upsert_issues(&board_issues).expect("upsert_issues");
    storage.upsert_issue_detail(&updated_101).expect("upsert_issue_detail");

    let ko_hits = storage.search_issues("배포", 10).expect("search_issues 배포");
    assert!(
        ko_hits.iter().any(|h| h.id_or_key == "PROJ-101"),
        "expected 2-char Korean query '배포' to match PROJ-101, got {ko_hits:?}"
    );
}

#[tokio::test]
async fn e2e_cloud_confluence_v2_and_unified_search() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let auth = AuthManager::with_credentials(
        "conf-e2e",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("build cloud client");

    // 1. Spaces & page tree
    let spaces = client.list_spaces(20).await.expect("list_spaces");
    assert_eq!(spaces.len(), 1);
    assert_eq!(spaces[0].key, "ENG");

    let roots = client
        .list_space_root_pages("ENG", 20)
        .await
        .expect("list_space_root_pages");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].id, "2001");

    let children = client.list_child_pages("2001").await.expect("list_child_pages");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, "2002");

    // 2. get_page (ADF + TOC + cross-ref PROJ-101 extraction)
    let page_2001 = client.get_page("2001").await.expect("get_page 2001");
    assert_eq!(page_2001.body_representation, "atlas_doc_format");
    assert!(page_2001.toc.len() >= 2, "expected TOC headings, got {:?}", page_2001.toc);
    assert!(
        page_2001.linked_issue_keys.contains(&"PROJ-101".to_string()),
        "expected PROJ-101 cross-ref, got {:?}",
        page_2001.linked_issue_keys
    );

    // 3. create_page (stringified ADF atlas_doc_format on wire)
    let created_page = client
        .create_page(&OutgoingPage {
            space_id_or_key: "ENG".into(),
            parent_id: Some("2001".into()),
            title: "신규 배포 런북".into(),
            body_markdown: "# 배포 절차\n\nPROJ-101 확인 필요".into(),
        })
        .await
        .expect("create_page");
    assert_eq!(created_page.summary.version_number, 1);
    {
        let bodies = state.captured_cloud_create_page_bodies.read().unwrap();
        assert_eq!(bodies[0]["body"]["representation"], "atlas_doc_format");
        let adf_str = bodies[0]["body"]["value"].as_str().expect("stringified ADF");
        let parsed: Value = serde_json::from_str(adf_str).expect("valid ADF JSON");
        assert_eq!(parsed["type"], "doc");
    }

    // 4. update_page (version.number increments 1 -> 2)
    let updated_page = client
        .update_page(
            &created_page.summary.id,
            &PageUpdate {
                title: "신규 배포 런북 v2".into(),
                body_markdown: "# 배포 절차 (업데이트)\n\n검증 완료".into(),
                prev_version: 1,
                version_message: Some("2차 수정".into()),
            },
        )
        .await
        .expect("update_page");
    assert_eq!(updated_page.summary.version_number, 2);

    // 5. add_page_comment & CQL search
    let page_comment = client
        .add_page_comment(&created_page.summary.id, "검토 완료")
        .await
        .expect("add_page_comment");
    assert!(!page_comment.id.is_empty());

    let cql_hits = client
        .search_confluence_cql("text ~ \"배포\"", 20)
        .await
        .expect("search_confluence_cql");
    assert!(!cql_hits.is_empty());

    // 6. SQLite pages_fts + search_unified across BOTH Jira & Confluence
    let storage = Storage::open_in_memory().expect("open storage");
    let issues = client
        .search_issues_jql("project = PROJ", 10, None, None)
        .await
        .unwrap()
        .issues;
    storage.upsert_issues(&issues).unwrap();
    storage.upsert_spaces(&spaces).unwrap();
    storage.upsert_pages(&roots).unwrap();
    storage.upsert_page_detail(&page_2001).unwrap();

    let page_hits = storage.search_pages("배포", 10).expect("search_pages");
    assert!(page_hits.iter().any(|h| h.id_or_key == "2001"));

    let unified = storage.search_unified("배포", 20).expect("search_unified");
    assert!(
        unified.iter().any(|h| h.kind == SearchHitKind::JiraIssue),
        "expected Jira hit in unified search: {unified:?}"
    );
    assert!(
        unified.iter().any(|h| h.kind == SearchHitKind::ConfluencePage),
        "expected Confluence hit in unified search: {unified:?}"
    );
}

#[tokio::test]
async fn e2e_datacenter_pat_jira_v2_and_confluence_v1() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat-secret-999");
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let creds = StoredCredentials::BearerPat {
        site_url: base_url.clone(),
        token: "dc-pat-secret-999".into(),
        deployment: DeploymentKind::DataCenter,
    };
    assert_eq!(creds.authorization_header(), "Bearer dc-pat-secret-999");

    let auth = AuthManager::with_credentials("dc-e2e", creds);
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth)
        .expect("build dc client");

    // 1. /rest/api/2/myself
    let me = client.get_myself().await.expect("dc get_myself");
    assert_eq!(me.id, "dc-admin");
    assert_eq!(me.display_name, "DC Admin");

    // 2. POST /rest/api/2/search with startAt pagination
    let p1 = client
        .search_issues_jql("project = PROJ", 1, None, Some(0))
        .await
        .expect("dc search startAt=0");
    assert_eq!(p1.issues.len(), 1);
    assert_eq!(p1.next_start_at, Some(1));

    let p2 = client
        .search_issues_jql("project = PROJ", 1, None, p1.next_start_at)
        .await
        .expect("dc search startAt=1");
    assert_eq!(p2.issues.len(), 1);
    assert!(p2.next_start_at.is_none());

    // 3. POST /rest/api/2/issue (Wiki Markup)
    let created_dc_issue = client
        .create_issue(&OutgoingIssue {
            project_key: "DC".into(),
            issue_type: "Task".into(),
            summary: "Data Center 이슈".into(),
            description_markdown: "# 제목\n\n`code` 테스트".into(),
            priority: None,
            assignee_id: None,
            labels: vec![],
            parent_key: None,
        })
        .await
        .expect("dc create_issue");
    assert!(created_dc_issue.summary.key.starts_with("DC-"));
    {
        let bodies = state.captured_dc_create_issue_bodies.read().unwrap();
        assert!(bodies[0]["fields"]["description"].is_string());
    }

    // 4. Confluence DC /rest/api/space & /rest/api/content (Storage XHTML <ac:structured-macro>)
    let spaces = client.list_spaces(20).await.expect("dc list_spaces");
    assert_eq!(spaces[0].key, "ENG");

    let dc_page = client.get_page("2001").await.expect("dc get_page 2001");
    assert_eq!(dc_page.body_representation, "storage");
    assert!(
        dc_page
            .blocks
            .iter()
            .any(|b| matches!(b, DocBlock::Panel { .. })),
        "expected <ac:structured-macro ac:name=\"info\"> to parse as DocBlock::Panel, got {:?}",
        dc_page.blocks
    );

    let dc_created_page = client
        .create_page(&OutgoingPage {
            space_id_or_key: "ENG".into(),
            parent_id: None,
            title: "DC Storage 페이지".into(),
            body_markdown: "# DC 가이드\n\n본문입니다.".into(),
        })
        .await
        .expect("dc create_page");
    assert_eq!(dc_created_page.body_representation, "storage");
    {
        let bodies = state.captured_dc_create_page_bodies.read().unwrap();
        assert_eq!(bodies[0]["body"]["storage"]["representation"], "storage");
    }
}

#[tokio::test]
async fn e2e_serve_http_mcp_image_preview_and_self_updater() {
    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    let atlassian_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = atlassian_url.clone();

    let mut cfg = AppConfig::default();
    cfg.site_url = atlassian_url.clone();
    cfg.deployment = DeploymentKind::Cloud;

    let auth = AuthManager::with_credentials(
        "srv-e2e",
        StoredCredentials::ApiToken {
            site_url: atlassian_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::new(cfg.clone(), auth).expect("client");
    let storage = Storage::open_in_memory().expect("storage");

    // Seed cache for offline search checks
    let issue_101 = client.get_issue("PROJ-101").await.unwrap();
    storage.upsert_issue_detail(&issue_101).unwrap();
    let page_2001 = client.get_page("2001").await.unwrap();
    storage.upsert_page_detail(&page_2001).unwrap();

    // 1. HTTP API (serve::build_router)
    let api_token = "local-serve-secret";
    let serve_url = spawn_server(serve::build_router(
        cfg.clone(),
        client.clone(),
        storage.clone(),
        Some(api_token.to_string()),
    ))
    .await;

    let http = reqwest::Client::new();
    let health_resp = http
        .get(format!("{serve_url}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health_resp.status(), StatusCode::OK);
    let health_json: Value = health_resp.json().await.unwrap();
    assert_eq!(health_json["ok"], true);

    // Bearer-protected endpoints reject missing token
    let unauth_resp = http
        .get(format!("{serve_url}/issues"))
        .send()
        .await
        .expect("GET /issues unauth");
    assert_eq!(unauth_resp.status(), StatusCode::UNAUTHORIZED);

    let issues_resp = http
        .get(format!("{serve_url}/issues"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /issues");
    assert_eq!(issues_resp.status(), StatusCode::OK);
    let issues_json: Value = issues_resp.json().await.unwrap();
    assert!(!issues_json["issues"].as_array().unwrap().is_empty());

    let page_resp = http
        .get(format!("{serve_url}/pages/2001"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /pages/2001");
    assert_eq!(page_resp.status(), StatusCode::OK);
    let page_json: Value = page_resp.json().await.unwrap();
    assert_eq!(page_json["page"]["summary"]["id"], "2001");

    let search_resp = http
        .get(format!("{serve_url}/search?q=%EB%B0%B0%ED%8F%AC"))
        .bearer_auth(api_token)
        .send()
        .await
        .expect("GET /search");
    assert_eq!(search_resp.status(), StatusCode::OK);
    let search_json: Value = search_resp.json().await.unwrap();
    assert!(!search_json["hits"].as_array().unwrap().is_empty());

    // 2. MCP (mcp::handle_rpc_line)
    let init_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        })
        .to_string(),
    )
    .await
    .expect("initialize response");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "atlassian");

    let list_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
    )
    .await
    .expect("tools/list response");
    let tools = list_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 27, "expected 27 MCP tools, got {}", tools.len());
    assert!(tools.iter().any(|t| t["name"] == "list_sites"));

    let get_issue_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_jira_issue", "arguments": { "key": "PROJ-101" } }
        })
        .to_string(),
    )
    .await
    .expect("get_jira_issue mcp");
    assert_ne!(get_issue_mcp["result"]["isError"], true);

    let get_page_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "2001" } }
        })
        .to_string(),
    )
    .await
    .expect("get_confluence_page mcp");
    assert_ne!(get_page_mcp["result"]["isError"], true);

    let search_mcp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "search_all", "arguments": { "query": "배포" } }
        })
        .to_string(),
    )
    .await
    .expect("search_all mcp");
    assert_ne!(search_mcp["result"]["isError"], true);

    // Destructive confirm: true guard
    let del_blocked = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "delete_jira_issue", "arguments": { "key": "PROJ-103" } }
        })
        .to_string(),
    )
    .await
    .expect("delete_jira_issue without confirm");
    assert_eq!(del_blocked["result"]["isError"], true);

    let del_allowed = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "delete_jira_issue", "arguments": { "key": "PROJ-103", "confirm": true } }
        })
        .to_string(),
    )
    .await
    .expect("delete_jira_issue with confirm: true");
    assert_ne!(del_allowed["result"]["isError"], true);

    // 3. Inline image preview (render_halfblocks + render_for_stdout)
    let png_bytes = generate_test_png_bytes();
    let rendered = img_preview::render_halfblocks(&png_bytes, 16, 8).expect("render_halfblocks");
    assert_eq!(rendered.width_px, 16);
    assert_eq!(rendered.height_px, 16);
    assert!(!rendered.lines.is_empty());

    std::env::set_var("ATLASSIAN_IMAGE_PROTOCOL", "halfblocks");
    let stdout_ansi = img_preview::render_for_stdout(&png_bytes, 16, 8).expect("render_for_stdout");
    assert!(stdout_ansi.contains('▀'));
    std::env::remove_var("ATLASSIAN_IMAGE_PROTOCOL");

    // 4. Self-updater against mock GitHub release endpoint
    let asset_name = if cfg!(windows) { "jira.exe" } else { "jira" };
    let new_binary_payload = b"mock-updated-jira-binary-v9.9.9".to_vec();
    let sha_hex = hex::encode(Sha256::digest(&new_binary_payload));
    let sums_txt = format!("{sha_hex}  {asset_name}\n");

    let gh_base_holder = Arc::new(RwLock::new(String::new()));
    let gh_base_clone = gh_base_holder.clone();
    let bin_bytes = new_binary_payload.clone();
    let sums_clone = sums_txt.clone();
    let asset_name_owned = asset_name.to_string();

    let gh_app = Router::new()
        .route(
            "/repos/shinick-dev/atlassian/releases/latest",
            get(move || {
                let gh_base = gh_base_clone.read().unwrap().clone();
                let asset = asset_name_owned.clone();
                let bin_len = bin_bytes.len() as u64;
                let sums_len = sums_clone.len() as u64;
                async move {
                    Json(json!({
                        "tag_name": "v9.9.9",
                        "html_url": format!("{gh_base}/shinick-dev/atlassian/releases/tag/v9.9.9"),
                        "assets": [
                            {
                                "name": asset,
                                "size": bin_len,
                                "browser_download_url": format!("{gh_base}/download/{asset}")
                            },
                            {
                                "name": "SHA256SUMS.txt",
                                "size": sums_len,
                                "browser_download_url": format!("{gh_base}/download/SHA256SUMS.txt")
                            }
                        ]
                    }))
                }
            }),
        )
        .route(
            &format!("/download/{asset_name}"),
            get({
                let b = new_binary_payload.clone();
                move || async move { b }
            }),
        )
        .route(
            "/download/SHA256SUMS.txt",
            get({
                let s = sums_txt.clone();
                move || async move { s }
            }),
        );

    let gh_url = spawn_server(gh_app).await;
    *gh_base_holder.write().unwrap() = gh_url.clone();

    let upd_dir = tempfile::tempdir().expect("updater tempdir");
    let target_exe = upd_dir.path().join(asset_name);
    std::fs::write(&target_exe, b"old-binary-v0.1.0").unwrap();

    let check_res = updater::check_or_apply_update_at(
        &gh_url,
        "shinick-dev/atlassian",
        true,
        false,
        &target_exe,
        None,
    )
    .await
    .expect("update check");
    assert!(check_res.update_available);
    assert!(!check_res.updated);

    let apply_res = updater::check_or_apply_update_at(
        &gh_url,
        "shinick-dev/atlassian",
        false,
        false,
        &target_exe,
        None,
    )
    .await
    .expect("update apply");
    assert!(apply_res.updated);
    assert_eq!(std::fs::read(&target_exe).unwrap(), new_binary_payload);
}

#[tokio::test]
async fn e2e_multi_site_oauth_3lo_refresh_collision_free_sync_and_federation() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use atlassian_cli::{
        atlassian::SiteClientPool,
        auth::{exchange_oauth_code, fetch_accessible_resources},
        config::SiteProfile,
        storage::SiteRecord,
        sync::{self, CircuitBreakerMap, SiteSyncTarget},
    };

    ensure_isolated_env();

    struct MockOAuthGatewayState {
        refresh_calls: AtomicUsize,
        acme_forced_401: AtomicUsize,
        gateway_hits_acme: AtomicUsize,
        gateway_hits_side: AtomicUsize,
    }

    let oauth_state = Arc::new(MockOAuthGatewayState {
        refresh_calls: AtomicUsize::new(0),
        acme_forced_401: AtomicUsize::new(0),
        gateway_hits_acme: AtomicUsize::new(0),
        gateway_hits_side: AtomicUsize::new(0),
    });

    fn check_oauth_bearer(headers: &HeaderMap, expected: &str) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            == Some(&format!("Bearer {expected}"))
    }

    let oauth_router = Router::new()
        .route(
            "/oauth/token",
            post(|State(st): State<Arc<MockOAuthGatewayState>>, Json(body): Json<Value>| async move {
                let grant = body["grant_type"].as_str().unwrap_or_default();
                match grant {
                    "authorization_code" => {
                        assert_eq!(body["code"], "code-3lo-xyz");
                        assert_eq!(body["client_id"], "mock-client-id");
                        (
                            StatusCode::OK,
                            Json(json!({
                                "access_token": "oauth-tok-v1",
                                "refresh_token": "oauth-ref-v1",
                                "expires_in": 3600,
                                "scope": "offline_access read:jira-work read:confluence-content.all"
                            })),
                        )
                    }
                    "refresh_token" => {
                        assert_eq!(body["refresh_token"], "oauth-ref-v1");
                        st.refresh_calls.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(json!({
                                "access_token": "oauth-tok-v2",
                                "refresh_token": "oauth-ref-v2",
                                "expires_in": 3600,
                                "scope": "offline_access read:jira-work read:confluence-content.all"
                            })),
                        )
                    }
                    _ => (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "unsupported_grant_type" })),
                    ),
                }
            }),
        )
        .route(
            "/oauth/token/accessible-resources",
            get(|headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" })));
                }
                (
                    StatusCode::OK,
                    Json(json!([
                        {
                            "id": "cloud-id-acme",
                            "url": "https://acme-cloud.atlassian.net",
                            "name": "Acme Cloud",
                            "scopes": ["read:jira-work", "read:confluence-content.all"]
                        },
                        {
                            "id": "cloud-id-side",
                            "url": "https://side-cloud.atlassian.net",
                            "name": "Side Cloud",
                            "scopes": ["read:jira-work", "read:confluence-content.all"]
                        }
                    ])),
                )
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/api/3/project/search",
            get(|State(st): State<Arc<MockOAuthGatewayState>>, Path(cid): Path<String>, headers: HeaderMap| async move {
                // Force a 401 on the very first request to Acme Cloud with v1 token to verify automatic refresh!
                if cid == "cloud-id-acme" && check_oauth_bearer(&headers, "oauth-tok-v1") {
                    if st.acme_forced_401.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "message": "Access token expired" })),
                        );
                    }
                }
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                if cid == "cloud-id-acme" {
                    st.gateway_hits_acme.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "values": [{
                                "id": "10001",
                                "key": "ENG",
                                "name": "Acme Engineering",
                                "projectTypeKey": "software"
                            }]
                        })),
                    )
                } else {
                    st.gateway_hits_side.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        Json(json!({
                            "values": [{
                                "id": "20001",
                                "key": "SIDE",
                                "name": "Side Product",
                                "projectTypeKey": "software"
                            }]
                        })),
                    )
                }
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board",
            get(|Path(cid): Path<String>, headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                let (id, name, pkey) = if cid == "cloud-id-acme" {
                    (1, "Acme Scrum", "ENG")
                } else {
                    (2, "Side Kanban", "SIDE")
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "values": [{
                            "id": id,
                            "name": name,
                            "type": "scrum",
                            "location": { "projectKey": pkey, "projectName": name }
                        }]
                    })),
                )
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board/{board_id}/sprint",
            get(|Path((_cid, board_id)): Path<(String, i64)>| async move {
                Json(json!({
                    "values": [{
                        "id": 10,
                        "name": format!("Sprint 10 (B{board_id})"),
                        "state": "active"
                    }]
                }))
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/agile/1.0/board/{board_id}/sprint/{sprint_id}/issue",
            get(|Path((cid, _bid, _sid)): Path<(String, i64, i64)>| async move {
                let (key, proj, summary) = if cid == "cloud-id-acme" {
                    ("ENG-1", "ENG", "Acme Cloud Gateway Migration")
                } else {
                    ("SIDE-1", "SIDE", "Side Cloud Telemetry Rollout")
                };
                Json(json!({
                    "issues": [{
                        "id": format!("id-{cid}-{key}"),
                        "key": key,
                        "fields": {
                            "project": { "key": proj },
                            "summary": summary,
                            "status": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T12:00:00.000+0000"
                        }
                    }]
                }))
            }),
        )
        .route(
            "/ex/jira/{cloud_id}/rest/api/3/search/jql",
            post(|Path(cid): Path<String>, headers: HeaderMap| async move {
                if !check_oauth_bearer(&headers, "oauth-tok-v1")
                    && !check_oauth_bearer(&headers, "oauth-tok-v2")
                {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Unauthorized" })));
                }
                let (key, proj, summary) = if cid == "cloud-id-acme" {
                    ("ENG-1", "ENG", "Acme Cloud Gateway Migration")
                } else {
                    ("SIDE-1", "SIDE", "Side Cloud Telemetry Rollout")
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "issues": [{
                            "id": format!("id-{cid}-{key}"),
                            "key": key,
                            "fields": {
                                "project": { "key": proj },
                                "summary": summary,
                                "status": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                                "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                                "updated": "2026-11-19T12:00:00.000+0000"
                            }
                        }]
                    })),
                )
            }),
        )
        .route(
            "/ex/confluence/{cloud_id}/wiki/api/v2/spaces",
            get(|Path(cid): Path<String>| async move {
                let (id, key, name) = if cid == "cloud-id-acme" {
                    ("sp-acme", "DOCS", "Acme Cloud Docs")
                } else {
                    ("sp-side", "SIDEWIKI", "Side Cloud Wiki")
                };
                Json(json!({
                    "results": [{
                        "id": id,
                        "key": key,
                        "name": name,
                        "type": "global",
                        "status": "current"
                    }]
                }))
            }),
        )
        .route(
            "/ex/confluence/{cloud_id}/wiki/api/v2/spaces/{space_id}/pages",
            get(|Path((cid, space_id)): Path<(String, String)>| async move {
                let title = if cid == "cloud-id-acme" {
                    "Acme Cloud Gateway Runbook"
                } else {
                    "Side Cloud Guide"
                };
                Json(json!({
                    "results": [{
                        "id": "100",
                        "spaceId": space_id,
                        "title": title,
                        "status": "current",
                        "version": { "number": 1, "createdAt": "2026-11-19T12:00:00Z" }
                    }]
                }))
            }),
        )
        .with_state(oauth_state.clone());

    let oauth_base = spawn_server(oauth_router).await;

    // Mock Data Center server (`corp-dc`) sharing the EXACT same project key `ENG` and issue key `ENG-1`!
    let dc_router = Router::new()
        .route(
            "/rest/api/2/project",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return resp;
                }
                Json(json!([{
                    "id": "90001",
                    "key": "ENG",
                    "name": "Corp DC Engineering",
                    "projectTypeKey": "software"
                }]))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return resp;
                }
                Json(json!({
                    "values": [{
                        "id": 1,
                        "name": "Corp DC Board",
                        "type": "scrum",
                        "location": { "projectKey": "ENG", "projectName": "Corp DC Engineering" }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/sprint",
            get(|| async {
                Json(json!({
                    "values": [{
                        "id": 10,
                        "name": "Corp DC Sprint 10",
                        "state": "active"
                    }]
                }))
            }),
        )
        .route(
            "/rest/agile/1.0/board/{board_id}/sprint/{sprint_id}/issue",
            get(|| async {
                Json(json!({
                    "issues": [{
                        "id": "dc-eng-1",
                        "key": "ENG-1",
                        "fields": {
                            "project": { "key": "ENG" },
                            "summary": "Corp DC Gateway Migration",
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T13:00:00.000+0000"
                        }
                    }]
                }))
            }),
        )
        .route(
            "/rest/api/2/search",
            post(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return resp;
                }
                Json(json!({
                    "startAt": 0,
                    "maxResults": 50,
                    "total": 1,
                    "issues": [{
                        "id": "dc-eng-1",
                        "key": "ENG-1",
                        "fields": {
                            "project": { "key": "ENG" },
                            "summary": "Corp DC Gateway Migration",
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "10001", "name": "Task", "subtask": false },
                            "updated": "2026-11-19T13:00:00.000+0000"
                        }
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/space",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return resp;
                }
                Json(json!({
                    "results": [{
                        "id": 901,
                        "key": "DOCS",
                        "name": "Corp DC Docs",
                        "type": "global"
                    }]
                }))
                .into_response()
            }),
        )
        .route(
            "/rest/api/content",
            get(|headers: HeaderMap| async move {
                if let Err(resp) = check_bearer_auth(&headers, "dc-pat-secret") {
                    return resp;
                }
                Json(json!({
                    "results": [{
                        "id": "100",
                        "title": "Corp DC Gateway Runbook",
                        "status": "current",
                        "space": { "id": 901, "key": "DOCS" },
                        "version": { "number": 2, "when": "2026-11-19T13:00:00Z" }
                    }]
                }))
                .into_response()
            }),
        );

    let dc_base = spawn_server(dc_router).await;

    // 1. OAuth 3LO token exchange + accessible-resources auto-discovery
    let http = reqwest::Client::new();
    let initial_oauth_creds = exchange_oauth_code(
        &http,
        &oauth_base,
        "mock-client-id",
        Some("mock-client-secret"),
        "code-3lo-xyz",
        "http://127.0.0.1:9999/callback",
    )
    .await
    .expect("OAuth 3LO token exchange");
    assert_eq!(initial_oauth_creds.oauth_access_token(), Some("oauth-tok-v1"));

    let resources = fetch_accessible_resources(
        &http,
        &oauth_base,
        initial_oauth_creds.oauth_access_token().unwrap(),
    )
    .await
    .expect("fetch accessible-resources");
    assert_eq!(resources.len(), 2);

    let cred_dir = tempfile::tempdir().expect("isolated credentials dir");
    let cred_path = cred_dir.path().join("credentials.json");
    let oauth_auth = AuthManager::for_account("cloud-oauth").with_fallback_path(cred_path.clone());
    oauth_auth
        .save_credentials(&initial_oauth_creds)
        .expect("save OAuth credentials");
    let dc_auth = AuthManager::for_account("corp-dc-acct").with_fallback_path(cred_path.clone());
    dc_auth
        .save_credentials(&StoredCredentials::BearerPat {
            site_url: dc_base.clone(),
            token: "dc-pat-secret".to_string(),
            deployment: DeploymentKind::DataCenter,
        })
        .expect("save DC PAT credentials");

    let mut cfg = AppConfig::default();
    for res in &resources {
        cfg.upsert_site(res.to_site_profile("cloud-oauth"));
    }
    cfg.upsert_site(SiteProfile {
        id: "corp-dc".to_string(),
        name: "Corp DC".to_string(),
        site_url: dc_base.clone(),
        confluence_url: dc_base.clone(),
        deployment: DeploymentKind::DataCenter,
        cloud_id: None,
        account_id: "corp-dc-acct".to_string(),
        enabled: true,
        scopes: Vec::new(),
    });
    cfg.default_site = Some("acme-cloud".to_string());

    // 2. Build SiteClientPool with OAuth gateway + token refresh endpoints
    let pool = SiteClientPool::from_config_with_options(
        &cfg,
        Some(cred_path.clone()),
        Some(oauth_base.clone()),
        Some(oauth_base.clone()),
    )
    .expect("build SiteClientPool");
    assert_eq!(pool.enabled_clients().len(), 3);

    // 3. Parallel multi-site sync into SQLite v2
    let storage = Storage::open_in_memory().expect("in-memory SQLite v2");
    for site in cfg.effective_sites() {
        storage
            .upsert_site(&SiteRecord {
                site_id: site.id.clone(),
                name: site.display_name().to_string(),
                site_url: site.site_url.clone(),
                deployment: site.deployment.as_str().to_string(),
                enabled: site.enabled,
                last_synced_at: None,
                last_error: None,
            })
            .unwrap();
    }

    let targets: Vec<SiteSyncTarget> = pool
        .enabled_clients()
        .into_iter()
        .map(|(site, client)| SiteSyncTarget {
            site_id: site.id.clone(),
            site_name: site.display_name().to_string(),
            client,
        })
        .collect();

    let mut breakers = CircuitBreakerMap::new();
    let outcome = sync::run_multi_site_sync(&targets, &storage, &mut breakers, 4, None).await;
    assert_eq!(outcome.total_sites, 3, "outcome: {outcome:?}");
    assert_eq!(outcome.succeeded_sites, 3, "outcome: {outcome:?}");

    // Verify OAuth 3LO gateway routing + 401 automatic refresh + rotated refresh token persistence
    assert!(
        oauth_state.gateway_hits_acme.load(Ordering::SeqCst) >= 1,
        "expected OAuth gateway hits for acme-cloud"
    );
    assert!(
        oauth_state.gateway_hits_side.load(Ordering::SeqCst) >= 1,
        "expected OAuth gateway hits for side-cloud"
    );
    assert!(
        oauth_state.refresh_calls.load(Ordering::SeqCst) >= 1,
        "expected automatic 401 OAuth token refresh"
    );
    let refreshed_creds = oauth_auth.load_credentials().unwrap().unwrap();
    assert_eq!(refreshed_creds.oauth_access_token(), Some("oauth-tok-v2"));
    match refreshed_creds {
        StoredCredentials::CloudOAuth { refresh_token, .. } => {
            assert_eq!(refresh_token.as_deref(), Some("oauth-ref-v2"));
        }
        other => panic!("expected CloudOAuth credentials, got {other:?}"),
    }

    // 4. Verify zero key collision across acme-cloud and corp-dc for ENG / ENG-1
    let acme_projects = storage.list_projects_for_site("acme-cloud").unwrap();
    let dc_projects = storage.list_projects_for_site("corp-dc").unwrap();
    assert_eq!(acme_projects.len(), 1);
    assert_eq!(dc_projects.len(), 1);
    assert_eq!(acme_projects[0].key, "ENG");
    assert_eq!(acme_projects[0].name, "Acme Engineering");
    assert_eq!(dc_projects[0].key, "ENG");
    assert_eq!(dc_projects[0].name, "Corp DC Engineering");

    let acme_issues = storage.list_issues_for_site("acme-cloud", None, 20).unwrap();
    let dc_issues = storage.list_issues_for_site("corp-dc", None, 20).unwrap();
    assert_eq!(acme_issues[0].key, "ENG-1");
    assert_eq!(acme_issues[0].summary, "Acme Cloud Gateway Migration");
    assert_eq!(dc_issues[0].key, "ENG-1");
    assert_eq!(dc_issues[0].summary, "Corp DC Gateway Migration");

    // 5. Verify federated unified_search_scoped returns BOTH ENG-1 hits with distinct site_ids!
    let federated_hits = storage
        .unified_search_scoped("Gateway Migration", None, 20)
        .expect("federated unified_search_scoped");
    let eng1_sites: Vec<&str> = federated_hits
        .iter()
        .filter(|(_, h)| h.id_or_key == "ENG-1")
        .map(|(sid, _)| sid.as_str())
        .collect();
    assert_eq!(eng1_sites.len(), 2, "expected 2 distinct ENG-1 hits, got {federated_hits:?}");
    assert!(eng1_sites.contains(&"acme-cloud"));
    assert!(eng1_sites.contains(&"corp-dc"));

    let acme_only_hits = storage
        .unified_search_scoped("Gateway Migration", Some("acme-cloud"), 20)
        .expect("scoped search");
    assert_eq!(acme_only_hits.len(), 1);
    assert_eq!(acme_only_hits[0].0, "acme-cloud");

    // 6. Verify HTTP GET /api/v1/sites & ?site= scoping + MCP list_sites & site argument
    let default_client = pool.client_for("acme-cloud").unwrap();
    let api_router = serve::build_router(cfg.clone(), default_client.clone(), storage.clone(), None);
    let api_base = spawn_server(api_router).await;

    let sites_resp: Value = http
        .get(format!("{api_base}/api/v1/sites"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sites_resp["sites"].as_array().unwrap().len(), 3);

    let dc_search_resp: Value = http
        .get(format!("{api_base}/search?q=Gateway&site=corp-dc"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dc_hits = dc_search_resp["hits"].as_array().unwrap();
    assert!(!dc_hits.is_empty());
    assert!(dc_hits.iter().all(|h| h["site_id"] == "corp-dc"));

    let mcp_sites = mcp::handle_rpc_line(
        &cfg,
        &default_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 101,
            "method": "tools/call",
            "params": { "name": "list_sites", "arguments": {} }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_sites["result"]["isError"], false);
    let mcp_sites_payload: Value =
        serde_json::from_str(mcp_sites["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_sites_payload["sites"].as_array().unwrap().len(), 3);

    let mcp_scoped_search = mcp::handle_rpc_line(
        &cfg,
        &default_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 102,
            "method": "tools/call",
            "params": {
                "name": "search_all",
                "arguments": { "query": "Gateway Migration", "site": "acme-cloud" }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_scoped_search["result"]["isError"], false);
    let mcp_search_payload: Value =
        serde_json::from_str(mcp_scoped_search["result"]["content"][0]["text"].as_str().unwrap())
            .unwrap();
    let mcp_hits = mcp_search_payload["hits"].as_array().unwrap();
    assert_eq!(mcp_hits.len(), 1);
    assert_eq!(mcp_hits[0]["site_id"], "acme-cloud");
    assert_eq!(mcp_hits[0]["id_or_key"], "ENG-1");
}
