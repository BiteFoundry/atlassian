//! CLI and stdio MCP workflows against documented Cloud and Data Center routes.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command};

pub(crate) const XHTML: &str = "<ac:layout><ac:layout-section><ac:layout-cell><p>Fixture</p></ac:layout-cell></ac:layout-section></ac:layout>";
pub(crate) const ADF: &str = r#"{"version":1,"type":"doc","content":[{"type":"panel","attrs":{"localId":"fixture"},"content":[{"type":"paragraph","content":[{"type":"text","text":"Fixture"}]}]}]}"#;

pub(crate) struct Fixture {
    pub(crate) cloud: bool,
    pub(crate) fields: Value,
    pub(crate) parent: &'static str,
    pub(crate) page_title: String,
    pub(crate) page_body: String,
    pub(crate) version: i32,
    pub(crate) sprint: i32,
    pub(crate) epic: String,
    pub(crate) labels: Vec<String>,
    pub(crate) attachments: Vec<Value>,
    pub(crate) worklogs: Vec<Value>,
    pub(crate) fail_remotelink: bool,
    pub(crate) fail_labels: bool,
    pub(crate) requests: Vec<(String, String)>,
    pub(crate) writes: Vec<(String, String, Value)>,
}

impl Fixture {
    pub(crate) fn new(cloud: bool) -> Self {
        Self {
            cloud,
            fields: json!({
                "summary": "Existing summary",
                "description": if cloud { serde_json::from_str(ADF).unwrap() } else { json!("h2. Fixture") },
                "labels": ["keep"],
                "issuetype": {"name": "Task", "subtask": false},
                "customfield_10009": "unrelated"
            }),
            parent: "702",
            page_title: "Page 701".into(),
            page_body: if cloud { ADF } else { XHTML }.into(),
            version: 7,
            sprint: 4,
            epic: String::new(),
            labels: vec!["keep".into(), "legacy/ops".into()],
            attachments: Vec::new(),
            worklogs: Vec::new(),
            fail_remotelink: false,
            fail_labels: false,
            requests: Vec::new(),
            writes: Vec::new(),
        }
    }

    pub(crate) fn page(&self, id: &str) -> Value {
        let parent = (id == "701").then_some(self.parent);
        let version = if id == "701" { self.version } else { 1 };
        let title = if id == "701" {
            self.page_title.clone()
        } else {
            format!("Page {id}")
        };
        if self.cloud {
            json!({"id": id, "spaceId": "77", "spaceKey": "ENG", "parentId": parent,
                "title": title, "status": "current", "version": {"number": version},
                "body": {"atlas_doc_format": {"value": self.page_body}}})
        } else {
            let ancestors: Vec<_> = parent.into_iter().map(|p| json!({"id": p})).collect();
            let mut labels_obj = json!({"results": self.label_entries()});
            if self.fail_labels {
                labels_obj["_links"] = json!({"next": format!("/rest/api/content/{id}/label?start=2")});
            }
            json!({"id": id, "space": {"id": 77, "key": if id == "704" {"OTHER"} else {"ENG"}},
                "title": title, "type": "page", "version": {"number": version},
                "ancestors": ancestors, "body": {"storage": {"value": self.page_body}},
                "metadata": {"labels": labels_obj},
                "children": {"attachment": {"results": self.attachments}}})
        }
    }

    pub(crate) fn label_entries(&self) -> Vec<Value> {
        self.labels
            .iter()
            .map(|name| json!({"name": name, "prefix": "global"}))
            .collect()
    }
}

