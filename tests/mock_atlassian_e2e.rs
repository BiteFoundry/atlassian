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
    remotelink_requests: AtomicUsize,
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
            remotelink_requests: AtomicUsize::new(0),
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
            }),
        )
        .route(
            "/wiki/api/v2/footer-comments",
            post(|State(st): State<Arc<MockAtlassianState>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                if let Err(resp) = check_basic_auth(&headers, &st.cloud_email, &st.cloud_token) {
                    return resp;
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
                st.remotelink_requests.fetch_add(1, Ordering::SeqCst);
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
                ..Default::default()
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
        serve::ServeOptions {
            token: Some(api_token.to_string()),
            ..serve::ServeOptions::default()
        },
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
    assert!(tools.iter().any(|t| t["name"] == "move_confluence_page"));
    assert!(tools.iter().any(|t| t["name"] == "list_sites"));
    assert!(tools.iter().any(|t| t["name"] == "download_attachment"));

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
            "/repos/BiteFoundry/atlassian/releases/latest",
            get(move || {
                let gh_base = gh_base_clone.read().unwrap().clone();
                let asset = asset_name_owned.clone();
                let bin_len = bin_bytes.len() as u64;
                let sums_len = sums_clone.len() as u64;
                async move {
                    Json(json!({
                        "tag_name": "v9.9.9",
                        "html_url": format!("{gh_base}/BiteFoundry/atlassian/releases/tag/v9.9.9"),
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
        "BiteFoundry/atlassian",
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
        "BiteFoundry/atlassian",
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
        confluence_account_id: None,
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
    let api_router = serve::build_router(cfg.clone(), default_client.clone(), storage.clone(), serve::ServeOptions::default());
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

#[tokio::test]
async fn e2e_tui_request_issue_detail_fetches_remotelink_once_across_cache_miss_and_reopen() {
    use atlassian_cli::{
        config::Persona,
        sync,
        ui::app::{AppState, UiMsg, UiState},
    };

    ensure_isolated_env();
    let state = MockAtlassianState::new("alice@acme.com", "cloud-api-tok-123", "dc-pat");
    {
        let mut issues = state.issues.write().unwrap();
        let rec = issues.get_mut("PROJ-101").unwrap();
        rec.description_cloud_adf = json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Cached v1 body — see https://acme.atlassian.net/wiki/spaces/ENG/pages/2002/Rollback"
                }]
            }]
        });
    }
    let base_url = spawn_server(build_mock_atlassian_router(state.clone())).await;
    *state.base_url.write().unwrap() = base_url.clone();

    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let auth = AuthManager::with_credentials(
        "tui-detail-e2e",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-api-tok-123".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth.clone())
        .expect("build cloud client");
    let storage = Storage::open_in_memory().expect("open in-memory storage");
    let (core_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), core_tx);
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut app = AppState::new_with_persona(
        cfg.clone(),
        Persona::Jira,
        auth,
        client,
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    let mut ui = UiState::from_config_with_persona(&cfg, Persona::Jira);

    async fn next_issue_detail(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiMsg>,
    ) -> UiMsg {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(msg @ UiMsg::IssueDetailLoaded { .. }) => return msg,
                    Some(_) => continue,
                    None => panic!("ui_rx closed before IssueDetailLoaded"),
                }
            }
        })
        .await
        .expect("timed out waiting for IssueDetailLoaded")
    }

    // 1. Cache miss: active_issue stays empty until network load finishes with 1 /remotelink call.
    app.request_issue_detail(&mut ui, "PROJ-101");
    assert!(app.active_issue.is_none());

    let msg = next_issue_detail(&mut ui_rx).await;
    app.handle_ui_msg(&mut ui, msg);

    assert_eq!(state.remotelink_requests.load(Ordering::SeqCst), 1);
    let active = app.active_issue.as_ref().expect("active_issue populated");
    assert!(active.description_markdown.contains("Cached v1 body"));
    assert!(active.linked_pages.iter().any(|p| p.page_id == "2001" && p.title == "Release Guide"));
    assert!(active.linked_pages.iter().any(|p| p.page_id == "2002"));
    let cached = storage.get_issue_detail("PROJ-101").unwrap().expect("cached detail");
    assert_eq!(cached.linked_pages.len(), 2);

    // 2. Cache hit reopen: cached v1 shows immediately, then fresh v2 replaces it with 1 more /remotelink call.
    {
        let mut issues = state.issues.write().unwrap();
        let rec = issues.get_mut("PROJ-101").unwrap();
        rec.description_cloud_adf = json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "paragraph",
                "content": [{
                    "type": "text",
                    "text": "Fresh v2 body — see https://acme.atlassian.net/wiki/spaces/ENG/pages/2002/Rollback"
                }]
            }]
        });
    }
    app.active_issue = None;
    app.request_issue_detail(&mut ui, "PROJ-101");
    assert!(
        app.active_issue
            .as_ref()
            .expect("cached detail shown immediately")
            .description_markdown
            .contains("Cached v1 body")
    );

    let msg2 = next_issue_detail(&mut ui_rx).await;
    app.handle_ui_msg(&mut ui, msg2);

    assert_eq!(state.remotelink_requests.load(Ordering::SeqCst), 2);
    let refreshed = app.active_issue.as_ref().expect("refreshed active_issue");
    assert!(refreshed.description_markdown.contains("Fresh v2 body"));
    assert!(refreshed.linked_pages.iter().any(|p| p.page_id == "2001"));
    assert!(refreshed.linked_pages.iter().any(|p| p.page_id == "2002"));
    assert!(
        storage
            .get_issue_detail("PROJ-101")
            .unwrap()
            .expect("updated cache")
            .description_markdown
            .contains("Fresh v2 body")
    );
}