pub(crate) async fn upstream(
    State(state): State<Arc<Mutex<Fixture>>>,
    method: Method,
    path: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    let route = path.path();
    let jira = if state.cloud {
        "/rest/api/3"
    } else {
        "/rest/api/2"
    };
    let conf = if state.cloud {
        "/wiki/rest/api/content"
    } else {
        "/rest/api/content"
    };
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let query: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(path.query().unwrap_or("").as_bytes()).collect();
    state.requests.push((method.to_string(), path.to_string()));
    if method != Method::GET {
        state
            .writes
            .push((method.to_string(), path.to_string(), body.clone()));
    }
    let get = method == Method::GET;

    if route == format!("{jira}/myself") && get {
        return Json(json!({
            "accountId": "acc-test",
            "displayName": "Test User",
            "emailAddress": "test@example.invalid",
            "active": true
        }))
        .into_response();
    }
    if route == format!("{jira}/user/assignable/search") && get {
        let q = query
            .get("query")
            .or_else(|| query.get("username"))
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let all_users = [
            json!({
                "accountId": "acc-test",
                "displayName": "Test User",
                "emailAddress": "test@example.invalid",
                "active": true
            }),
            json!({
                "accountId": "acc-bob",
                "displayName": "Bob Lee",
                "emailAddress": "bob.lee@example.invalid",
                "active": true
            }),
            json!({
                "accountId": "acc-bob2",
                "displayName": "Bob Park",
                "emailAddress": "bob.park@example.invalid",
                "active": true
            }),
        ];
        let matched: Vec<Value> = all_users
            .into_iter()
            .filter(|u| {
                q.is_empty()
                    || u["displayName"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&q)
                    || u["emailAddress"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&q)
            })
            .collect();
        return Json(Value::Array(matched)).into_response();
    }
    if route == format!("{jira}/issue") && method == Method::POST {
        let fields = body["fields"].as_object().unwrap();
        if let Some(assignee) = fields.get("assignee").and_then(|v| v.as_object()) {
            let id = assignee
                .get("id")
                .or_else(|| assignee.get("accountId"))
                .or_else(|| assignee.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !matches!(id, "acc-test" | "acc-bob" | "acc-bob2") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "errorMessages": [],
                        "errors": { "assignee": "Specify a valid value for assignee" }
                    })),
                )
                    .into_response();
            }
            state.fields["assignee"] = json!({
                "accountId": id,
                "displayName": match id {
                    "acc-bob" => "Bob Lee",
                    "acc-bob2" => "Bob Park",
                    _ => "Test User",
                }
            });
        }
        if let Some(summary) = fields.get("summary") {
            state.fields["summary"] = summary.clone();
        }
        if let Some(description) = fields.get("description") {
            state.fields["description"] = description.clone();
        }
        return (StatusCode::CREATED, Json(json!({ "id": "1001", "key": "ATL-1" }))).into_response();
    }

    if route == format!("{jira}/issueLink") && method == Method::POST {
        assert_eq!(
            body,
            json!({"inwardIssue": {"key": "ATL-1"}, "outwardIssue": {"key": "ATL-2"}, "type": {"name": "Blocks"}})
        );
        state.fields["issuelinks"] = json!([{"id": "88", "type": {"name": "Blocks",
            "inward": "is blocked by", "outward": "blocks"}, "outwardIssue": {"key": "ATL-2"}}]);
        return StatusCode::CREATED.into_response();
    }
    if let Some(key) = route.strip_prefix(&format!("{jira}/issue/")) {
        if key == "ATL-1/worklog" && method == Method::POST {
            assert_eq!(query["adjustEstimate"], "leave");
            let mut logged = body;
            logged["id"] = json!((state.worklogs.len() + 1).to_string());
            state.worklogs.push(logged.clone());
            if logged["timeSpentSeconds"] == 66 {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"message":"Ambiguous fixture response"})),
                )
                    .into_response();
            }
            return (StatusCode::CREATED, Json(logged)).into_response();
        }
        if let Some(id) = key.strip_prefix("ATL-1/worklog/") {
            assert!(get);
            let entry = state.worklogs.iter().find(|log| log["id"] == id).unwrap();
            return Json(entry.clone()).into_response();
        }
        if get && key.ends_with("/remotelink") {
            if state.fail_remotelink {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"message": "remotelink unavailable"})),
                )
                    .into_response();
            }
            return Json(json!([])).into_response();
        }
        if key == "ATL-1/assignee" && method == Method::PUT {
            let id = body
                .get("accountId")
                .or_else(|| body.get("name"))
                .and_then(|v| v.as_str());
            match id {
                None => {
                    state.fields["assignee"] = Value::Null;
                    return StatusCode::NO_CONTENT.into_response();
                }
                Some(uid) if matches!(uid, "acc-test" | "acc-bob" | "acc-bob2") => {
                    state.fields["assignee"] = json!({
                        "accountId": uid,
                        "displayName": match uid {
                            "acc-bob" => "Bob Lee",
                            "acc-bob2" => "Bob Park",
                            _ => "Test User",
                        }
                    });
                    return StatusCode::NO_CONTENT.into_response();
                }
                Some(unknown) => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({
                            "errorMessages": [format!("User '{unknown}' does not exist")]
                        })),
                    )
                        .into_response();
                }
            }
        }
        if get && key == "ATL-99999" {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "errorMessages": ["Issue does not exist"] })),
            )
                .into_response();
        }
        if get {
            let mut fields = state.fields.clone();
            if key == "ATL-3" {
                fields["issuetype"]["subtask"] = json!(true);
            }
            if let Some(selection) = query.get("fields") {
                fields
                    .as_object_mut()
                    .unwrap()
                    .retain(|name, _| selection.split(',').any(|f| f == name));
            }
            return Json(json!({"id": "1001", "key": key, "fields": fields})).into_response();
        }
        if method == Method::PUT && key == "ATL-1" {
            let fields = body["fields"].as_object().unwrap();
            assert!(fields
                .keys()
                .all(|name| matches!(
                    name.as_str(),
                    "parent" | "description" | "summary" | "assignee"
                ) || name.starts_with("customfield_")));
            if let Some(assignee) = fields.get("assignee") {
                if !assignee.is_null() {
                    let id = assignee
                        .get("id")
                        .or_else(|| assignee.get("accountId"))
                        .or_else(|| assignee.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !matches!(id, "acc-test" | "acc-bob" | "acc-bob2") {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "errorMessages": [],
                                "errors": { "assignee": "Specify a valid value for assignee" }
                            })),
                        )
                            .into_response();
                    }
                }
            }
            for (name, value) in fields {
                state.fields[name] = value.clone();
            }
            return StatusCode::NO_CONTENT.into_response();
        }
    }
    if route == "/rest/agile/1.0/sprint/20/issue" && method == Method::POST {
        assert_eq!(body, json!({"issues": ["ATL-1","ATL-2"]}));
        state.sprint = 20;
        return StatusCode::NO_CONTENT.into_response();
    }
    if route == "/rest/agile/1.0/epic/ATL-9/issue" && method == Method::POST && !state.cloud {
        assert_eq!(body, json!({"issues": ["ATL-1"]}));
        state.epic = "ATL-9".into();
        return StatusCode::NO_CONTENT.into_response();
    }
    if let Some(key) = route.strip_prefix("/rest/agile/1.0/issue/").filter(|_| get) {
        return Json(json!({"key": key, "fields": {"sprint": {"id": state.sprint}, "epic": {"key": state.epic}}})).into_response();
    }
    if route == "/wiki/rest/api/content/701/move/append/703" && state.cloud && method == Method::PUT
    {
        assert!(bytes.is_empty(), "Cloud move sent a body");
        state.parent = "703";
        return Json(json!({"pageId": "701"})).into_response();
    }
    if route == format!("{conf}/701/label") {
        if method == Method::POST {
            for entry in body.as_array().unwrap() {
                assert_eq!(entry["prefix"], "global");
                let name = entry["name"].as_str().unwrap();
                if !state.labels.iter().any(|label| label == name) {
                    state.labels.push(name.to_owned());
                }
            }
            return Json(json!({"results": state.label_entries()})).into_response();
        }
        if method == Method::DELETE {
            let name = &query["name"];
            state.labels.retain(|label| label != name);
            return StatusCode::NO_CONTENT.into_response();
        }
    }
    if route == format!("{conf}/701/child/attachment") && method == Method::POST {
        assert_eq!(headers["x-atlassian-token"], "no-check");
        let payload = String::from_utf8_lossy(&bytes);
        assert!(payload.contains("filename=\"fixture.txt\""));
        assert!(payload.contains("fixture attachment content"));
        assert_eq!(payload.contains("name=\"minorEdit\""), state.cloud);
        let attachment = json!({"id": "att-54", "title": "fixture.txt", "extensions": {
            "mediaType": "text/plain", "fileSize": 26}, "_links": {"download": "/download/fixture.txt"}
        });
        state.attachments.push(attachment.clone());
        return Json(json!({"results": [attachment]})).into_response();
    }
    if route == "/wiki/api/v2/spaces" && state.cloud && get {
        return Json(json!({
            "results": [{ "id": "77", "key": "ENG", "name": "Engineering", "type": "global" }]
        }))
        .into_response();
    }
    if route == "/wiki/api/v2/pages" && state.cloud && method == Method::POST {
        if let Some(title) = body["title"].as_str() {
            state.page_title = title.to_owned();
        }
        if let Some(val) = body["body"]["value"].as_str() {
            state.page_body = val.to_owned();
        }
        return Json(state.page("701")).into_response();
    }
    if let Some(page) = route.strip_prefix(if state.cloud {
        "/wiki/api/v2/pages/"
    } else {
        "/rest/api/content/"
    }) {
        if get {
            if page.ends_with("/labels") {
                if state.fail_labels {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"message": "labels unavailable"})),
                    )
                        .into_response();
                }
                return Json(json!({"results": state.label_entries()})).into_response();
            }
            if page.ends_with("/attachments") {
                return Json(json!({"results": state.attachments})).into_response();
            }
            if page.ends_with("/footer-comments") || page.ends_with("/child/comment") {
                return Json(json!({"results": []})).into_response();
            }
            return Json(state.page(page)).into_response();
        }
        if page == "701" && method == Method::PUT && body["body"].is_object() {
            if body["version"]["number"] != state.version + 1 {
                return StatusCode::CONFLICT.into_response();
            }
            let native = if state.cloud {
                &body["body"]
            } else {
                &body["body"]["storage"]
            };
            assert_eq!(
                native["representation"],
                if state.cloud {
                    "atlas_doc_format"
                } else {
                    "storage"
                }
            );
            if let Some(title) = body["title"].as_str() {
                state.page_title = title.to_owned();
            }
            state.page_body = native["value"].as_str().unwrap().to_owned();
            state.version += 1;
            return Json(state.page(page)).into_response();
        }
        if !state.cloud && page == "701" && method == Method::PUT {
            assert_eq!(
                body,
                json!({"id":"701", "type":"page", "title":"Page 701",
                    "space":{"key":"ENG"}, "ancestors":[{"id":"703"}], "version":{"number":8}
                })
            );
            state.parent = "703";
            state.version += 1;
            return Json(state.page(page)).into_response();
        }
    }
    (StatusCode::NOT_FOUND, format!("Unhandled {method} {path}")).into_response()
}

pub(crate) struct Harness {
    pub(crate) state: Arc<Mutex<Fixture>>,
    pub(crate) profile: TempDir,
    pub(crate) url: String,
    pub(crate) bin: PathBuf,
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl Harness {
    pub(crate) async fn new(cloud: bool) -> Self {
        let fixture = Arc::new(Mutex::new(Fixture::new(cloud)));
        let router = Router::new().fallback(upstream).with_state(fixture.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let bin = std::env::var_os("ATLASSIAN_WORKFLOW_TEST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_atlassian")));
        Self {
            state: fixture,
            profile: tempfile::tempdir().unwrap(),
            url,
            bin,
            task,
        }
    }

    pub(crate) fn process(&self) -> Command {
        let cloud = self.state.lock().unwrap().cloud;
        let mut cmd = Command::new(&self.bin);
        cmd.kill_on_drop(true)
            .current_dir(self.profile.path())
            .env("ATLASSIAN_CONFIG_DIR", self.profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("ATLASSIAN_SITE_URL", &self.url)
            .env(
                "ATLASSIAN_CONFLUENCE_URL",
                if cloud {
                    format!("{}/wiki", self.url)
                } else {
                    self.url.clone()
                },
            )
            .env(
                "ATLASSIAN_DEPLOYMENT",
                if cloud { "cloud" } else { "datacenter" },
            )
            .env("ATLASSIAN_PERSONA", "jira")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE")
            .env_remove("JIRA_PAT")
            .env_remove("ATLASSIAN_PAT")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY");
        if cloud {
            cmd.env("ATLASSIAN_EMAIL", "test@example.invalid")
                .env("ATLASSIAN_API_TOKEN", "fixture-token");
        } else {
            cmd.env("ATLASSIAN_PAT", "fixture-pat");
        }
        cmd
    }

    pub(crate) async fn cli_with_persona(&self, persona: &str, args: &[&str], success: bool) -> Value {
        let mut cmd = self.process();
        cmd.env("ATLASSIAN_PERSONA", persona);
        let result = tokio::time::timeout(Duration::from_secs(20), cmd.args(args).output())
            .await
            .expect("CLI timed out")
            .unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.success(), success, "{args:?} {stderr}");
        if success {
            serde_json::from_slice(&result.stdout).unwrap()
        } else {
            json!(stderr)
        }
    }