#[tokio::test]
async fn regression_atlassian_bug_01_cloud_footer_comment_endpoint_and_controls() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug01MockState {
        cloud_post_hits: Vec<Value>,
        cloud_wrong_post_hits: usize,
        cloud_get_hits: usize,
        cloud_comments: Vec<Value>,
        dc_post_hits: Vec<Value>,
    }

    let st = Arc::new(Mutex::new(Bug01MockState {
        cloud_comments: vec![json!({
            "id": "cm-existing",
            "version": { "authorId": "acc-alice", "createdAt": "2026-11-15T12:00:00Z" },
            "body": {
                "atlas_doc_format": {
                    "representation": "atlas_doc_format",
                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Existing comment\"}]}]}"
                }
            }
        })],
        ..Default::default()
    }));

    let router = Router::new()
        .route(
            "/wiki/api/v2/pages/200",
            get(|| async {
                Json(json!({
                    "id": "200",
                    "spaceId": "5001",
                    "spaceKey": "ENG",
                    "title": "Page 200",
                    "status": "current",
                    "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z", "authorId": "acc-alice" },
                    "body": {
                        "atlas_doc_format": {
                            "representation": "atlas_doc_format",
                            "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Body\"}]}]}"
                        }
                    },
                    "_links": { "webui": "/spaces/ENG/pages/200" }
                }))
            }),
        )
        .route(
            "/wiki/api/v2/pages/200/labels",
            get(|| async { Json(json!({ "results": [] })) }),
        )
        .route(
            "/wiki/api/v2/pages/200/attachments",
            get(|| async { Json(json!({ "results": [] })) }),
        )
        .route(
            "/wiki/api/v2/pages/200/footer-comments",
            get({
                let st = st.clone();
                move || {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.cloud_get_hits += 1;
                        Json(json!({ "results": g.cloud_comments.clone() }))
                    }
                }
            })
            .post({
                let st = st.clone();
                move || {
                    let st = st.clone();
                    async move {
                        st.lock().unwrap().cloud_wrong_post_hits += 1;
                        (
                            StatusCode::METHOD_NOT_ALLOWED,
                            Json(json!({ "message": "POST not supported on /pages/{id}/footer-comments" })),
                        )
                    }
                }
            }),
        )
        .route(
            "/wiki/api/v2/footer-comments",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.cloud_post_hits.push(body.clone());
                        let cid = format!("cm-cloud-{}", g.cloud_post_hits.len());
                        let comment = json!({
                            "id": cid,
                            "pageId": body.get("pageId").cloned().unwrap_or(Value::Null),
                            "version": {
                                "number": 1,
                                "authorId": "acc-alice",
                                "createdAt": "2026-11-15T12:10:00Z"
                            },
                            "body": {
                                "atlas_doc_format": body.get("body").cloned().unwrap_or(Value::Null)
                            }
                        });
                        g.cloud_comments.push(comment.clone());
                        (StatusCode::CREATED, Json(comment))
                    }
                }
            }),
        )
        .route(
            "/rest/api/content",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let mut g = st.lock().unwrap();
                        g.dc_post_hits.push(body.clone());
                        let cid = format!("cm-dc-{}", g.dc_post_hits.len());
                        Json(json!({
                            "id": cid,
                            "type": "comment",
                            "version": {
                                "number": 1,
                                "when": "2026-11-15T12:20:00Z",
                                "by": { "displayName": "Alice DC" }
                            },
                            "body": body.get("body").cloned().unwrap_or(Value::Null)
                        }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let cloud_auth = AuthManager::with_credentials(
        "bug01-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "cloud-tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth)
            .expect("cloud client");
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().expect("storage");

    // 1. CLI entry point
    let profile = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_atlassian");
    let cli_out = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "cloud-tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["wiki", "comment", "200", "CLI comment body", "--json"])
        .output()
        .await
        .expect("run CLI wiki comment");
    assert!(
        cli_out.status.success(),
        "CLI wiki comment failed: {}",
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let cli_json: Value = serde_json::from_slice(&cli_out.stdout).expect("CLI JSON");
    assert_eq!(cli_json["comment"]["id"], "cm-cloud-1");

    // 2. MCP entry point
    let mcp_resp = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "add_confluence_comment",
                "arguments": { "page_id": "200", "body_markdown": "MCP comment body" }
            }
        })
        .to_string(),
    )
    .await
    .expect("mcp rpc");
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    let mcp_payload: Value =
        serde_json::from_str(mcp_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(mcp_payload["comment"]["id"], "cm-cloud-2");

    // 3. Local HTTP entry point
    let api_router = serve::build_router(cfg.clone(), cloud_client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();
    let http_resp = http
        .post(format!("{api_base}/pages/200/comments"))
        .json(&json!({ "body_markdown": "HTTP comment body" }))
        .send()
        .await
        .expect("http post comment");
    assert_eq!(http_resp.status(), StatusCode::OK);
    let http_json: Value = http_resp.json().await.expect("http json");
    assert_eq!(http_json["comment"]["id"], "cm-cloud-3");

    // Verify all 3 Cloud POST requests hit /wiki/api/v2/footer-comments with top-level string pageId="200"
    {
        let g = st.lock().unwrap();
        assert_eq!(
            g.cloud_wrong_post_hits, 0,
            "must not POST to /wiki/api/v2/pages/200/footer-comments"
        );
        assert_eq!(g.cloud_post_hits.len(), 3);
        let expected_texts = ["CLI comment body", "MCP comment body", "HTTP comment body"];
        for (req, expected_text) in g.cloud_post_hits.iter().zip(expected_texts.iter()) {
            assert_eq!(req["pageId"], json!("200"), "payload: {req}");
            assert_eq!(req["body"]["representation"], "atlas_doc_format");
            let adf_str = req["body"]["value"].as_str().expect("stringified ADF");
            let adf: Value = serde_json::from_str(adf_str).expect("valid ADF JSON");
            assert!(
                adf.to_string().contains(expected_text),
                "expected '{expected_text}' in ADF: {adf}"
            );
        }
    }

    // 4. Regression control: GET /wiki/api/v2/pages/200/footer-comments still works
    let listed = cloud_client
        .list_page_comments("200")
        .await
        .expect("list_page_comments");
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0].id, "cm-existing");
    assert_eq!(listed[3].id, "cm-cloud-3");
    assert!(st.lock().unwrap().cloud_get_hits >= 1);

    // 5. Regression control: Data Center add_page_comment still uses POST /rest/api/content
    let dc_auth = AuthManager::with_credentials(
        "bug01-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .expect("dc client");
    let dc_comment = dc_client
        .add_page_comment("200", "DC comment body")
        .await
        .expect("dc add_page_comment");
    assert_eq!(dc_comment.id, "cm-dc-1");
    let dc_hits = &st.lock().unwrap().dc_post_hits;
    assert_eq!(dc_hits.len(), 1);
    assert_eq!(dc_hits[0]["type"], "comment");
    assert_eq!(dc_hits[0]["container"]["id"], "200");
    assert_eq!(dc_hits[0]["container"]["type"], "page");
    assert_eq!(dc_hits[0]["body"]["storage"]["representation"], "storage");
    assert!(dc_hits[0]["body"]["storage"]["value"]
        .as_str()
        .unwrap()
        .contains("DC comment body"));
}