    pub(crate) async fn cli(&self, args: &[&str], success: bool) -> Value {
        self.cli_with_persona("jira", args, success).await
    }

    pub(crate) async fn mcp_rpc(&self, persona: &str, mcp_args: &[&str], request: Value) -> (Value, String) {
        let mut cmd = self.process();
        cmd.env("ATLASSIAN_PERSONA", persona);
        let mut child = cmd
            .arg("mcp")
            .args(mcp_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2024-11-05\"}}}}\n\
                     {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{request}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
            .await
            .expect("MCP timed out")
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let line = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .find(|line| {
                serde_json::from_str::<Value>(line)
                    .map(|val| val["id"] == request["id"])
                    .unwrap_or(false)
            })
            .unwrap()
            .to_string();
        let response: Value = serde_json::from_str(&line).unwrap();
        (response, line)
    }

    pub(crate) async fn mcp(&self, name: &str, args: Value, success: bool) -> Value {
        self.mcp_with_options("jira", &[], name, args, success)
            .await
    }

    pub(crate) async fn mcp_with_options(
        &self,
        persona: &str,
        mcp_args: &[&str],
        name: &str,
        args: Value,
        success: bool,
    ) -> Value {
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}});
        let (response, _) = self.mcp_rpc(persona, mcp_args, call).await;
        assert_eq!(
            response["result"]["isError"], !success,
            "{name}: {response}"
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        if success {
            serde_json::from_str(text).unwrap()
        } else {
            json!(text)
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.task.abort();
    }
}


pub(crate) fn apply_body_preview(original: &Value, change: &Value) -> Value {
    let start = change["start"].as_u64().unwrap() as usize;
    if change["representation"] == "atlas_doc_format" {
        let mut updated = original.clone();
        let content = updated["content"].as_array_mut().unwrap();
        let before = change["before"].as_array().unwrap();
        assert_eq!(&content[start..start + before.len()], before);
        content.splice(
            start..start + before.len(),
            change["after"].as_array().unwrap().clone(),
        );
        updated
    } else {
        let mut updated = original.as_str().unwrap().to_owned();
        let before = change["before"].as_str().unwrap();
        assert_eq!(&updated[start..start + before.len()], before);
        updated.replace_range(
            start..start + before.len(),
            change["after"].as_str().unwrap(),
        );
        json!(updated)
    }
}

pub(crate) fn native_body(cloud: bool, raw: &str) -> Value {
    if cloud {
        serde_json::from_str(raw).unwrap()
    } else {
        json!(raw)
    }
}