#[tokio::test]
async fn regression_atlassian_bug_04_plain_text_vs_explicit_jql_cql() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug04State {
        jql_sent: Vec<String>,
        cql_sent: Vec<String>,
    }

    let st = Arc::new(Mutex::new(Bug04State::default()));
    let router = Router::new()
        .route(
            "/rest/api/3/search/jql",
            post({
                let st = st.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    async move {
                        let jql = body["jql"].as_str().unwrap_or("").to_string();
                        st.lock().unwrap().jql_sent.push(jql);
                        Json(json!({ "issues": [], "total": 0 }))
                    }
                }
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get({
                let st = st.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let st = st.clone();
                    async move {
                        let cql = q.get("cql").cloned().unwrap_or_default();
                        st.lock().unwrap().cql_sent.push(cql);
                        Json(json!({ "results": [] }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let auth = AuthManager::with_credentials(
        "bug04-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .expect("client");
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().expect("storage");
    let api_router = serve::build_router(cfg.clone(), client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();
    let profile = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_atlassian");

    let plain_inputs = ["read and write", "alpha or beta", "timeout=30", "~user"];

    // 1. MCP search_jira_issues with project="ENG" -> all 4 plain-text queries stay inside text ~ "..." AND keep project = "ENG"
    for q in plain_inputs {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "search_jira_issues",
                    "arguments": { "query": q, "project": "ENG" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let last_jql = st.lock().unwrap().jql_sent.last().unwrap().clone();
        assert!(
            last_jql.contains("project = \"ENG\"") && last_jql.contains(&format!("text ~ \"{q}\"")),
            "MCP Jira query '{q}' must wrap text and keep project=ENG; got: {last_jql}"
        );
    }

    // 2. MCP search_confluence_pages & HTTP /search?remote=true -> all 4 plain-text queries stay inside text ~ "..."
    for q in plain_inputs {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "search_confluence_pages",
                    "arguments": { "query": q, "remote": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let last_cql = st.lock().unwrap().cql_sent.last().unwrap().clone();
        assert_eq!(
            last_cql,
            format!("text ~ \"{q}\""),
            "MCP CQL query '{q}' must wrap in text ~"
        );

        let encoded = urlencoding::encode(q);
        let http_res = http
            .get(format!("{api_base}/search?q={encoded}&scope=all&remote=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(http_res.status(), StatusCode::OK);
        let last_jql = st.lock().unwrap().jql_sent.last().unwrap().clone();
        let last_cql = st.lock().unwrap().cql_sent.last().unwrap().clone();
        assert!(
            last_jql.contains(&format!("text ~ \"{q}\"")),
            "HTTP Jira query '{q}' must wrap in text ~; got: {last_jql}"
        );
        assert_eq!(
            last_cql,
            format!("text ~ \"{q}\""),
            "HTTP CQL query '{q}' must wrap in text ~"
        );
    }

    // 3. Compatible raw expressions pass through untouched
    for raw_jql in [
        "project = FLS",
        "type = page",
        "status IN (\"To Do\", \"In Progress\")",
        "cf[10016] >= 5",
        "\"Story Points\" = 3",
        "ORDER BY updated DESC",
    ] {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "search_jira_issues",
                    "arguments": { "query": raw_jql }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(
            st.lock().unwrap().jql_sent.last().unwrap(),
            raw_jql,
            "compatible JQL '{raw_jql}' should pass through"
        );
    }
    for raw_cql in ["space = ENG", "type = page", "label NOT IN (archive)"] {
        let resp = mcp::handle_rpc_line(
            &cfg,
            &client,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "search_confluence_pages",
                    "arguments": { "query": raw_cql, "remote": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        assert_eq!(
            st.lock().unwrap().cql_sent.last().unwrap(),
            raw_cql,
            "compatible CQL '{raw_cql}' should pass through"
        );
    }

    // 4. Regression control: CLI --jql / --cql preserve custom fields & quoted and/or verbatim
    let custom_jql = "customFieldAlias = \"read and write\"";
    let cli_jql = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["search", custom_jql, "--jql", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_jql.status.success(), "{}", String::from_utf8_lossy(&cli_jql.stderr));
    assert_eq!(st.lock().unwrap().jql_sent.last().unwrap(), custom_jql);

    let custom_cql = "customMeta = \"alpha or beta\"";
    let cli_cql = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["wiki", "search", custom_cql, "--cql", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_cql.status.success(), "{}", String::from_utf8_lossy(&cli_cql.stderr));
    assert_eq!(st.lock().unwrap().cql_sent.last().unwrap(), custom_cql);

    // CLI plain-text remote search wraps 'read and write' in text ~ "..."
    let cli_plain = tokio::process::Command::new(bin)
        .current_dir(profile.path())
        .env("ATLASSIAN_CONFIG_DIR", profile.path())
        .env("ATLASSIAN_DISABLE_KEYRING", "1")
        .env("ATLASSIAN_SITE_URL", &base_url)
        .env("ATLASSIAN_CONFLUENCE_URL", format!("{base_url}/wiki"))
        .env("ATLASSIAN_DEPLOYMENT", "cloud")
        .env("ATLASSIAN_EMAIL", "alice@acme.com")
        .env("ATLASSIAN_API_TOKEN", "tok")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["search", "read and write", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(cli_plain.status.success(), "{}", String::from_utf8_lossy(&cli_plain.stderr));
    assert!(st
        .lock()
        .unwrap()
        .jql_sent
        .last()
        .unwrap()
        .contains("text ~ \"read and write\""));
}

#[tokio::test]
async fn regression_atlassian_bug_05_dc_jira_search_start_at_pagination() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct Bug05State {
        dc_start_ats: Vec<usize>,
        cloud_tokens: Vec<Option<String>>,
    }

    let st = Arc::new(Mutex::new(Bug05State::default()));
    let all_dc_issues: Vec<Value> = (1..=7)
        .map(|i| {
            json!({
                "id": format!("1000{i}"),
                "key": format!("DC-{i}"),
                "fields": {
                    "project": { "key": "DC", "name": "DC Project" },
                    "summary": format!("DC Issue {i}"),
                    "status": { "id": "10", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "created": "2026-11-15T08:00:00.000+0000",
                    "updated": format!("2026-11-15T10:0{i}:00.000+0000")
                }
            })
        })
        .collect();

    let router = Router::new()
        .route(
            "/rest/api/2/search",
            post({
                let st = st.clone();
                let issues = all_dc_issues.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    let issues = issues.clone();
                    async move {
                        let start_at = body["startAt"].as_u64().unwrap_or(0) as usize;
                        let max_results = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        st.lock().unwrap().dc_start_ats.push(start_at);
                        let slice: Vec<Value> = issues
                            .iter()
                            .skip(start_at)
                            .take(max_results)
                            .cloned()
                            .collect();
                        Json(json!({
                            "startAt": start_at,
                            "maxResults": max_results,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            post({
                let st = st.clone();
                let issues = all_dc_issues.clone();
                move |Json(body): Json<Value>| {
                    let st = st.clone();
                    let issues = issues.clone();
                    async move {
                        let tok = body
                            .get("nextPageToken")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        st.lock().unwrap().cloud_tokens.push(tok.clone());
                        if tok.is_none() {
                            Json(json!({
                                "issues": issues[0..2].to_vec(),
                                "nextPageToken": "tok-page-2",
                                "total": 3
                            }))
                        } else {
                            Json(json!({
                                "issues": issues[2..3].to_vec(),
                                "total": 3
                            }))
                        }
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let dc_auth = AuthManager::with_credentials(
        "bug05-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .unwrap();
    let dc_cfg = AppConfig {
        site_url: base_url.clone(),
        confluence_url: base_url.clone(),
        deployment: DeploymentKind::DataCenter,
        ..Default::default()
    };
    let dc_storage = Storage::open_in_memory().unwrap();

    // 1. MCP DC pagination: 0 -> 3 -> 6 with limit 3 over 7 issues (3/3/1, 0 duplicates, last next_start_at=null)
    let mut mcp_keys = Vec::new();
    let mut next_start: Option<u64> = None;
    let mut page_sizes = Vec::new();
    for step in 0..3 {
        let mut args = json!({ "jql": "project = DC", "limit": 3 });
        if let Some(sa) = next_start {
            args["start_at"] = json!(sa);
        }
        let resp = mcp::handle_rpc_line(
            &dc_cfg,
            &dc_client,
            &dc_storage,
            &json!({
                "jsonrpc": "2.0",
                "id": step + 1,
                "method": "tools/call",
                "params": { "name": "search_jira_issues", "arguments": args }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "MCP page {step} failed: {resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        let issues = payload["issues"].as_array().unwrap();
        page_sizes.push(issues.len());
        for i in issues {
            mcp_keys.push(i["key"].as_str().unwrap().to_string());
        }
        next_start = payload["next_start_at"].as_u64();
        if step < 2 {
            assert_eq!(next_start, Some(((step + 1) * 3) as u64));
        } else {
            assert_eq!(payload["next_start_at"], Value::Null);
        }
    }
    assert_eq!(page_sizes, vec![3, 3, 1]);
    assert_eq!(mcp_keys, vec!["DC-1", "DC-2", "DC-3", "DC-4", "DC-5", "DC-6", "DC-7"]);

    // 2. HTTP DC pagination: 0 -> 3 -> 6 with limit 3 over 7 issues (3/3/1, 0 duplicates, last next_start_at=null)
    let api_router = serve::build_router(dc_cfg.clone(), dc_client.clone(), dc_storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();

    let mut http_keys = Vec::new();
    let mut http_sizes = Vec::new();
    let mut next_start: Option<u64> = None;
    for step in 0..3 {
        let url = match next_start {
            Some(sa) => format!("{api_base}/issues?jql=project%20%3D%20DC&limit=3&start_at={sa}"),
            None => format!("{api_base}/issues?jql=project%20%3D%20DC&limit=3"),
        };
        let res = http.get(&url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let payload: Value = res.json().await.unwrap();
        let issues = payload["issues"].as_array().unwrap();
        http_sizes.push(issues.len());
        for i in issues {
            http_keys.push(i["key"].as_str().unwrap().to_string());
        }
        next_start = payload["next_start_at"].as_u64();
        if step < 2 {
            assert_eq!(next_start, Some(((step + 1) * 3) as u64));
        } else {
            assert_eq!(payload["next_start_at"], Value::Null);
        }
    }
    assert_eq!(http_sizes, vec![3, 3, 1]);
    assert_eq!(http_keys, vec!["DC-1", "DC-2", "DC-3", "DC-4", "DC-5", "DC-6", "DC-7"]);
    assert_eq!(st.lock().unwrap().dc_start_ats, vec![0, 3, 6, 0, 3, 6]);

    // 3. Invalid inputs (negative, wrong type, conflicting cursors) -> MCP input error / HTTP 400 with 0 Jira calls
    let hits_before = st.lock().unwrap().dc_start_ats.len();
    for bad_args in [
        json!({ "jql": "project = DC", "start_at": -1 }),
        json!({ "jql": "project = DC", "start_at": "3" }),
        json!({ "jql": "project = DC", "start_at": 3, "page_token": "tok-1" }),
    ] {
        let resp = mcp::handle_rpc_line(
            &dc_cfg,
            &dc_client,
            &dc_storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": { "name": "search_jira_issues", "arguments": bad_args }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], true, "expected MCP input error for {bad_args}: {resp}");
    }
    for bad_url in [
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=-1"),
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=abc"),
        format!("{api_base}/issues?jql=project%20%3D%20DC&start_at=3&page_token=tok-1"),
    ] {
        let res = http.get(&bad_url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "expected HTTP 400 for {bad_url}");
    }
    assert_eq!(
        st.lock().unwrap().dc_start_ats.len(),
        hits_before,
        "invalid cursor inputs must make 0 Jira requests"
    );

    // 4. Regression control: Cloud page_token roundtrip and first-page shape remain unchanged
    let cloud_auth = AuthManager::with_credentials(
        "bug05-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth).unwrap();
    let cloud_cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let cloud_storage = Storage::open_in_memory().unwrap();
    let p1 = mcp::handle_rpc_line(
        &cloud_cfg,
        &cloud_client,
        &cloud_storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "search_jira_issues", "arguments": { "jql": "project = DC", "limit": 2 } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(p1["result"]["isError"], false);
    let p1_val: Value =
        serde_json::from_str(p1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p1_val["next_page_token"], "tok-page-2");
    assert_eq!(p1_val["next_start_at"], Value::Null);

    let p2 = mcp::handle_rpc_line(
        &cloud_cfg,
        &cloud_client,
        &cloud_storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "search_jira_issues", "arguments": { "jql": "project = DC", "limit": 2, "page_token": "tok-page-2" } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(p2["result"]["isError"], false);
    let p2_val: Value =
        serde_json::from_str(p2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(p2_val["issues"].as_array().unwrap().len(), 1);
    assert_eq!(p2_val["next_page_token"], Value::Null);
    assert_eq!(
        st.lock().unwrap().cloud_tokens,
        vec![None, Some("tok-page-2".to_string())]
    );
}

#[tokio::test]
async fn regression_atlassian_bug_09_remote_unified_search_global_ordering() {
    // 5 Jira issues + 3 Confluence pages with interleaved timestamps AND a timestamp tie
    // Expected global order (updated_at DESC, site_id ASC, id_or_key ASC):
    //   1. "P-900" (2026-11-20T12:00:00Z)  [Confluence]
    //   2. "ENG-1" (2026-11-20T11:30:00Z)  [Jira]
    //   3. "ENG-2" (2026-11-20T11:00:00Z)  [Jira, tied with P-800 -> "ENG-2" < "P-800"]
    //   4. "P-800" (2026-11-20T11:00:00Z)  [Confluence, tied with ENG-2]
    //   5. "ENG-3" (2026-11-20T10:30:00Z)  [Jira]
    //   6. "P-700" (2026-11-20T10:00:00Z)  [Confluence]
    //   7. "ENG-4" (2026-11-20T09:30:00Z)  [Jira]
    //   8. "ENG-5" (2026-11-20T09:00:00Z)  [Jira]
    let jira_items = vec![
        ("ENG-1", "2026-11-20T11:30:00.000+0000"),
        ("ENG-2", "2026-11-20T11:00:00.000+0000"),
        ("ENG-3", "2026-11-20T10:30:00.000+0000"),
        ("ENG-4", "2026-11-20T09:30:00.000+0000"),
        ("ENG-5", "2026-11-20T09:00:00.000+0000"),
    ];
    let conf_items = vec![
        ("P-900", "2026-11-20T12:00:00Z"),
        ("P-800", "2026-11-20T11:00:00Z"),
        ("P-700", "2026-11-20T10:00:00Z"),
    ];

    let jira_all_newer = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = Router::new()
        .route(
            "/rest/api/3/search/jql",
            post({
                let jira_all_newer = jira_all_newer.clone();
                move |Json(body): Json<Value>| {
                    let all_newer = jira_all_newer.load(Ordering::SeqCst);
                    let items = jira_items.clone();
                    async move {
                        let max_results = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let issues: Vec<Value> = items
                            .iter()
                            .take(max_results)
                            .enumerate()
                            .map(|(idx, (key, ts))| {
                                let updated = if all_newer {
                                    format!("2026-11-21T1{}:00:00.000+0000", 9 - idx)
                                } else {
                                    (*ts).to_string()
                                };
                                json!({
                                    "id": format!("j-{idx}"),
                                    "key": key,
                                    "fields": {
                                        "project": { "key": "ENG", "name": "Engineering" },
                                        "summary": format!("Rollout {key}"),
                                        "status": { "id": "10", "name": "Open", "statusCategory": { "key": "new" } },
                                        "issuetype": { "id": "1", "name": "Task", "subtask": false },
                                        "created": "2026-11-01T00:00:00.000+0000",
                                        "updated": updated
                                    }
                                })
                            })
                            .collect();
                        Json(json!({ "issues": issues, "total": issues.len() }))
                    }
                }
            }),
        )
        .route(
            "/wiki/rest/api/search",
            get({
                move |Query(q): Query<HashMap<String, String>>| {
                    let items = conf_items.clone();
                    async move {
                        let limit = q
                            .get("limit")
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(50);
                        let results: Vec<Value> = items
                            .iter()
                            .take(limit)
                            .map(|(id, ts)| {
                                json!({
                                    "content": {
                                        "id": id,
                                        "type": "page",
                                        "title": format!("Rollout Doc {id}"),
                                        "space": { "id": 5001, "key": "ENG" },
                                        "version": { "number": 1, "when": ts },
                                        "_links": { "webui": format!("/spaces/ENG/pages/{id}") }
                                    },
                                    "lastModified": ts
                                })
                            })
                            .collect();
                        Json(json!({ "results": results }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;
    let auth = AuthManager::with_credentials(
        "bug09-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let client = AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth)
        .unwrap();
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().unwrap();
    let api_router = serve::build_router(cfg.clone(), client.clone(), storage.clone(), serve::ServeOptions::default());
    let api_base = spawn_server(api_router).await;
    let http = reqwest::Client::new();

    // 1. Interleaved Jira + Confluence with limit=5 -> exact top 5 across MCP & HTTP including tie-break
    let expected_top5 = vec!["P-900", "ENG-1", "ENG-2", "P-800", "ENG-3"];

    let mcp_resp = mcp::handle_rpc_line(
        &cfg,
        &client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "search_all",
                "arguments": { "query": "Rollout", "scope": "all", "remote": true, "limit": 5 }
            }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    let mcp_payload: Value =
        serde_json::from_str(mcp_resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let mcp_ids: Vec<&str> = mcp_payload["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(mcp_ids, expected_top5, "MCP remote unified search order mismatch");

    let http_payload: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=all&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let http_ids: Vec<&str> = http_payload["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(http_ids, expected_top5, "HTTP remote unified search order mismatch");

    // 2. Regression control: single-service scope still returns only that service
    let http_conf_only: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=confluence&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let conf_ids: Vec<&str> = http_conf_only["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(conf_ids, vec!["P-900", "P-800", "P-700"]);

    // 3. Regression control: when all Jira items are strictly newer, top 5 are all Jira (no artificial quota)
    jira_all_newer.store(true, Ordering::SeqCst);
    let http_all_jira: Value = http
        .get(format!("{api_base}/search?q=Rollout&scope=all&remote=true&limit=5"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let all_jira_ids: Vec<&str> = http_all_jira["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["id_or_key"].as_str())
        .collect();
    assert_eq!(all_jira_ids, vec!["ENG-1", "ENG-2", "ENG-3", "ENG-4", "ENG-5"]);
}

#[tokio::test]
async fn regression_atlassian_bug_14_cloud_page_ancestors_and_controls() {
    use std::sync::{atomic::AtomicBool, Mutex};

    #[derive(Default)]
    struct Bug14State {
        requests: Vec<String>,
        fail_ancestors_300: bool,
    }

    let st = Arc::new(Mutex::new(Bug14State::default()));
    let deep_fail = Arc::new(AtomicBool::new(false));
    let router = Router::new().fallback({
        let st = st.clone();
        let _deep_fail = deep_fail.clone();
        move |uri: axum::http::Uri| {
            let st = st.clone();
            async move {
                let path = uri.path().to_string();
                let full = uri.to_string();
                st.lock().unwrap().requests.push(full);

                // Root page 100 (no parentId)
                if path == "/wiki/api/v2/pages/100" {
                    return Json(json!({
                        "id": "100",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": Value::Null,
                        "status": "current",
                        "title": "Root Guide",
                        "version": { "number": 1, "createdAt": "2026-11-15T08:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Root\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/100" }
                    }))
                    .into_response();
                }
                // Parent page 200 (parentId=100)
                if path == "/wiki/api/v2/pages/200" {
                    return Json(json!({
                        "id": "200",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "100",
                        "status": "current",
                        "title": "Parent Section",
                        "version": { "number": 1, "createdAt": "2026-11-15T09:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Parent\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/200" }
                    }))
                    .into_response();
                }
                // Child page 300 (parentId=200)
                if path == "/wiki/api/v2/pages/300" {
                    return Json(json!({
                        "id": "300",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "200",
                        "status": "current",
                        "title": "Child Spec",
                        "version": { "number": 2, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Child\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/300" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/300/ancestors" {
                    if st.lock().unwrap().fail_ancestors_300 {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "message": "temporary ancestor error" })),
                        )
                            .into_response();
                    }
                    // Official v2 response: only id & type (no title!)
                    return Json(json!({
                        "results": [
                            { "id": "100", "type": "page" },
                            { "id": "200", "type": "page" }
                        ]
                    }))
                    .into_response();
                }

                // Forbidden ancestor test: page 400 has ancestors [401 (forbidden folder), 402 (readable folder)]
                if path == "/wiki/api/v2/pages/400" {
                    return Json(json!({
                        "id": "400",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "402",
                        "status": "current",
                        "title": "Restricted Child",
                        "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/400" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/400/ancestors" {
                    return Json(json!({
                        "results": [
                            { "id": "401", "type": "folder" },
                            { "id": "402", "type": "folder" }
                        ]
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/folders/401" {
                    return (StatusCode::FORBIDDEN, Json(json!({ "message": "Forbidden" }))).into_response();
                }
                if path == "/wiki/api/v2/folders/402" {
                    return Json(json!({ "id": "402", "title": "Team Folder" })).into_response();
                }

                // Deep tree (> 250 ancestors): page 900 -> 250 ancestors [d-2..d-251], then d-2 -> [d-0, d-1]
                if path == "/wiki/api/v2/pages/900" {
                    return Json(json!({
                        "id": "900",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "d-251",
                        "status": "current",
                        "title": "Deep Leaf",
                        "version": { "number": 1, "createdAt": "2026-11-15T10:00:00Z" },
                        "body": { "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/900" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/900/ancestors" {
                    let results: Vec<Value> = (2..=251)
                        .map(|n| json!({ "id": format!("d-{n}"), "type": "page" }))
                        .collect();
                    return Json(json!({ "results": results })).into_response();
                }
                if path == "/wiki/api/v2/pages/d-2/ancestors" {
                    return Json(json!({
                        "results": [
                            { "id": "d-0", "type": "page" },
                            { "id": "d-1", "type": "page" }
                        ]
                    }))
                    .into_response();
                }
                if let Some(did) = path.strip_prefix("/wiki/api/v2/pages/d-") {
                    if !did.contains('/') {
                        return Json(json!({ "id": format!("d-{did}"), "title": format!("Level {did}") }))
                            .into_response();
                    }
                }

                // DC control page 501
                if path == "/rest/api/content/501" {
                    return Json(json!({
                        "id": "501",
                        "type": "page",
                        "title": "DC Child",
                        "space": { "id": 1, "key": "ENG" },
                        "version": { "number": 1, "when": "2026-11-15T10:00:00Z" },
                        "ancestors": [
                            { "id": "500", "title": "DC Root" }
                        ],
                        "body": { "storage": { "value": "<p>DC</p>" } },
                        "metadata": { "labels": { "results": [] } },
                        "children": { "attachment": { "results": [] } }
                    }))
                    .into_response();
                }
                if path.ends_with("/labels")
                    || path.ends_with("/attachments")
                    || path.ends_with("/footer-comments")
                    || path.ends_with("/child/comment")
                {
                    return Json(json!({ "results": [] })).into_response();
                }
                (StatusCode::NOT_FOUND, format!("Unhandled {path}")).into_response()
            }
        }
    });

    let base_url = spawn_server(router).await;
    let cloud_auth = AuthManager::with_credentials(
        "bug14-cloud",
        StoredCredentials::ApiToken {
            site_url: base_url.clone(),
            email: "alice@acme.com".into(),
            api_token: "tok".into(),
            deployment: DeploymentKind::Cloud,
        },
    );
    let cloud_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, cloud_auth).unwrap();
    let cfg = AppConfig {
        site_url: base_url.clone(),
        deployment: DeploymentKind::Cloud,
        ..Default::default()
    };
    let storage = Storage::open_in_memory().unwrap();

    // 1. Root page 100 -> empty ancestors, 0 /ancestors calls
    let root_detail = cloud_client.get_page("100").await.unwrap();
    assert!(root_detail.ancestors.is_empty());
    assert!(
        !st.lock().unwrap().requests.iter().any(|r| r.contains("/100/ancestors")),
        "root page must not query /ancestors"
    );
    storage.upsert_page_detail(&root_detail).unwrap();

    // 2. Child page 300 via MCP get_confluence_page (compact=false):
    //    100 is cached (no extra GET /pages/100!), 200 is fetched metadata-only (no /200/footer-comments or /200/attachments!)
    st.lock().unwrap().requests.clear();
    let mcp_child = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "300", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_child["result"]["isError"], false, "{mcp_child}");
    let child_payload: Value =
        serde_json::from_str(mcp_child["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let ancestors = child_payload["page"]["ancestors"].as_array().unwrap();
    assert_eq!(
        ancestors,
        &vec![
            json!({ "id": "100", "title": "Root Guide" }),
            json!({ "id": "200", "title": "Parent Section" }),
        ],
        "child ancestors must be ordered root -> parent with resolved titles"
    );
    {
        let reqs = &st.lock().unwrap().requests;
        assert!(
            !reqs.iter().any(|r| r.contains("/pages/100")),
            "cached ancestor 100 should use site cache first: {reqs:?}"
        );
        assert!(
            !reqs.iter().any(|r| r.contains("/pages/200/footer-comments") || r.contains("/pages/200/attachments")),
            "ancestor title resolution must not recursively fetch comments/attachments: {reqs:?}"
        );
    }

    // Offline re-read returns the exact same ancestors
    let offline_child = storage.get_page_detail("300").unwrap().unwrap();
    assert_eq!(offline_child.ancestors.len(), 2);
    assert_eq!(offline_child.ancestors[0].id, "100");
    assert_eq!(offline_child.ancestors[0].title, "Root Guide");
    assert_eq!(offline_child.ancestors[1].id, "200");
    assert_eq!(offline_child.ancestors[1].title, "Parent Section");

    // 3. Ancestor fetch failure on refresh must NOT overwrite cached ancestors with []
    st.lock().unwrap().fail_ancestors_300 = true;
    let mcp_refresh = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "300", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_refresh["result"]["isError"], false);
    let preserved = storage.get_page_detail("300").unwrap().unwrap();
    assert_eq!(
        preserved.ancestors.len(),
        2,
        "failed /ancestors read must preserve previously cached ancestor path"
    );

    // 4. Forbidden ancestor metadata falls back to displaying ID as title, while readable folder gets its title
    let detail_400 = cloud_client.get_page("400").await.unwrap();
    assert_eq!(detail_400.ancestors.len(), 2);
    assert_eq!(detail_400.ancestors[0].id, "401");
    assert_eq!(detail_400.ancestors[0].title, "401");
    assert_eq!(detail_400.ancestors[1].id, "402");
    assert_eq!(detail_400.ancestors[1].title, "Team Folder");

    // 5. Deep tree (>250 ancestors): merges upper segment in front without duplicates
    // Pre-cache d-0..d-251 titles in storage to keep the deep-tree test super fast and verify cache lookup
    let cached_deep: Vec<atlassian_cli::models::PageSummary> = (0..=251)
        .map(|n| atlassian_cli::models::PageSummary {
            id: format!("d-{n}"),
            space_id: "5001".into(),
            space_key: "ENG".into(),
            title: format!("Level {n}"),
            version_number: 1,
            ..Default::default()
        })
        .collect();
    storage.upsert_pages(&cached_deep).unwrap();
    let mcp_deep = mcp::handle_rpc_line(
        &cfg,
        &cloud_client,
        &storage,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_confluence_page", "arguments": { "page_id": "900", "compact": false } }
        })
        .to_string(),
    )
    .await
    .unwrap();
    assert_eq!(mcp_deep["result"]["isError"], false, "{mcp_deep}");
    let deep_payload: Value =
        serde_json::from_str(mcp_deep["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let deep_ancestors = deep_payload["page"]["ancestors"].as_array().unwrap();
    assert_eq!(deep_ancestors.len(), 252, "expected 252 unique ancestors (d-0..d-251)");
    assert_eq!(deep_ancestors[0], json!({ "id": "d-0", "title": "Level 0" }));
    assert_eq!(deep_ancestors[1], json!({ "id": "d-1", "title": "Level 1" }));
    assert_eq!(deep_ancestors[2], json!({ "id": "d-2", "title": "Level 2" }));
    assert_eq!(deep_ancestors[251], json!({ "id": "d-251", "title": "Level 251" }));

    // 6. Control: Data Center detail continues to preserve its ancestors
    let dc_auth = AuthManager::with_credentials(
        "bug14-dc",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "dc-pat".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let dc_client =
        AtlassianClient::with_base_urls(&base_url, &base_url, DeploymentKind::DataCenter, dc_auth)
            .unwrap();
    let dc_detail = dc_client.get_page("501").await.unwrap();
    assert_eq!(dc_detail.ancestors.len(), 1);
    assert_eq!(dc_detail.ancestors[0].id, "500");
    assert_eq!(dc_detail.ancestors[0].title, "DC Root");
}

#[tokio::test]
async fn regression_atlassian_05_collection_paging_and_controls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let req_count = Arc::new(AtomicUsize::new(0));
    let projects: Vec<Value> = (1..=150)
        .map(|i| {
            json!({
                "id": format!("{i}"),
                "key": format!("P{i:03}"),
                "name": format!("Project {i:03}"),
                "projectTypeKey": "software"
            })
        })
        .collect();
    let issues: Vec<Value> = (1..=120)
        .map(|i| {
            json!({
                "id": format!("{i}"),
                "key": format!("ENG-{i:03}"),
                "fields": {
                    "project": { "key": "ENG", "name": "Engineering" },
                    "summary": format!("Board Issue {i:03}"),
                    "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "updated": format!("2026-11-20T{:02}:{:02}:00Z", i / 60, i % 60)
                }
            })
        })
        .collect();

    let router = Router::new()
        .route(
            "/rest/api/3/project/search",
            get({
                let req_count = req_count.clone();
                let projects = projects.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    let projects = projects.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("startAt").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let max: usize = q.get("maxResults").and_then(|s| s.parse().ok()).unwrap_or(100);
                        let slice: Vec<Value> = projects.iter().skip(start).take(max).cloned().collect();
                        let is_last = start + slice.len() >= projects.len();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": projects.len(),
                            "isLast": is_last,
                            "values": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/project",
            get({
                let req_count = req_count.clone();
                let projects = projects.clone();
                move || {
                    let req_count = req_count.clone();
                    let projects = projects.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        Json(json!(projects))
                    }
                }
            }),
        )
        .route(
            "/rest/agile/1.0/board/7/issue",
            get({
                let req_count = req_count.clone();
                let issues = issues.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    let issues = issues.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("startAt").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let max: usize = q.get("maxResults").and_then(|s| s.parse().ok()).unwrap_or(50);
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        )
        .route(
            "/wiki/api/v2/pages/700/children",
            get({
                let req_count = req_count.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        if q.get("cursor").map(String::as_str) == Some("c2=") {
                            Json(json!({
                                "results": [
                                    { "id": "703", "spaceId": "77", "parentId": "700", "title": "Child 3", "status": "current", "version": { "number": 1 } }
                                ]
                            }))
                        } else {
                            Json(json!({
                                "results": [
                                    { "id": "701", "spaceId": "77", "parentId": "700", "title": "Child 1", "status": "current", "version": { "number": 1 } },
                                    { "id": "702", "spaceId": "77", "parentId": "700", "title": "Child 2", "status": "current", "version": { "number": 1 } }
                                ],
                                "_links": { "next": "/wiki/api/v2/pages/700/children?limit=2&cursor=c2%3D" }
                            }))
                        }
                    }
                }
            }),
        )
        .route(
            "/rest/api/content/700/child/page",
            get({
                let req_count = req_count.clone();
                move |Query(q): Query<HashMap<String, String>>| {
                    let req_count = req_count.clone();
                    async move {
                        req_count.fetch_add(1, Ordering::SeqCst);
                        let start: usize = q.get("start").and_then(|s| s.parse().ok()).unwrap_or(0);
                        if start == 0 {
                            Json(json!({
                                "results": [
                                    { "id": "701", "title": "Child 1", "space": { "key": "ENG" }, "version": { "number": 1 } },
                                    { "id": "702", "title": "Child 2", "space": { "key": "ENG" }, "version": { "number": 1 } }
                                ],
                                "_links": { "next": "/rest/api/content/700/child/page?limit=2&start=2" }
                            }))
                        } else {
                            Json(json!({
                                "results": [
                                    { "id": "703", "title": "Child 3", "space": { "key": "ENG" }, "version": { "number": 1 } }
                                ]
                            }))
                        }
                    }
                }
            }),
        )
        .route(
            "/rest/api/3/search/jql",
            post({
                let issues = issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body
                            .get("nextPageToken")
                            .and_then(Value::as_str)
                            .and_then(|t| t.strip_prefix("tok:"))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        let mut resp = json!({ "issues": slice });
                        if start + slice.len() < issues.len() {
                            resp["nextPageToken"] = json!(format!("tok:{}", start + slice.len()));
                        }
                        Json(resp)
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/search",
            post({
                let issues = issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body["startAt"].as_u64().unwrap_or(0) as usize;
                        let slice: Vec<Value> = issues.iter().skip(start).take(max).cloned().collect();
                        Json(json!({
                            "startAt": start,
                            "maxResults": max,
                            "total": issues.len(),
                            "issues": slice
                        }))
                    }
                }
            }),
        );

    let base_url = spawn_server(router).await;

    async fn call_mcp(
        cfg: &AppConfig,
        client: &AtlassianClient,
        storage: &Storage,
        name: &str,
        arguments: Value,
    ) -> (bool, Value) {
        let resp = mcp::handle_rpc_line(
            cfg,
            client,
            storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert!(
            resp["result"].get("structuredContent").is_none(),
            "MCP result must not add structuredContent"
        );
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        if is_error {
            (true, Value::String(text.to_string()))
        } else {
            (false, serde_json::from_str(text).unwrap())
        }
    }

    for deployment in [DeploymentKind::Cloud, DeploymentKind::DataCenter] {
        let conf_url = if deployment == DeploymentKind::Cloud {
            format!("{base_url}/wiki")
        } else {
            base_url.clone()
        };
        let auth = AuthManager::with_credentials(
            "paging-test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "tok".into(),
                deployment,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, &conf_url, deployment, auth).unwrap();
        let cfg = AppConfig {
            site_url: base_url.clone(),
            confluence_url: conf_url,
            deployment,
            ..Default::default()
        };
        let storage = Storage::open_in_memory().unwrap();

        // 1. get_board_issues: limit 500 -> 100 / true / "100", then cursor "100" -> 20 / false / null
        let (err, b1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 500 }),
        )
        .await;
        assert!(!err, "{b1}");
        assert_eq!(b1["returned"], 100);
        assert_eq!(b1["has_more"], true);
        assert_eq!(b1["next_cursor"], "100");
        assert_eq!(b1["issues"].as_array().unwrap().len(), 100);

        let (err, b2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 500, "cursor": "100" }),
        )
        .await;
        assert!(!err, "{b2}");
        assert_eq!(b2["returned"], 20);
        assert_eq!(b2["has_more"], false);
        assert!(b2["next_cursor"].is_null());

        // Offline continuation reads all 120 from cache in consistent order
        let (err, off1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 70, "offline": true }),
        )
        .await;
        assert!(!err, "{off1}");
        assert_eq!(off1["returned"], 70);
        assert_eq!(off1["has_more"], true);
        assert_eq!(off1["next_cursor"], "70");
        let (err, off2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "limit": 70, "cursor": "70", "offline": true }),
        )
        .await;
        assert!(!err, "{off2}");
        assert_eq!(off2["returned"], 50);
        assert_eq!(off2["has_more"], false);
        assert!(off2["next_cursor"].is_null());

        // 2. list_projects: refresh -> 50 / "50" -> 50 / "100" -> 50 / null (all 150 in both Cloud & DC)
        let mut recovered_projects = Vec::new();
        let proj_before = req_count.load(Ordering::SeqCst);
        let (err, pr1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "refresh": true }),
        )
        .await;
        assert!(!err, "{pr1}");
        let proj_calls = req_count.load(Ordering::SeqCst) - proj_before;
        assert_eq!(
            proj_calls,
            if deployment == DeploymentKind::Cloud { 2 } else { 1 },
            "Cloud follows isLast (2 requests); Data Center fetches once"
        );
        assert_eq!(pr1["returned"], 50);
        assert_eq!(pr1["has_more"], true);
        assert_eq!(pr1["next_cursor"], "50");
        recovered_projects.extend(
            pr1["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        let (err, pr2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "cursor": "50" }),
        )
        .await;
        assert!(!err, "{pr2}");
        assert_eq!(pr2["returned"], 50);
        assert_eq!(pr2["has_more"], true);
        assert_eq!(pr2["next_cursor"], "100");
        recovered_projects.extend(
            pr2["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        let (err, pr3) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_projects",
            json!({ "cursor": "100" }),
        )
        .await;
        assert!(!err, "{pr3}");
        assert_eq!(pr3["returned"], 50);
        assert_eq!(pr3["has_more"], false);
        assert!(pr3["next_cursor"].is_null());
        recovered_projects.extend(
            pr3["projects"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["key"].as_str().unwrap().to_string()),
        );
        assert_eq!(recovered_projects.len(), 150);

        // 3. list_child_pages: limit 2 -> cursor -> remaining 1
        let (err, ch1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_child_pages",
            json!({ "page_id": "700", "limit": 2 }),
        )
        .await;
        assert!(!err, "{ch1}");
        assert_eq!(ch1["returned"], 2);
        assert_eq!(ch1["has_more"], true);
        let cursor = ch1["next_cursor"].as_str().unwrap().to_string();
        let (err, ch2) = call_mcp(
            &cfg,
            &client,
            &storage,
            "list_child_pages",
            json!({ "page_id": "700", "limit": 2, "cursor": cursor }),
        )
        .await;
        assert!(!err, "{ch2}");
        assert_eq!(ch2["returned"], 1);
        assert_eq!(ch2["has_more"], false);
        assert!(ch2["next_cursor"].is_null());

        // 4. Invalid offset cursor rejects with zero upstream calls
        let before = req_count.load(Ordering::SeqCst);
        let (is_err, err_msg) = call_mcp(
            &cfg,
            &client,
            &storage,
            "get_board_issues",
            json!({ "board_id": 7, "cursor": "abc" }),
        )
        .await;
        assert!(is_err);
        let parsed_err: Value = serde_json::from_str(err_msg.as_str().unwrap()).unwrap();
        assert_eq!(parsed_err["ok"], false);
        assert_eq!(
            parsed_err["error"]["message"].as_str().unwrap(),
            "cursor must be a non-negative integer offset taken from next_cursor"
        );
        assert_eq!(
            req_count.load(Ordering::SeqCst),
            before,
            "invalid cursor must not trigger upstream requests"
        );

        // 5. Control: search_jira_issues preserves v0.5.1 keys and 2-page continuation
        let (err, s1) = call_mcp(
            &cfg,
            &client,
            &storage,
            "search_jira_issues",
            json!({ "jql": "project = ENG", "limit": 100 }),
        )
        .await;
        assert!(!err, "{s1}");
        let mut s1_keys: Vec<&str> = s1.as_object().unwrap().keys().map(String::as_str).collect();
        s1_keys.sort();
        assert_eq!(
            s1_keys,
            vec!["issues", "jql", "next_page_token", "next_start_at", "site_id", "total"]
        );
        assert_eq!(s1["issues"].as_array().unwrap().len(), 100);
        let next_args = if deployment == DeploymentKind::Cloud {
            assert!(s1["total"].is_null());
            json!({ "jql": "project = ENG", "limit": 100, "page_token": s1["next_page_token"] })
        } else {
            assert_eq!(s1["total"], 120);
            json!({ "jql": "project = ENG", "limit": 100, "start_at": s1["next_start_at"] })
        };
        let (err, s2) = call_mcp(&cfg, &client, &storage, "search_jira_issues", next_args).await;
        assert!(!err, "{s2}");
        assert_eq!(s2["issues"].as_array().unwrap().len(), 20);
        assert!(s2["next_page_token"].is_null());
        assert!(s2["next_start_at"].is_null());
    }
}

#[tokio::test]
async fn http_offline_issues_total_is_cache_count() {
    ensure_isolated_env();
    let storage = Storage::open_in_memory().expect("storage");
    let issues: Vec<atlassian_cli::models::IssueSummary> = (1..=120)
        .map(|i| atlassian_cli::models::IssueSummary {
            id: format!("{i}"),
            key: format!("ENG-{i}"),
            project_key: "ENG".into(),
            summary: format!("Cached issue {i}"),
            updated_at: format!("2026-11-20T{:02}:{:02}:00Z", i / 60, i % 60),
            ..Default::default()
        })
        .collect();
    storage.upsert_issues(&issues).unwrap();
    storage
        .upsert_issue_detail(&atlassian_cli::models::IssueDetail {
            summary: issues[0].clone(),
            description_raw: "raw body".into(),
            description_markdown: "md body".into(),
            ..Default::default()
        })
        .unwrap();

    let cfg = AppConfig::default();
    let auth = AuthManager::new("http-offline-total");
    let client = AtlassianClient::new(cfg.clone(), auth).unwrap();
    let serve_url = spawn_server(serve::build_router(cfg, client, storage, serve::ServeOptions::default())).await;

    let http = reqwest::Client::new();
    let r2: Value = http
        .get(format!("{serve_url}/issues?offline=true&limit=2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r2["issues"].as_array().unwrap().len(), 2);
    assert_eq!(r2["total"], 120);

    let r1000: Value = http
        .get(format!("{serve_url}/issues?offline=true&limit=1000"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r1000["issues"].as_array().unwrap().len(), 120);
    assert_eq!(r1000["total"], 120);

    // Control: HTTP detail still defaults to full
    let detail: Value = http
        .get(format!("{serve_url}/issues/ENG-1?offline=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["issue"]["description_raw"], "raw body");
    assert!(detail["issue"].get("omitted").is_none());
}

/// Mock Data Center pair for split-credential tests: Jira under "/jira" accepts only
/// "PAT-J", Confluence under "/wiki" only "PAT-C". Every Authorization header is recorded
/// per product so a test can assert that no credential crossed over.
#[derive(Default)]
struct SplitPatMock {
    jira_auth: RwLock<Vec<String>>,
    confluence_auth: RwLock<Vec<String>>,
    requests: RwLock<Vec<String>>,
}

fn build_split_pat_router(state: Arc<SplitPatMock>) -> Router {
    Router::new().fallback(move |uri: axum::http::Uri, headers: HeaderMap| {
        let state = state.clone();
        async move {
            let path = uri.path().to_string();
            let auth = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            state.requests.write().unwrap().push(uri.to_string());
            if path.starts_with("/wiki") {
                state.confluence_auth.write().unwrap().push(auth);
                if let Err(resp) = check_bearer_auth(&headers, "PAT-C") {
                    return resp;
                }
                if path == "/wiki/rest/api/space" {
                    return Json(json!({
                        "results": [
                            { "id": 1, "key": "AAA", "name": "Alpha", "type": "global", "_links": { "webui": "/display/AAA" } },
                            { "id": 2, "key": "DOCS", "name": "Docs", "type": "global", "_links": { "webui": "/display/DOCS" } }
                        ],
                        "start": 0, "limit": 250, "size": 2, "_links": {}
                    }))
                    .into_response();
                }
                return Json(json!({ "results": [], "start": 0, "limit": 50, "size": 0, "_links": {} }))
                    .into_response();
            }
            state.jira_auth.write().unwrap().push(auth);
            if let Err(resp) = check_bearer_auth(&headers, "PAT-J") {
                return resp;
            }
            match path.as_str() {
                "/jira/rest/api/2/myself" => Json(json!({
                    "name": "jdoe", "key": "jdoe", "displayName": "John Doe", "active": true
                }))
                .into_response(),
                "/jira/rest/api/2/project" => Json(json!([
                    { "id": "10000", "key": "ENG", "name": "Engineering" }
                ]))
                .into_response(),
                _ => Json(json!({ "values": [], "issues": [], "total": 0, "isLast": true }))
                    .into_response(),
            }
        }
    })
}

fn split_pat_config(base_url: &str, confluence_account: Option<&str>) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.site_url = format!("{base_url}/jira");
    cfg.confluence_url = format!("{base_url}/wiki");
    cfg.deployment = DeploymentKind::DataCenter;
    cfg.default_site = Some("corp".into());
    cfg.sites = vec![atlassian_cli::config::SiteProfile {
        id: "corp".into(),
        name: "Corp DC".into(),
        site_url: format!("{base_url}/jira"),
        confluence_url: format!("{base_url}/wiki"),
        deployment: DeploymentKind::DataCenter,
        account_id: "corp".into(),
        confluence_account_id: confluence_account.map(str::to_string),
        ..Default::default()
    }];
    cfg
}

#[tokio::test]
async fn e2e_datacenter_split_pat_jira_and_confluence() {
    use atlassian_cli::atlassian::SiteClientPool;

    ensure_isolated_env();
    let state = Arc::new(SplitPatMock::default());
    let base_url = spawn_server(build_split_pat_router(state.clone())).await;

    let dir = tempfile::tempdir().unwrap();
    let cred_path = dir.path().join("credentials.json");
    let dc_pat = |token: &str, site_url: String| StoredCredentials::BearerPat {
        site_url,
        token: token.into(),
        deployment: DeploymentKind::DataCenter,
    };
    AuthManager::for_account("corp")
        .with_fallback_path(cred_path.clone())
        .save_credentials(&dc_pat("PAT-J", format!("{base_url}/jira")))
        .unwrap();
    AuthManager::for_account("corp-confluence")
        .with_fallback_path(cred_path.clone())
        .save_credentials(&dc_pat("PAT-C", format!("{base_url}/wiki")))
        .unwrap();

    // 1. With confluence_account_id each product gets its own PAT and nothing crosses over.
    let cfg = split_pat_config(&base_url, Some("corp-confluence"));
    let pool = SiteClientPool::from_config_with_options(&cfg, Some(cred_path.clone()), None, None)
        .expect("pool");
    let client = pool.client_for("corp").unwrap();
    assert_eq!(client.confluence_credential_account(), Some("corp-confluence"));

    let projects = client.list_projects().await.expect("jira with PAT-J");
    assert_eq!(projects[0].key, "ENG");
    let spaces = client.list_all_spaces().await.expect("confluence with PAT-C");
    assert_eq!(
        spaces.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        vec!["AAA", "DOCS"]
    );
    assert_eq!(client.get_myself().await.unwrap().id, "jdoe");
    {
        let jira = state.jira_auth.read().unwrap();
        let confluence = state.confluence_auth.read().unwrap();
        assert!(!jira.is_empty() && jira.iter().all(|h| h == "Bearer PAT-J"), "{jira:?}");
        assert!(
            !confluence.is_empty() && confluence.iter().all(|h| h == "Bearer PAT-C"),
            "{confluence:?}"
        );
    }

    // 2. Without the Confluence credential the Jira PAT is sent to Confluence, which
    //    rejects it with 401, while Jira keeps working.
    state.jira_auth.write().unwrap().clear();
    state.confluence_auth.write().unwrap().clear();
    let cfg = split_pat_config(&base_url, None);
    let pool = SiteClientPool::from_config_with_options(&cfg, Some(cred_path), None, None)
        .expect("pool");
    let client = pool.client_for("corp").unwrap();
    assert_eq!(client.confluence_credential_account(), None);
    assert_eq!(client.list_projects().await.unwrap()[0].key, "ENG");
    let err = client.list_all_spaces().await.expect_err("confluence must reject PAT-J");
    assert!(format!("{err:#}").contains("401"), "{err:#}");
    assert!(state
        .confluence_auth
        .read()
        .unwrap()
        .iter()
        .all(|h| h == "Bearer PAT-J"));
}

#[tokio::test]
async fn regression_known_2_sync_uses_default_space_key() {
    use atlassian_cli::{atlassian::SiteClientPool, sync};

    ensure_isolated_env();
    let state = Arc::new(SplitPatMock::default());
    let base_url = spawn_server(build_split_pat_router(state.clone())).await;

    let dir = tempfile::tempdir().unwrap();
    let cred_path = dir.path().join("credentials.json");
    let dc_pat = |token: &str, site_url: String| StoredCredentials::BearerPat {
        site_url,
        token: token.into(),
        deployment: DeploymentKind::DataCenter,
    };
    AuthManager::for_account("corp")
        .with_fallback_path(cred_path.clone())
        .save_credentials(&dc_pat("PAT-J", format!("{base_url}/jira")))
        .unwrap();
    AuthManager::for_account("corp-confluence")
        .with_fallback_path(cred_path.clone())
        .save_credentials(&dc_pat("PAT-C", format!("{base_url}/wiki")))
        .unwrap();

    // The configured default space is the second one the server lists.
    let mut cfg = split_pat_config(&base_url, Some("corp-confluence"));
    cfg.default_space_key = Some("DOCS".into());
    let pool = SiteClientPool::from_config_with_options(&cfg, Some(cred_path), None, None)
        .expect("pool");
    let client = pool.client_for("corp").unwrap();
    assert_eq!(client.config().default_space_key.as_deref(), Some("DOCS"));

    let storage = Storage::open_in_memory().unwrap();
    sync::bootstrap_workspace_for_site("corp", &client, &storage)
        .await
        .expect("bootstrap");

    let requests = state.requests.read().unwrap().clone();
    let page_requests: Vec<&String> = requests
        .iter()
        .filter(|uri| uri.starts_with("/wiki") && !uri.starts_with("/wiki/rest/api/space?"))
        .collect();
    assert!(
        page_requests.iter().any(|uri| uri.contains("DOCS")),
        "root pages of the default space DOCS must be fetched: {requests:?}"
    );
    assert!(
        !page_requests.iter().any(|uri| uri.contains("AAA")),
        "the first listed space must not be used when a default is configured: {requests:?}"
    );
}

#[tokio::test]
async fn regression_atl_09_confluence_only_dc_login_and_whoami() {
    use atlassian_cli::config::Persona;

    ensure_isolated_env();
    // A Confluence-only Data Center host: no Jira routes at all (404), and the Confluence
    // identity endpoint accepts only the Confluence PAT.
    let app = Router::new().route(
        "/rest/api/user/current",
        get(|headers: HeaderMap| async move {
            let authorized = headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                == Some("Bearer PAT-C");
            if !authorized {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            Json(json!({
                "type": "known",
                "username": "jdoe",
                "userKey": "8a7f808a5f1e2d3c015f1e2d3c4b0000",
                "displayName": "John Doe"
            }))
            .into_response()
        }),
    );
    let base_url = spawn_server(app).await;

    let auth = AuthManager::with_credentials(
        "wiki",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "PAT-C".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth).unwrap();

    let probe = client.probe_identity().await;
    let jira_failure = probe
        .jira
        .as_ref()
        .expect("jira is probed when a site URL is set")
        .as_ref()
        .expect_err("there is no Jira on this host");
    assert!(jira_failure.is_not_found(), "{jira_failure:?}");
    let confluence_user = probe.confluence_user().expect("confluence identity");
    assert_eq!(confluence_user.id, "jdoe");
    assert_eq!(confluence_user.display_name, "John Doe");
    assert_eq!(probe.status_labels(), ("not_found", "ok"));

    // Login and whoami accept the Confluence user for either persona.
    assert_eq!(probe.login_user(Persona::Jira).expect("jira persona").id, "jdoe");
    assert_eq!(
        probe.login_user(Persona::Confluence).expect("confluence persona").id,
        "jdoe"
    );

    // A rejected credential on the same host is still a hard failure.
    let bad_auth = AuthManager::with_credentials(
        "wiki-bad",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "PAT-J".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let bad_client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, bad_auth)
            .unwrap();
    let bad_probe = bad_client.probe_identity().await;
    assert_eq!(bad_probe.status_labels(), ("not_found", "unauthorized"));
    assert!(bad_probe.login_user(Persona::Jira).is_err());
}

#[tokio::test]
async fn regression_ent_anon_dc_200_anonymous_is_reported_as_unauthorized() {
    use atlassian_cli::{atlassian::client::ProbeFailure, models::SpaceSummary, sync};

    ensure_isolated_env();
    // Data Center with anonymous access enabled: a rejected PAT gets HTTP 200 and an
    // empty listing instead of 401.
    let anonymous = || {
        [
            ("X-AUSERNAME", "anonymous"),
            ("X-Seraph-LoginReason", "AUTHENTICATED_FAILED"),
        ]
    };
    let app = Router::new()
        .route(
            "/rest/api/space",
            get(move || async move {
                (anonymous(), Json(json!({ "results": [], "start": 0, "limit": 50, "size": 0 })))
            }),
        )
        .route(
            "/rest/api/user/current",
            get(move || async move { (anonymous(), Json(json!({ "type": "anonymous" }))) }),
        );
    let base_url = spawn_server(app).await;

    let auth = AuthManager::with_credentials(
        "corp",
        StoredCredentials::BearerPat {
            site_url: base_url.clone(),
            token: "PAT-J-secret".into(),
            deployment: DeploymentKind::DataCenter,
        },
    );
    let client =
        AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::DataCenter, auth).unwrap();

    let err = client
        .list_all_spaces()
        .await
        .expect_err("an anonymous 200 must not look like an empty space list");
    let failure = ProbeFailure::from_error(&err);
    assert_eq!(failure.status, Some(401), "{failure:?}");
    assert!(failure.anonymous && failure.is_unauthorized(), "{failure:?}");
    let text = format!("{err:#}");
    assert!(text.contains("ATLASSIAN_CONFLUENCE_PAT"), "{text}");
    assert!(text.contains("--confluence-pat"), "{text}");
    assert!(text.contains("X-AUSERNAME: anonymous"), "{text}");
    assert!(!text.contains("PAT-J-secret"), "the credential must never be printed: {text}");

    let probe = client.probe_identity().await;
    assert_eq!(probe.status_labels().1, "anonymous");

    // The cached spaces survive a sync that only sees the anonymous answer.
    let storage = Storage::open_in_memory().unwrap();
    let cached = SpaceSummary {
        id: "sp-DOCS".into(),
        key: "DOCS".into(),
        name: "Docs".into(),
        space_type: "global".into(),
        homepage_id: Some("100".into()),
        description: None,
        web_url: format!("{base_url}/display/DOCS"),
        pinned: false,
    };
    storage.upsert_spaces_for_site("corp", &[cached.clone()]).unwrap();
    let _ = sync::bootstrap_workspace_for_site("corp", &client, &storage).await;
    let after = storage.list_spaces_for_site("corp").unwrap();
    assert_eq!(
        after.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        vec!["DOCS"],
        "the spaces cache must not be pruned or overwritten"
    );
    assert_eq!(after[0].name, "Docs");
}
