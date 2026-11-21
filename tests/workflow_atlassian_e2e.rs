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

const XHTML: &str = "<ac:layout><ac:layout-section><ac:layout-cell><p>Fixture</p></ac:layout-cell></ac:layout-section></ac:layout>";
const ADF: &str = r#"{"version":1,"type":"doc","content":[{"type":"panel","attrs":{"localId":"fixture"},"content":[{"type":"paragraph","content":[{"type":"text","text":"Fixture"}]}]}]}"#;

struct Fixture {
    cloud: bool,
    fields: Value,
    parent: &'static str,
    page_title: String,
    page_body: String,
    version: i32,
    sprint: i32,
    epic: String,
    labels: Vec<String>,
    attachments: Vec<Value>,
    worklogs: Vec<Value>,
    requests: Vec<(String, String)>,
    writes: Vec<(String, String, Value)>,
}

impl Fixture {
    fn new(cloud: bool) -> Self {
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
            requests: Vec::new(),
            writes: Vec::new(),
        }
    }

    fn page(&self, id: &str) -> Value {
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
            json!({"id": id, "space": {"id": 77, "key": if id == "704" {"OTHER"} else {"ENG"}},
                "title": title, "type": "page", "version": {"number": version},
                "ancestors": ancestors, "body": {"storage": {"value": self.page_body}},
                "metadata": {"labels": {"results": self.label_entries()}},
                "children": {"attachment": {"results": self.attachments}}})
        }
    }

    fn label_entries(&self) -> Vec<Value> {
        self.labels
            .iter()
            .map(|name| json!({"name": name, "prefix": "global"}))
            .collect()
    }
}

async fn upstream(
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
            return Json(json!([])).into_response();
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
                .all(|name| matches!(name.as_str(), "parent" | "description" | "summary")
                    || name.starts_with("customfield_")));
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
    if let Some(page) = route.strip_prefix(if state.cloud {
        "/wiki/api/v2/pages/"
    } else {
        "/rest/api/content/"
    }) {
        if get {
            if page.ends_with("/labels") {
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

struct Harness {
    state: Arc<Mutex<Fixture>>,
    profile: TempDir,
    url: String,
    bin: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new(cloud: bool) -> Self {
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

    fn process(&self) -> Command {
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

    async fn cli_with_persona(&self, persona: &str, args: &[&str], success: bool) -> Value {
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

    async fn cli(&self, args: &[&str], success: bool) -> Value {
        self.cli_with_persona("jira", args, success).await
    }

    async fn mcp_rpc(&self, persona: &str, mcp_args: &[&str], request: Value) -> (Value, String) {
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

    async fn mcp(&self, name: &str, args: Value, success: bool) -> Value {
        self.mcp_with_options("jira", &[], name, args, success)
            .await
    }

    async fn mcp_with_options(
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

#[tokio::test]
async fn jira_workflows_preserve_unrelated_fields_and_verify_writes() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;
        let original = mock.state.lock().unwrap().fields.clone();
        let parent = if cloud {
            mock.cli(&["parent", "ATL-1", "ATL-9", "--json"], true)
                .await
        } else {
            let err = mock
                .mcp(
                    "set_jira_parent",
                    json!({"key":"ATL-3", "parent_key":"ATL-9"}),
                    false,
                )
                .await;
            assert!(err.as_str().unwrap().contains("subtask"));
            assert!(mock.state.lock().unwrap().writes.is_empty());
            mock.mcp(
                "set_jira_parent",
                json!({"key":"ATL-1", "parent_key":"ATL-9"}),
                true,
            )
            .await
        };
        assert_eq!(parent["parent_key"], "ATL-9");

        let fields = json!({"customfield_10100": 4, "customfield_10101": [{"id":"5"}],
            "customfield_10102": null, "customfield_10103": true, "customfield_10104": "typed"});
        let updated = if cloud {
            mock.mcp(
                "update_jira_custom_fields",
                json!({"key":"ATL-1","fields":fields}),
                true,
            )
            .await
        } else {
            let file = mock.profile.path().join("fields.json");
            std::fs::write(&file, fields.to_string()).unwrap();
            mock.cli(
                &[
                    "custom-fields",
                    "ATL-1",
                    "--fields-file",
                    file.to_str().unwrap(),
                    "--json",
                ],
                true,
            )
            .await
        };
        assert_eq!(updated["custom_fields"], fields);
        let prior_writes = mock.state.lock().unwrap().writes.len();
        let invalid = mock
            .mcp(
                "update_jira_custom_fields",
                json!({"key":"ATL-1","fields":{"summary":"unrelated"}}),
                false,
            )
            .await;
        assert!(invalid
            .as_str()
            .unwrap()
            .contains("invalid custom field ID"));
        mock.cli(
            &["custom-fields", "ATL-1", "--fields-json", "[]", "--json"],
            false,
        )
        .await;
        mock.mcp(
            "move_jira_issues_to_sprint",
            json!({"sprint_id":20,"issue_keys":["ATL-1","atl-1"]}),
            false,
        )
        .await;
        mock.mcp(
            "add_jira_worklog",
            json!({"key":"ATL-1","seconds":0}),
            false,
        )
        .await;
        mock.cli(
            &[
                "log-work",
                "ATL-1",
                "--seconds",
                "45",
                "--started",
                "yesterday",
            ],
            false,
        )
        .await;
        assert_eq!(mock.state.lock().unwrap().writes.len(), prior_writes);

        let link = if cloud {
            mock.cli(
                &["link", "ATL-1", "ATL-2", "--type", "Blocks", "--json"],
                true,
            )
            .await
        } else {
            mock.mcp(
                "create_jira_issue_link",
                json!({"inward_key":"ATL-1", "outward_key":"ATL-2", "link_type":"Blocks"}),
                true,
            )
            .await
        };
        assert_eq!(link["link"]["id"], "88");
        let sprint = if cloud {
            mock.mcp(
                "move_jira_issues_to_sprint",
                json!({"sprint_id":20,"issue_keys":["ATL-1","ATL-2"]}),
                true,
            )
            .await
        } else {
            mock.cli(&["move-to-sprint", "20", "ATL-1", "ATL-2", "--json"], true)
                .await
        };
        assert_eq!(sprint["issue_keys"], json!(["ATL-1", "ATL-2"]));
        let logged = if cloud {
            mock.cli(
                &[
                    "log-work",
                    "ATL-1",
                    "--seconds",
                    "1800",
                    "--started",
                    "2026-11-21T10:00:00+09:00",
                    "--comment",
                    "**Triage**",
                    "--json",
                ],
                true,
            )
            .await
        } else {
            mock.mcp(
                "add_jira_worklog",
                json!({"key":"ATL-1","seconds":1800,"started":
                "2026-11-21T10:00:00+09:00","comment_markdown":"**Triage**"}),
                true,
            )
            .await
        };
        assert_eq!(logged["worklog"]["id"], "1");
        assert_eq!(logged["worklog"]["timeSpentSeconds"], 1800);
        assert_eq!(logged["worklog"]["started"], "2026-11-21T10:00:00.000+0900");
        assert_eq!(logged["worklog"]["comment"].is_object(), cloud);
        mock.mcp(
            "add_jira_worklog",
            json!({"key":"ATL-1","seconds":66}),
            false,
        )
        .await;
        let state = mock.state.lock().unwrap();
        assert_eq!(state.worklogs.len(), 2, "ambiguous POST was replayed");
        for name in ["summary", "labels", "description", "customfield_10009"] {
            assert_eq!(state.fields[name], original[name]);
        }
        let expected = if cloud {
            json!({"parent":{"key":"ATL-9"}})
        } else {
            json!({"issues":["ATL-1"]})
        };
        assert_eq!(
            state.writes[0].2,
            if cloud {
                json!({"fields": expected})
            } else {
                expected
            }
        );
        assert_eq!(state.writes[1].2, json!({"fields": fields}));
    }
}

#[tokio::test]
async fn confluence_workflows_keep_native_body_and_use_versioned_metadata_on_dc() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;
        let raw = mock.state.lock().unwrap().page("701")["body"].clone();
        mock.mcp(
            "move_confluence_page",
            json!({"page_id":"701","parent_id":"703","prev_version":6}),
            false,
        )
        .await;
        mock.mcp(
            "move_confluence_page",
            json!({"page_id":"701","parent_id":"701"}),
            false,
        )
        .await;
        if !cloud {
            mock.mcp(
                "move_confluence_page",
                json!({"page_id":"701","parent_id":"703"}),
                false,
            )
            .await;
            mock.mcp(
                "move_confluence_page",
                json!({"page_id":"701","parent_id":"704","prev_version":7}),
                false,
            )
            .await;
        }
        let file = mock.profile.path().join("fixture.txt");
        mock.mcp(
            "upload_confluence_attachment",
            json!({"page_id":"701","path":file}),
            false,
        )
        .await;
        mock.mcp(
            "add_confluence_labels",
            json!({"page_id":"701","labels":[""]}),
            false,
        )
        .await;
        assert!(mock.state.lock().unwrap().writes.is_empty());

        let moved = if cloud {
            mock.cli(&["wiki", "move", "701", "703", "--json"], true)
                .await
        } else {
            mock.mcp(
                "move_confluence_page",
                json!({"page_id":"701","parent_id":"703","prev_version":7}),
                true,
            )
            .await
        };
        let expected_version = if cloud { 7 } else { 8 };
        assert_eq!(moved["page"]["parent_id"], "703");
        assert_eq!(moved["page"]["version_number"], expected_version);
        mock.mcp(
            "move_confluence_page",
            json!({"page_id":"701","parent_id":"703","prev_version":expected_version}),
            true,
        )
        .await;
        assert_eq!(mock.state.lock().unwrap().writes.len(), 1);

        let added = if cloud {
            mock.cli(
                &[
                    "wiki", "labels", "701", "--add", "ops", "--add", "new", "--json",
                ],
                true,
            )
            .await
        } else {
            mock.mcp(
                "add_confluence_labels",
                json!({"page_id":"701","labels":["ops","new"]}),
                true,
            )
            .await
        };
        assert_eq!(added["added"], json!(["ops", "new"]));
        if cloud {
            mock.mcp(
                "remove_confluence_label",
                json!({"page_id":"701","label":"legacy/ops"}),
                true,
            )
            .await;
        } else {
            mock.cli(
                &["wiki", "labels", "701", "--remove", "legacy/ops", "--json"],
                true,
            )
            .await;
        }
        std::fs::write(&file, b"fixture attachment content").unwrap();
        let uploaded = mock
            .mcp(
                "upload_confluence_attachment",
                json!({"page_id":"701","path":file}),
                true,
            )
            .await;
        assert_eq!(uploaded["attachments"][0]["id"], "att-54");
        let page = mock.cli(&["wiki", "view", "701", "--json"], true).await;
        assert_eq!(page["page"]["summary"]["version_number"], expected_version);
        assert_eq!(page["page"]["labels"], json!(["keep", "ops", "new"]));
        let native = if cloud {
            &raw["atlas_doc_format"]["value"]
        } else {
            &raw["storage"]["value"]
        };
        assert_eq!(&page["page"]["body_raw"], native);
        let state = mock.state.lock().unwrap();
        assert_eq!(
            state.writes.len(),
            4,
            "move, add labels, remove label, upload"
        );
        assert!(state.writes[2].1.ends_with("?name=legacy%2Fops"));
    }
}

fn apply_body_preview(original: &Value, change: &Value) -> Value {
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

fn native_body(cloud: bool, raw: &str) -> Value {
    if cloud {
        serde_json::from_str(raw).unwrap()
    } else {
        json!(raw)
    }
}

#[tokio::test]
async fn detail_mcp_defaults_to_compact_for_live_and_offline_without_changing_cli() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;
        for (tool, field, identifier, id, markdown, raw, cli_args) in [
            (
                "get_jira_issue",
                "issue",
                "key",
                "ATL-1",
                "description_markdown",
                "description_raw",
                vec!["view", "ATL-1", "--json"],
            ),
            (
                "get_confluence_page",
                "page",
                "page_id",
                "701",
                "body_markdown",
                "body_raw",
                vec!["wiki", "view", "701", "--json"],
            ),
        ] {
            let expected = mock.cli(&cli_args, true).await;
            assert!(expected[field][raw].is_string(), "{tool}: {expected}");
            // Seed project metadata so a fresh MCP process can read the existing detail cache.
            let cache = mock.profile.path().join("default").join("cache.db");
            let storage = atlassian_cli::storage::Storage::open(&cache).unwrap();
            storage
                .upsert_projects_for_site(
                    expected["site_id"].as_str().unwrap_or("default"),
                    &[atlassian_cli::models::ProjectSummary {
                        key: "ATL".into(),
                        name: "Fixture Project".into(),
                        ..Default::default()
                    }],
                )
                .unwrap();
            for offline in [false, true] {
                let mut args = json!({(identifier): id, "offline": offline});
                let default = mock.mcp(tool, args.clone(), true).await;
                args["compact"] = json!(true);
                let compact = mock.mcp(tool, args.clone(), true).await;
                args["compact"] = json!(false);
                let full = mock.mcp(tool, args, true).await;
                assert_eq!(default, compact, "{tool}, offline={offline}");
                assert_eq!(
                    default[field].as_object().unwrap().len(),
                    if field == "issue" { 4 } else { 6 }
                );
                assert_eq!(default[field][markdown], expected[field][markdown]);
                assert_eq!(full[field][markdown], default[field][markdown]);
                assert_eq!(full[field][raw], expected[field][raw]);
                assert_eq!(full[field]["summary"], expected[field]["summary"]);
            }
        }
        assert!(mock.state.lock().unwrap().writes.is_empty());
    }
}

#[tokio::test]
async fn jira_and_confluence_cloud_and_dc_dry_runs_match_native_writes() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;
        let cache = mock.profile.path().join("default").join("cache.db");
        let mut issue = mock.state.lock().unwrap().fields["description"].clone();
        let mut page = native_body(cloud, &mock.state.lock().unwrap().page_body);
        for (operation, cli_args, arguments) in [
            (
                "append",
                vec!["--append", "**Added**"],
                json!({"append_markdown": "**Added**"}),
            ),
            (
                "replace",
                vec!["--replace", "Added", "Vérified"],
                json!({"replace_text":{"old_text":"Added","new_text":"Vérified"}}),
            ),
            (
                "full",
                vec!["--body", "Full replacement."],
                json!({"description_markdown": "Full replacement."}),
            ),
        ] {
            let mut issue_args = arguments.clone();
            issue_args["key"] = json!("ATL-1");
            issue_args["dry_run"] = json!(true);
            let mut page_args = arguments;
            if operation == "full" {
                page_args = json!({"body_markdown": issue_args["description_markdown"]});
            }
            let version = mock.state.lock().unwrap().version;
            page_args["page_id"] = json!("701");
            page_args["prev_version"] = json!(version);
            page_args["dry_run"] = json!(true);
            let version_arg = version.to_string();
            let mut issue_cli = vec!["edit", "ATL-1", "--dry-run", "--json"];
            issue_cli.extend(cli_args.iter().copied());
            let mut page_cli = vec![
                "wiki",
                "edit",
                "701",
                "--prev-version",
                &version_arg,
                "--dry-run",
                "--json",
            ];
            page_cli.extend(cli_args);
            let before_cache = std::fs::read(&cache).ok();
            let prior_writes = mock.state.lock().unwrap().writes.len();
            let issue_preview = mock
                .mcp("update_jira_issue", issue_args.clone(), true)
                .await;
            let page_preview = mock
                .mcp("update_confluence_page", page_args.clone(), true)
                .await;
            assert_eq!(
                mock.cli(&issue_cli, true).await,
                issue_preview,
                "{operation}"
            );
            assert_eq!(mock.cli(&page_cli, true).await, page_preview, "{operation}");
            assert_eq!(issue_preview["dry_run"], true);
            assert_eq!(issue_preview["preview"]["changed"], true);
            assert_eq!(page_preview["preview"]["changed"], true);
            assert_eq!(page_preview["preview"]["prev_version"], version);
            assert_eq!(page_preview["preview"]["next_version"], version + 1);
            assert_eq!(mock.state.lock().unwrap().writes.len(), prior_writes);
            assert_eq!(
                std::fs::read(&cache).ok(),
                before_cache,
                "{operation} updated the cache"
            );
            if operation == "append" {
                assert!(before_cache.is_none());
            } else {
                assert!(before_cache.is_some());
            }
            issue_args["dry_run"] = json!(false);
            page_args["dry_run"] = json!(false);
            mock.mcp("update_jira_issue", issue_args, true).await;
            mock.mcp("update_confluence_page", page_args, true).await;
            issue = apply_body_preview(&issue, &issue_preview["preview"]["changes"]["description"]);
            page = apply_body_preview(&page, &page_preview["preview"]["changes"]["body"]);
            let state = mock.state.lock().unwrap();
            assert_eq!(state.fields["description"], issue, "{operation}");
            assert_eq!(native_body(cloud, &state.page_body), page, "{operation}");
            assert_eq!(state.writes.len(), prior_writes + 2);
        }
    }
}

#[tokio::test]
async fn confluence_cloud_section_edit_preserves_native_nodes_and_checks_heading_and_version() {
    let mock = Harness::new(true).await;
    let heading = |text, level| {
        json!({"type":"heading","attrs":{"level":level},
        "content":[{"type":"text","text":text,"marks":[{"type":"strong"}]}]})
    };
    let paragraph = |text| json!({"type":"paragraph","content":[{"type":"text","text":text}]});
    let panel = json!({"type":"panel","attrs":{"panelType":"info","localId":"panel"},"content":[paragraph("Retain panel")]});
    let media = json!({"type":"mediaSingle","attrs":{"layout":"center"},"content":[
        {"type":"media","attrs":{"type":"file","id":"image-id","collection":"fixture"}}
    ]});
    let table = json!({"type":"table","attrs":{"localId":"table-01"},"content":[
        {"type":"tableRow","content":[{"type":"tableCell","content":[paragraph("Keep cell")]}]}
    ]});
    let layout_distinct = json!({"type":"layoutSection","attrs":{"localId":"layout-01"},"content":[
        {"type":"layoutColumn","attrs":{"width":50},
            "content":[heading("Column notes", 2), paragraph("Nested heading stays")]},
        {"type":"layoutColumn","attrs":{"width":50},
            "content":[paragraph("Right column stays")]}
    ]});
    let original = json!({"type":"doc","version":1,"content":[panel, heading("Deploy safely", 2),
        paragraph("Old body"), heading("Old subsection", 3), paragraph("More old body"),
        heading("Next section", 2), media, table, layout_distinct]});
    mock.state.lock().unwrap().page_body = original.to_string();
    for (name, version, error) in [
        ("Missing", 7, "was not found"),
        ("deploy safely", 7, "was not found"),
        ("Deploy safely", 6, "version"),
    ] {
        let response = mock
            .mcp(
                "update_confluence_page",
                json!({"page_id":"701","prev_version":version,
            "replace_section":{"heading":name,"markdown":"Replacement"},"dry_run":true}),
                false,
            )
            .await;
        assert!(response.as_str().unwrap().contains(error), "{response}");
    }

    // Duplicate across two top-level headings -> PUT 0.
    let mut duplicate = original.clone();
    duplicate["content"]
        .as_array_mut()
        .unwrap()
        .push(heading("Deploy safely", 4));
    mock.state.lock().unwrap().page_body = duplicate.to_string();
    let duplicate_error = mock
        .mcp(
            "update_confluence_page",
            json!({"page_id":"701","prev_version":7,
        "replace_section":{"heading":"Deploy safely","markdown":"Replacement"}}),
            false,
        )
        .await;
    assert!(duplicate_error
        .as_str()
        .unwrap()
        .contains("more than one section"));

    // Duplicate across top-level AND layout column -> PUT 0 for both CLI and MCP.
    let mut dup_top_and_col = original.clone();
    dup_top_and_col["content"][8]["content"][0]["content"][0] = heading("Deploy safely", 2);
    mock.state.lock().unwrap().page_body = dup_top_and_col.to_string();
    let dup_mcp = mock
        .mcp(
            "update_confluence_page",
            json!({"page_id":"701","prev_version":7,
        "replace_section":{"heading":"Deploy safely","markdown":"Replacement"}}),
            false,
        )
        .await;
    assert!(dup_mcp.as_str().unwrap().contains("more than one section"));
    let dup_cli = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Deploy safely",
                "Replacement",
                "--json",
            ],
            false,
        )
        .await;
    assert!(dup_cli.as_str().unwrap().contains("more than one section"));
    assert!(mock.state.lock().unwrap().writes.is_empty());

    // Case 1: unique top-level heading succeeds and preserves surrounding nodes including layout.
    mock.state.lock().unwrap().page_body = original.to_string();
    let inputs = json!({"page_id":"701","prev_version":7,"dry_run":true,
        "replace_section":{"heading":"Deploy safely","markdown":"Replacement"}});
    let preview = mock.mcp("update_confluence_page", inputs, true).await;
    let cli_preview = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Deploy safely",
                "Replacement",
                "--dry-run",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(preview, cli_preview);
    assert!(mock.state.lock().unwrap().writes.is_empty());
    let change = &preview["preview"]["changes"]["body"];
    assert_eq!(change["start"], 2);
    assert_eq!(
        change["before"],
        json!(original["content"].as_array().unwrap()[2..5])
    );
    assert_eq!(change["after"], json!([paragraph("Replacement")]));
    let saved = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Deploy safely",
                "Replacement",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(saved["page"]["summary"]["version_number"], 8);
    let restored = native_body(true, saved["page"]["body_raw"].as_str().unwrap());
    assert_eq!(restored, apply_body_preview(&original, change));
    assert_eq!(
        restored["content"].as_array().unwrap()[..2],
        original["content"].as_array().unwrap()[..2]
    );
    assert_eq!(
        restored["content"].as_array().unwrap()[3..],
        original["content"].as_array().unwrap()[5..]
    );
    assert_eq!(mock.state.lock().unwrap().writes.len(), 1);

    let dc = Harness::new(false).await;
    let error = dc
        .mcp(
            "update_confluence_page",
            json!({"page_id":"701","prev_version":7,"dry_run":true,
        "replace_section":{"heading":"Deploy safely","markdown":"Replacement"}}),
            false,
        )
        .await;
    assert!(error
        .as_str()
        .unwrap()
        .contains("requires a Confluence Cloud ADF page"));
    assert!(dc.state.lock().unwrap().writes.is_empty());
}

#[tokio::test]
async fn confluence_cloud_layout_column_section_edit_previews_saves_and_rejects_errors() {
    let mock = Harness::new(true).await;
    let heading = |text, level| {
        json!({"type":"heading","attrs":{"level":level},
        "content":[{"type":"text","text":text,"marks":[{"type":"strong"}]}]})
    };
    let paragraph = |text| json!({"type":"paragraph","content":[{"type":"text","text":text}]});
    let panel = json!({"type":"panel","attrs":{"panelType":"info","localId":"panel-col"},"content":[paragraph("Top panel stays")]});
    let media = json!({"type":"mediaSingle","attrs":{"layout":"center"},"content":[
        {"type":"media","attrs":{"type":"file","id":"img-col","collection":"fixture"}}
    ]});
    let table = json!({"type":"table","attrs":{"localId":"tbl-col"},"content":[
        {"type":"tableRow","content":[{"type":"tableCell","content":[paragraph("Table stays")]}]}
    ]});
    let original = json!({
        "type": "doc",
        "version": 1,
        "content": [
            panel,
            {
                "type": "layoutSection",
                "attrs": { "localId": "ls-main" },
                "content": [
                    {
                        "type": "layoutColumn",
                        "attrs": { "width": 50 },
                        "content": [
                            heading("Column rollout", 2),
                            paragraph("Old left body"),
                            heading("Old detail", 3),
                            paragraph("Old detail body"),
                            heading("Column after", 2),
                            paragraph("Keep after")
                        ]
                    },
                    {
                        "type": "layoutColumn",
                        "attrs": { "width": 50 },
                        "content": [
                            heading("Right notes", 2),
                            paragraph("Right column untouched")
                        ]
                    }
                ]
            },
            media,
            table
        ]
    });
    mock.state.lock().unwrap().page_body = original.to_string();

    // Missing heading, stale version, and duplicate across two columns all perform 0 writes.
    for (name, version, expected_err) in [
        ("Missing column heading", 7, "was not found"),
        ("Column rollout", 6, "version"),
    ] {
        let err = mock
            .mcp(
                "update_confluence_page",
                json!({"page_id":"701","prev_version":version,
                    "replace_section":{"heading":name,"markdown":"Updated"},"dry_run":true}),
                false,
            )
            .await;
        assert!(err.as_str().unwrap().contains(expected_err), "{err}");
    }
    let mut dup_cols = original.clone();
    dup_cols["content"][1]["content"][1]["content"]
        .as_array_mut()
        .unwrap()
        .push(heading("Column rollout", 2));
    mock.state.lock().unwrap().page_body = dup_cols.to_string();
    let dup_err = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Column rollout",
                "Updated",
                "--json",
            ],
            false,
        )
        .await;
    assert!(dup_err.as_str().unwrap().contains("more than one section"));
    assert!(mock.state.lock().unwrap().writes.is_empty());

    // Restore unique layout-column heading; CLI and MCP dry-run match with PUT 0.
    mock.state.lock().unwrap().page_body = original.to_string();
    let mcp_preview = mock
        .mcp(
            "update_confluence_page",
            json!({"page_id":"701","prev_version":7,"dry_run":true,
                "replace_section":{"heading":"Column rollout","markdown":"- updated item"}}),
            true,
        )
        .await;
    let cli_preview = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Column rollout",
                "- updated item",
                "--dry-run",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(mcp_preview, cli_preview);
    assert_eq!(mcp_preview["preview"]["changed"], true);
    assert_eq!(mcp_preview["preview"]["prev_version"], 7);
    assert_eq!(mcp_preview["preview"]["next_version"], 8);
    assert!(mock.state.lock().unwrap().writes.is_empty());
    let change = &mcp_preview["preview"]["changes"]["body"];
    assert_eq!(change["start"], 1);
    let after_col0 = &change["after"][0]["content"][0]["content"];
    assert_eq!(after_col0[0], heading("Column rollout", 2));
    assert_eq!(after_col0[1]["type"], "bulletList");
    assert_eq!(after_col0[2], heading("Column after", 2));
    assert_eq!(after_col0[3], paragraph("Keep after"));

    // Actual edit performs PUT 1, bumps version 7 -> 8, and preserves sibling column & surrounding nodes.
    let saved = mock
        .cli_with_persona(
            "confluence",
            &[
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Column rollout",
                "- updated item",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(saved["page"]["summary"]["version_number"], 8);
    let restored = native_body(true, saved["page"]["body_raw"].as_str().unwrap());
    assert_eq!(restored, apply_body_preview(&original, change));
    assert_eq!(restored["content"][0], original["content"][0]);
    assert_eq!(restored["content"][2], original["content"][2]);
    assert_eq!(restored["content"][3], original["content"][3]);
    assert_eq!(
        restored["content"][1]["attrs"],
        original["content"][1]["attrs"]
    );
    assert_eq!(
        restored["content"][1]["content"][1],
        original["content"][1]["content"][1]
    );
    assert_eq!(mock.state.lock().unwrap().writes.len(), 1);

    // Column-end edit (no next heading in right column) stops at column end and leaves left column intact.
    let saved_right = mock
        .mcp(
            "update_confluence_page",
            json!({"page_id":"701","prev_version":8,
                "replace_section":{"heading":"Right notes","markdown":"Right updated"}}),
            true,
        )
        .await;
    assert_eq!(saved_right["page"]["summary"]["version_number"], 9);
    let right_doc = native_body(true, saved_right["page"]["body_raw"].as_str().unwrap());
    assert_eq!(
        right_doc["content"][1]["content"][0],
        restored["content"][1]["content"][0]
    );
    assert_eq!(
        right_doc["content"][1]["content"][1]["content"],
        json!([heading("Right notes", 2), paragraph("Right updated")])
    );
    assert_eq!(mock.state.lock().unwrap().writes.len(), 2);
}

#[tokio::test]
async fn built_cli_handles_hyphen_text_payloads_and_rejects_missing_value_before_dry_run() {
    let mock = Harness::new(true).await;

    // Missing value before --dry-run MUST fail with 0 writes.
    mock.cli(&["edit", "ATL-1", "--append", "--dry-run"], false)
        .await;
    mock.cli(&["edit", "ATL-1", "--replace", "Fixture", "--dry-run"], false)
        .await;
    mock.cli(
        &[
            "wiki",
            "edit",
            "701",
            "--prev-version",
            "7",
            "--replace-section",
            "Title",
            "--dry-run",
        ],
        false,
    )
    .await;
    assert!(mock.state.lock().unwrap().writes.is_empty());

    // Jira edit --append with two-line Korean Markdown list + --summary with leading hyphen.
    let updated_issue = mock
        .cli(
            &[
                "edit",
                "ATL-1",
                "--summary",
                "- 요약 제목",
                "--append",
                "- 첫째 항목\n- 둘째 항목",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(updated_issue["issue"]["summary"]["summary"], "- 요약 제목");
    let desc = mock.state.lock().unwrap().fields["description"].clone();
    let last_node = desc["content"].as_array().unwrap().last().unwrap();
    assert_eq!(last_node["type"], "bulletList");
    assert_eq!(last_node["content"].as_array().unwrap().len(), 2);
    assert_eq!(
        last_node["content"][0]["content"][0]["content"][0]["text"],
        "첫째 항목"
    );
    assert_eq!(
        last_node["content"][1]["content"][0]["content"][0]["text"],
        "둘째 항목"
    );

    // Jira --replace with hyphen-leading OLD and NEW values in order.
    mock.cli(
        &["edit", "ATL-1", "--replace", "첫째 항목", "- 바뀐 항목", "--json"],
        true,
    )
    .await;
    mock.cli(
        &["edit", "ATL-1", "--replace", "- 바뀐 항목", "- 최종 항목", "--json"],
        true,
    )
    .await;
    let desc_after = mock.state.lock().unwrap().fields["description"].clone();
    assert_eq!(
        desc_after["content"].as_array().unwrap().last().unwrap()["content"][0]["content"][0]
            ["content"][0]["text"],
        "- 최종 항목"
    );

    // Jira log-work --comment with leading hyphen.
    let worklog = mock
        .cli(
            &[
                "log-work",
                "ATL-1",
                "--seconds",
                "60",
                "--comment",
                "- 작업 기록",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(worklog["worklog"]["timeSpentSeconds"], 60);
    assert_eq!(worklog["worklog"]["comment"]["content"][0]["type"], "bulletList");

    // Confluence wiki edit with hyphen title and two-line list append.
    let updated_page = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--title",
                "- 목록 제목",
                "--append",
                "- 항목 A\n- 항목 B",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(updated_page["page"]["summary"]["title"], "- 목록 제목");
    assert_eq!(updated_page["page"]["summary"]["version_number"], 8);
    let page_doc = native_body(true, updated_page["page"]["body_raw"].as_str().unwrap());
    let page_list = page_doc["content"].as_array().unwrap().last().unwrap();
    assert_eq!(page_list["type"], "bulletList");
    assert_eq!(page_list["content"].as_array().unwrap().len(), 2);

    // Exact write count: 3 Jira PUTs + 1 worklog POST + 1 Confluence PUT = 5.
    assert_eq!(mock.state.lock().unwrap().writes.len(), 5);
}

#[tokio::test]
async fn mcp_read_only_filters_catalog_blocks_writes_and_keeps_default_mode_working() {
    let mock = Harness::new(true).await;
    let list_req = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});

    let (full_resp, full_line) = mock.mcp_rpc("jira", &[], list_req.clone()).await;
    let (ro_jira_resp, ro_jira_line) = mock
        .mcp_rpc("jira", &["--read-only"], list_req.clone())
        .await;
    let (ro_conf_resp, ro_conf_line) = mock
        .mcp_rpc("confluence", &["--read-only"], list_req)
        .await;

    let full_tools = full_resp["result"]["tools"].as_array().unwrap();
    let ro_jira_tools = ro_jira_resp["result"]["tools"].as_array().unwrap();
    let ro_conf_tools = ro_conf_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(full_tools.len(), 37);
    assert_eq!(ro_jira_tools.len(), 15);
    assert_eq!(ro_conf_tools.len(), 15);
    assert_eq!(ro_jira_line, ro_conf_line);
    assert!(ro_jira_line.len() < full_line.len() / 2);

    // Allowed read tools succeed in --read-only mode against the loopback mock.
    let myself = mock
        .mcp_with_options("jira", &["--read-only"], "get_myself", json!({}), true)
        .await;
    assert_eq!(myself["user"]["id"], "acc-test");
    assert_eq!(myself["user"]["display_name"], "Test User");
    let issue = mock
        .mcp_with_options(
            "jira",
            &["--read-only"],
            "get_jira_issue",
            json!({"key": "ATL-1"}),
            true,
        )
        .await;
    assert_eq!(issue["issue"]["key"], "ATL-1");
    let page = mock
        .mcp_with_options(
            "confluence",
            &["--read-only"],
            "get_confluence_page",
            json!({"page_id": "701"}),
            true,
        )
        .await;
    assert_eq!(page["page"]["id"], "701");

    // Direct calls to hidden write, sync, and download tools are rejected BEFORE any HTTP request or file write.
    let dl_dir = tempfile::tempdir().unwrap();
    let requests_before = mock.state.lock().unwrap().requests.len();
    for (persona, tool, args) in [
        (
            "jira",
            "update_jira_issue",
            json!({"key": "ATL-1", "summary": "Blocked"}),
        ),
        (
            "jira",
            "add_jira_worklog",
            json!({"key": "ATL-1", "seconds": 60}),
        ),
        (
            "confluence",
            "update_confluence_page",
            json!({"page_id": "701", "title": "Blocked"}),
        ),
        ("jira", "sync_workspace", json!({})),
        (
            "jira",
            "download_attachment",
            json!({"issue_key": "ATL-1", "dest_dir": dl_dir.path()}),
        ),
    ] {
        let err = mock
            .mcp_with_options(persona, &["--read-only"], tool, args, false)
            .await;
        assert!(
            err.as_str().unwrap().contains("--read-only"),
            "{tool}: {err}"
        );
    }
    assert_eq!(mock.state.lock().unwrap().requests.len(), requests_before);
    assert!(mock.state.lock().unwrap().writes.is_empty());
    assert_eq!(std::fs::read_dir(dl_dir.path()).unwrap().count(), 0);

    // Default full mode still allows existing write calls.
    let updated = mock
        .mcp(
            "update_jira_issue",
            json!({"key": "ATL-1", "summary": "Allowed in full mode"}),
            true,
        )
        .await;
    assert_eq!(updated["issue"]["summary"]["summary"], "Allowed in full mode");
    assert_eq!(mock.state.lock().unwrap().writes.len(), 1);
}

#[tokio::test]
async fn regression_atlassian_bug_02_tree_preservation_and_controls() {
    #[derive(Default)]
    struct Bug02State {
        cloud_page_101_parent: Option<String>,
        cloud_page_101_version: i32,
        cloud_roots: Vec<String>,
        cloud_stale_103_summary: bool,
    }

    async fn run_cli(
        bin: &PathBuf,
        profile: &TempDir,
        url: &str,
        cloud: bool,
        args: &[&str],
    ) -> (bool, Value, String) {
        let mut cmd = Command::new(bin);
        cmd.kill_on_drop(true)
            .current_dir(profile.path())
            .env("ATLASSIAN_CONFIG_DIR", profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("ATLASSIAN_SITE_URL", url)
            .env(
                "ATLASSIAN_CONFLUENCE_URL",
                if cloud {
                    format!("{url}/wiki")
                } else {
                    url.to_string()
                },
            )
            .env(
                "ATLASSIAN_DEPLOYMENT",
                if cloud { "cloud" } else { "datacenter" },
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE");
        if cloud {
            cmd.env("ATLASSIAN_EMAIL", "test@example.invalid")
                .env("ATLASSIAN_API_TOKEN", "fixture-token");
        } else {
            cmd.env("ATLASSIAN_PAT", "fixture-pat");
        }
        let output = tokio::time::timeout(Duration::from_secs(20), cmd.args(args).output())
            .await
            .expect("CLI timed out")
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout_val = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (output.status.success(), stdout_val, stderr)
    }

    let bin = std::env::var_os("ATLASSIAN_WORKFLOW_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_atlassian")));

    // ---------------- Case 1: Data Center v3 ----------------
    let dc_router = Router::new().fallback(|method: Method, uri: Uri| async move {
        let path = uri.path();
        if method != Method::GET {
            return (StatusCode::METHOD_NOT_ALLOWED, format!("Unexpected {method} {path}")).into_response();
        }
        if path == "/rest/api/content/101" {
            return Json(json!({
                "id": "101",
                "type": "page",
                "status": "current",
                "title": "DC Child v3",
                "space": { "id": 5001, "key": "ENG", "name": "Engineering" },
                "version": { "number": 3, "when": "2026-11-15T10:00:00Z", "by": { "displayName": "Alice" } },
                "ancestors": [{ "id": "100", "title": "DC Root" }],
                "body": { "storage": { "representation": "storage", "value": "<p>DC v3 child body</p>" } },
                "metadata": { "labels": { "results": [] } },
                "children": { "attachment": { "results": [] } },
                "_links": { "webui": "/display/ENG/DC+Child+v3" }
            }))
            .into_response();
        }
        if path == "/rest/api/content/101/child/comment" {
            return Json(json!({ "results": [] })).into_response();
        }
        if path == "/rest/api/content/100/child/page" {
            // Real DC child listing omits ancestors
            return Json(json!({
                "results": [{
                    "id": "101",
                    "type": "page",
                    "status": "current",
                    "title": "DC Child v3",
                    "space": { "id": 5001, "key": "ENG" },
                    "version": { "number": 3, "when": "2026-11-15T10:00:00Z" },
                    "_links": { "webui": "/display/ENG/DC+Child+v3" }
                }]
            }))
            .into_response();
        }
        if path == "/rest/api/content/search" {
            // Real DC CQL search omits ancestors
            return Json(json!({
                "results": [{
                    "id": "101",
                    "type": "page",
                    "status": "current",
                    "title": "DC Child v3",
                    "space": { "id": 5001, "key": "ENG" },
                    "version": { "number": 3, "when": "2026-11-15T10:00:00Z" },
                    "_links": { "webui": "/display/ENG/DC+Child+v3" }
                }]
            }))
            .into_response();
        }
        if path == "/rest/api/space/ENG/content/page" {
            return Json(json!({
                "results": [{
                    "id": "100",
                    "type": "page",
                    "status": "current",
                    "title": "DC Root",
                    "space": { "id": 5001, "key": "ENG" },
                    "version": { "number": 1, "when": "2026-11-15T09:00:00Z" },
                    "_links": { "webui": "/display/ENG/DC+Root" }
                }]
            }))
            .into_response();
        }
        (StatusCode::NOT_FOUND, format!("Unhandled DC {path}")).into_response()
    });
    let dc_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dc_url = format!("http://{}", dc_listener.local_addr().unwrap());
    let dc_srv = tokio::spawn(async move { axum::serve(dc_listener, dc_router).await.unwrap() });
    let dc_profile = tempfile::tempdir().unwrap();

    // Detail -> Child listing -> CQL search -> Space root listing
    let (ok, _, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "view", "101", "--json"]).await;
    assert!(ok, "DC view 101 failed: {err}");
    let (ok, children_live, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "pages", "--parent", "100", "--json"]).await;
    assert!(ok, "DC child list failed: {err}");
    assert_eq!(children_live["pages"][0]["parent_id"], "100");
    let (ok, _, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "search", "DC Child", "--remote", "--json"]).await;
    assert!(ok, "DC search failed: {err}");
    let (ok, _, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "pages", "ENG", "--json"]).await;
    assert!(ok, "DC space pages failed: {err}");

    let (ok, dc_children_off, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "pages", "--parent", "100", "--offline", "--json"]).await;
    assert!(ok, "DC offline children failed: {err}");
    let dc_child_ids: Vec<&str> = dc_children_off["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert_eq!(dc_child_ids, vec!["101"], "DC child 101 must remain under parent 100 once");
    let (ok, dc_view_off, err) = run_cli(&bin, &dc_profile, &dc_url, false, &["wiki", "view", "101", "--offline", "--json"]).await;
    assert!(ok, "DC offline view 101 failed after space listing: {err}");
    assert!(
        dc_view_off["page"]["body_markdown"]
            .as_str()
            .unwrap()
            .contains("DC v3 child body")
    );
    dc_srv.abort();

    // ---------------- Case 2 & 3: Cloud v1, Uncached Cloud child, and Controls ----------------
    let cloud_state = Arc::new(Mutex::new(Bug02State {
        cloud_page_101_parent: Some("100".to_string()),
        cloud_page_101_version: 1,
        cloud_roots: vec!["100".to_string(), "105".to_string()],
        cloud_stale_103_summary: false,
    }));
    let cloud_router = Router::new()
        .fallback(
            |State(st): State<Arc<Mutex<Bug02State>>>, method: Method, uri: Uri| async move {
                let path = uri.path();
                if method != Method::GET {
                    return (StatusCode::METHOD_NOT_ALLOWED, format!("Unexpected {method} {path}")).into_response();
                }
                if path == "/wiki/api/v2/spaces" {
                    return Json(json!({
                        "results": [{
                            "id": "5001",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "_links": { "webui": "/spaces/ENG" }
                        }]
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/101" {
                    let g = st.lock().unwrap();
                    return Json(json!({
                        "id": "101",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": g.cloud_page_101_parent,
                        "status": "current",
                        "title": "Cloud Child v1",
                        "version": { "number": g.cloud_page_101_version, "createdAt": "2026-11-15T10:00:00Z", "authorId": "acc-1" },
                        "body": { "atlas_doc_format": { "representation": "atlas_doc_format", "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Cloud v1 child body\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/101" }
                    }))
                    .into_response();
                }
                if path == "/wiki/api/v2/pages/103" {
                    return Json(json!({
                        "id": "103",
                        "spaceId": "5001",
                        "spaceKey": "ENG",
                        "parentId": "100",
                        "status": "current",
                        "title": "Cloud Child v3",
                        "version": { "number": 3, "createdAt": "2026-11-15T12:00:00Z", "authorId": "acc-1" },
                        "body": { "atlas_doc_format": { "representation": "atlas_doc_format", "value": "{\"type\":\"doc\",\"version\":1,\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"Cloud v3 latest body\"}]}]}" } },
                        "_links": { "webui": "/spaces/ENG/pages/103" }
                    }))
                    .into_response();
                }
                if path.ends_with("/labels") || path.ends_with("/attachments") || path.ends_with("/footer-comments") {
                    return Json(json!({ "results": [] })).into_response();
                }
                if path == "/wiki/api/v2/pages/100/children" {
                    // Official Cloud v2 ChildPage schema: no parentId, no spaceKey, no version
                    return Json(json!({
                        "results": [
                            { "id": "101", "status": "current", "title": "Cloud Child v1", "spaceId": "5001", "childPosition": 1 },
                            { "id": "102", "status": "current", "title": "Uncached Cloud Child", "spaceId": "5001", "childPosition": 2 }
                        ]
                    }))
                    .into_response();
                }
                if path == "/wiki/rest/api/search" {
                    let stale_103 = st.lock().unwrap().cloud_stale_103_summary;
                    let mut results = vec![
                        json!({
                            "content": {
                                "id": "101",
                                "type": "page",
                                "title": "Cloud Child v1",
                                "version": { "number": 1 },
                                "_links": { "webui": "/spaces/ENG/pages/101" }
                            },
                            "resultGlobalContainer": { "displayUrl": "/spaces/ENG" },
                            "lastModified": "2026-11-15T10:00:00Z"
                        }),
                        json!({
                            "content": {
                                "id": "102",
                                "type": "page",
                                "title": "Uncached Cloud Child",
                                "_links": { "webui": "/spaces/ENG/pages/102" }
                            },
                            "resultGlobalContainer": { "displayUrl": "/spaces/ENG" },
                            "lastModified": "2026-11-15T10:05:00Z"
                        }),
                    ];
                    if stale_103 {
                        results.push(json!({
                            "content": {
                                "id": "103",
                                "type": "page",
                                "title": "Stale Cloud v1 Summary",
                                "version": { "number": 1 },
                                "_links": { "webui": "/spaces/ENG/pages/103" }
                            },
                            "resultGlobalContainer": { "displayUrl": "/spaces/ENG" },
                            "lastModified": "2026-11-10T00:00:00Z"
                        }));
                    }
                    return Json(json!({ "results": results })).into_response();
                }
                if path == "/wiki/api/v2/spaces/5001/pages" {
                    let roots = st.lock().unwrap().cloud_roots.clone();
                    let results: Vec<Value> = roots
                        .into_iter()
                        .map(|id| {
                            json!({
                                "id": id,
                                "spaceId": "5001",
                                "spaceKey": "ENG",
                                "parentId": Value::Null,
                                "status": "current",
                                "title": format!("Root {id}"),
                                "version": { "number": 1, "createdAt": "2026-11-15T09:00:00Z" },
                                "_links": { "webui": format!("/spaces/ENG/pages/{id}") }
                            })
                        })
                        .collect();
                    return Json(json!({ "results": results })).into_response();
                }
                (StatusCode::NOT_FOUND, format!("Unhandled Cloud {path}")).into_response()
            },
        )
        .with_state(cloud_state.clone());

    let cloud_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cloud_url = format!("http://{}", cloud_listener.local_addr().unwrap());
    let cloud_srv =
        tokio::spawn(async move { axum::serve(cloud_listener, cloud_router).await.unwrap() });
    let cloud_profile = tempfile::tempdir().unwrap();

    // Cloud v1 (101) cached via detail; 102 stays uncached before child listing
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "101", "--json"]).await;
    assert!(ok, "Cloud view 101 failed: {err}");
    let (ok, cloud_children_live, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "--parent", "100", "--json"]).await;
    assert!(ok, "Cloud child list failed: {err}");
    assert_eq!(cloud_children_live["pages"][0]["parent_id"], "100");
    assert_eq!(cloud_children_live["pages"][1]["parent_id"], "100");
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "search", "Cloud Child", "--remote", "--json"]).await;
    assert!(ok, "Cloud search failed: {err}");
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "ENG", "--json"]).await;
    assert!(ok, "Cloud space pages failed: {err}");

    let (ok, cloud_children_off, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "--parent", "100", "--offline", "--json"]).await;
    assert!(ok, "Cloud offline children failed: {err}");
    let child_pages = cloud_children_off["pages"].as_array().unwrap();
    let child_ids: Vec<&str> = child_pages.iter().filter_map(|p| p["id"].as_str()).collect();
    assert_eq!(child_ids, vec!["101", "102"], "both cached v1 (101) and uncached (102) must remain once under 100");
    assert!(child_pages.iter().all(|p| p["space_key"] == "ENG"), "space_key preserved/filled: {child_pages:?}");

    let (ok, cloud_view_off, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "101", "--offline", "--json"]).await;
    assert!(ok, "Cloud offline view 101 failed: {err}");
    assert!(cloud_view_off["page"]["body_markdown"].as_str().unwrap().contains("Cloud v1 child body"));

    // Control 1: Cloud v3 latest body is not replaced or downgraded by stale v1 summary
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "103", "--json"]).await;
    assert!(ok, "Cloud view 103 failed: {err}");
    cloud_state.lock().unwrap().cloud_stale_103_summary = true;
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "search", "Cloud Child", "--remote", "--json"]).await;
    assert!(ok, "Cloud search with stale 103 failed: {err}");
    let (ok, v3_off, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "103", "--offline", "--json"]).await;
    assert!(ok, "Cloud offline view 103 failed: {err}");
    assert_eq!(v3_off["page"]["summary"]["version_number"], 3);
    assert_eq!(v3_off["page"]["summary"]["title"], "Cloud Child v3");
    assert!(v3_off["page"]["body_markdown"].as_str().unwrap().contains("Cloud v3 latest body"));

    // Control 2: Real detail updates hierarchy when page moves to another parent and then to root
    {
        let mut g = cloud_state.lock().unwrap();
        g.cloud_page_101_parent = Some("200".to_string());
        g.cloud_page_101_version = 2;
    }
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "101", "--json"]).await;
    assert!(ok, "Cloud view 101 after parent change failed: {err}");
    let (ok, under_200, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "--parent", "200", "--offline", "--json"]).await;
    assert!(ok, "offline children under 200 failed: {err}");
    assert_eq!(under_200["pages"].as_array().unwrap().len(), 1);
    assert_eq!(under_200["pages"][0]["id"], "101");

    {
        let mut g = cloud_state.lock().unwrap();
        g.cloud_page_101_parent = None;
        g.cloud_page_101_version = 3;
        // Also remove root 105 from space roots to verify complete-scope pruning still deletes removed roots
        g.cloud_roots = vec!["100".to_string(), "101".to_string()];
    }
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "view", "101", "--json"]).await;
    assert!(ok, "Cloud view 101 after move to root failed: {err}");
    let (ok, under_200_after, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "--parent", "200", "--offline", "--json"]).await;
    assert!(ok, "offline children under 200 after move to root failed: {err}");
    assert!(under_200_after["pages"].as_array().unwrap().is_empty());

    // Control 3: Complete-scope prune deletes removed root 105 while keeping 100 & 101
    let (ok, _, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "ENG", "--json"]).await;
    assert!(ok, "Cloud space pages prune check failed: {err}");
    let (ok, roots_off, err) = run_cli(&bin, &cloud_profile, &cloud_url, true, &["wiki", "pages", "ENG", "--offline", "--json"]).await;
    assert!(ok, "Cloud offline roots failed: {err}");
    let mut root_ids: Vec<&str> = roots_off["pages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    root_ids.sort();
    assert_eq!(root_ids, vec!["100", "101"], "removed root 105 should be pruned while 100 and moved root 101 stay");

    cloud_srv.abort();
}

#[tokio::test]
async fn regression_atlassian_bug_07_site_auth_doctor_logout_consistency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_dc_myself_server(
        expected_pat: &'static str,
        user_id: &'static str,
        display_name: &'static str,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        let router = Router::new().route(
            "/rest/api/2/myself",
            axum::routing::get(move |headers: HeaderMap| {
                let hits = hits_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth != format!("Bearer {expected_pat}") {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({ "errorMessages": ["Unauthorized"] })),
                        )
                            .into_response();
                    }
                    Json(json!({
                        "name": user_id,
                        "key": user_id,
                        "displayName": display_name,
                        "emailAddress": format!("{user_id}@example.invalid"),
                        "active": true
                    }))
                    .into_response()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, hits, handle)
    }

    async fn run_cli(bin: &PathBuf, profile: &TempDir, args: &[&str]) -> (bool, Value, String) {
        let mut cmd = Command::new(bin);
        cmd.kill_on_drop(true)
            .current_dir(profile.path())
            .env("ATLASSIAN_CONFIG_DIR", profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE")
            .env_remove("ATLASSIAN_SITE_URL")
            .env_remove("ATLASSIAN_CONFLUENCE_URL")
            .env_remove("ATLASSIAN_DEPLOYMENT")
            .env_remove("ATLASSIAN_EMAIL")
            .env_remove("ATLASSIAN_API_TOKEN")
            .env_remove("ATLASSIAN_PAT")
            .env_remove("JIRA_PAT")
            .env_remove("JIRA_URL")
            .env_remove("CONFLUENCE_URL");
        let output = tokio::time::timeout(Duration::from_secs(20), cmd.args(args).output())
            .await
            .expect("CLI timed out")
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let stdout_val = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        (output.status.success(), stdout_val, stderr)
    }

    let bin = std::env::var_os("ATLASSIAN_WORKFLOW_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_atlassian")));

    let (s1_url, s1_hits, s1_srv) = spawn_dc_myself_server("pat-s1", "u-s1", "Site One User").await;
    let (s2_url, s2_hits, s2_srv) = spawn_dc_myself_server("pat-s2", "u-s2", "Site Two User").await;
    let profile = tempfile::tempdir().unwrap();

    // 1. Register S1 via `sites add` (becomes default)
    let (ok, _, err) = run_cli(
        &bin,
        &profile,
        &[
            "sites",
            "add",
            &s1_url,
            "--id",
            "s1",
            "--deployment",
            "datacenter",
            "--pat",
            "pat-s1",
            "--json",
        ],
    )
    .await;
    assert!(ok, "sites add s1 failed: {err}");

    let (ok, doc1, err) = run_cli(&bin, &profile, &["doctor", "--probe", "--json"]).await;
    assert!(ok, "doctor s1 failed: {err}");
    assert_eq!(doc1["logged_in"], true, "doctor s1 should be logged in: {doc1}");
    assert_eq!(doc1["site_url"], s1_url);
    assert_eq!(doc1["probe_me"]["display_name"], "Site One User");

    // 2. Login to S2 without --site -> S2 becomes default, printed context matches S2
    let (ok, login2, err) = run_cli(
        &bin,
        &profile,
        &[
            "login",
            "--url",
            &s2_url,
            "--deployment",
            "datacenter",
            "--pat",
            "pat-s2",
            "--json",
        ],
    )
    .await;
    assert!(ok, "login s2 failed: {err}");
    assert_eq!(login2["site_url"], s2_url);
    assert_eq!(login2["user"]["display_name"], "Site Two User");

    // 3. Reset counters: default whoami and doctor --probe must hit ONLY S2
    s1_hits.store(0, Ordering::SeqCst);
    s2_hits.store(0, Ordering::SeqCst);

    let (ok, who2, err) = run_cli(&bin, &profile, &["whoami", "--json"]).await;
    assert!(ok, "whoami after S2 login failed: {err}");
    assert_eq!(who2["site_url"], s2_url);
    assert_eq!(who2["user"]["display_name"], "Site Two User");
    assert_eq!(s1_hits.load(Ordering::SeqCst), 0, "default whoami must not hit S1");
    assert_eq!(s2_hits.load(Ordering::SeqCst), 1, "default whoami must hit S2");

    let (ok, doc2, err) = run_cli(&bin, &profile, &["doctor", "--probe", "--json"]).await;
    assert!(ok, "doctor after S2 login failed: {err}");
    assert_eq!(doc2["logged_in"], true);
    assert_eq!(doc2["site_url"], s2_url);
    assert_eq!(doc2["probe_me"]["display_name"], "Site Two User");
    assert_eq!(s1_hits.load(Ordering::SeqCst), 0, "default doctor must not hit S1");
    assert_eq!(s2_hits.load(Ordering::SeqCst), 2, "default doctor must hit S2");

    // 4. Explicit --site s1 doctor shows S1 logged-in status without changing default
    let (ok, doc_s1, err) =
        run_cli(&bin, &profile, &["--site", "s1", "doctor", "--probe", "--json"]).await;
    assert!(ok, "--site s1 doctor failed: {err}");
    assert_eq!(doc_s1["logged_in"], true, "{doc_s1}");
    assert_eq!(doc_s1["site_url"], s1_url);
    assert_eq!(doc_s1["probe_me"]["display_name"], "Site One User");

    // Explicit --site s1 login also preserves S2 as default
    let (ok, _, err) = run_cli(
        &bin,
        &profile,
        &[
            "--site",
            "s1",
            "login",
            "--url",
            &s1_url,
            "--deployment",
            "datacenter",
            "--pat",
            "pat-s1",
            "--json",
        ],
    )
    .await;
    assert!(ok, "--site s1 login failed: {err}");
    let (ok, who_still_s2, err) = run_cli(&bin, &profile, &["whoami", "--json"]).await;
    assert!(ok, "whoami after --site s1 login failed: {err}");
    assert_eq!(who_still_s2["site_url"], s2_url);
    assert_eq!(who_still_s2["user"]["display_name"], "Site Two User");

    // 5. Failed login must NOT change default site or stored credentials
    let (ok, _, _) = run_cli(
        &bin,
        &profile,
        &[
            "login",
            "--url",
            &s1_url,
            "--deployment",
            "datacenter",
            "--pat",
            "wrong-pat",
            "--json",
        ],
    )
    .await;
    assert!(!ok, "failed login must return non-zero");
    let (ok, who_after_fail, err) = run_cli(&bin, &profile, &["whoami", "--json"]).await;
    assert!(ok, "default S2 must stay valid after failed login: {err}");
    assert_eq!(who_after_fail["site_url"], s2_url);
    assert_eq!(who_after_fail["user"]["display_name"], "Site Two User");
    let (ok, s1_after_fail, err) =
        run_cli(&bin, &profile, &["--site", "s1", "whoami", "--json"]).await;
    assert!(ok, "S1 stored creds must stay valid after failed login: {err}");
    assert_eq!(s1_after_fail["user"]["display_name"], "Site One User");

    // 6. Default logout clears S2 only; default whoami fails while --site s1 whoami succeeds
    let (ok, _, err) = run_cli(&bin, &profile, &["logout", "--json"]).await;
    assert!(ok, "default logout failed: {err}");
    let (ok_s2_after, _, _) = run_cli(&bin, &profile, &["whoami", "--json"]).await;
    assert!(!ok_s2_after, "S2 whoami must fail after default logout");
    let (ok_s1_after, who_s1_after, err) =
        run_cli(&bin, &profile, &["--site", "s1", "whoami", "--json"]).await;
    assert!(ok_s1_after, "--site s1 whoami must still succeed: {err}");
    assert_eq!(who_s1_after["user"]["display_name"], "Site One User");

    // Explicit --site s1 logout clears S1
    let (ok, _, err) = run_cli(&bin, &profile, &["--site", "s1", "logout", "--json"]).await;
    assert!(ok, "--site s1 logout failed: {err}");
    let (ok_s1_gone, _, _) =
        run_cli(&bin, &profile, &["--site", "s1", "whoami", "--json"]).await;
    assert!(!ok_s1_gone, "--site s1 whoami must fail after explicit logout");

    // 7. Regression control: legacy single profile flow (login -> whoami -> doctor -> logout -> fail)
    let legacy_profile = tempfile::tempdir().unwrap();
    let (ok, legacy_login, err) = run_cli(
        &bin,
        &legacy_profile,
        &[
            "login",
            "--url",
            &s1_url,
            "--deployment",
            "datacenter",
            "--pat",
            "pat-s1",
            "--json",
        ],
    )
    .await;
    assert!(ok, "legacy login failed: {err}");
    assert_eq!(legacy_login["user"]["display_name"], "Site One User");
    let (ok, legacy_doc, err) =
        run_cli(&bin, &legacy_profile, &["doctor", "--probe", "--json"]).await;
    assert!(ok, "legacy doctor failed: {err}");
    assert_eq!(legacy_doc["logged_in"], true);
    assert_eq!(legacy_doc["probe_me"]["display_name"], "Site One User");
    let (ok, _, err) = run_cli(&bin, &legacy_profile, &["logout", "--json"]).await;
    assert!(ok, "legacy logout failed: {err}");
    let (ok_legacy_after, _, _) = run_cli(&bin, &legacy_profile, &["whoami", "--json"]).await;
    assert!(!ok_legacy_after, "legacy whoami must fail after logout");

    s1_srv.abort();
    s2_srv.abort();
}

#[tokio::test]
async fn regression_atlassian_bug_13_multi_site_sync_failure_propagation() {
    use std::sync::atomic::{AtomicBool, Ordering};

    async fn spawn_sync_site(
        fail: Arc<AtomicBool>,
        proj_key: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let router = Router::new().fallback(move |uri: Uri| {
            let fail = fail.clone();
            async move {
                if fail.load(Ordering::SeqCst) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "errorMessages": ["Simulated upstream 500 failure"] })),
                    )
                        .into_response();
                }
                let path = uri.path();
                if path == "/rest/api/2/project" {
                    return Json(json!([{ "id": "1", "key": proj_key, "name": proj_key }]))
                        .into_response();
                }
                if path == "/rest/agile/1.0/board" {
                    return Json(json!({ "values": [] })).into_response();
                }
                if path == "/rest/api/2/search" {
                    return Json(json!({ "issues": [], "total": 0, "startAt": 0 })).into_response();
                }
                if path == "/rest/api/space" {
                    return Json(json!({ "results": [] })).into_response();
                }
                Json(json!({})).into_response()
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, handle)
    }

    async fn run_cli(
        bin: &PathBuf,
        profile: &TempDir,
        extra_env: &[(&str, &str)],
        args: &[&str],
    ) -> (i32, String, String) {
        let mut cmd = Command::new(bin);
        cmd.kill_on_drop(true)
            .current_dir(profile.path())
            .env("ATLASSIAN_CONFIG_DIR", profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE")
            .env_remove("ATLASSIAN_SITE_URL")
            .env_remove("ATLASSIAN_CONFLUENCE_URL")
            .env_remove("ATLASSIAN_DEPLOYMENT")
            .env_remove("ATLASSIAN_EMAIL")
            .env_remove("ATLASSIAN_API_TOKEN")
            .env_remove("ATLASSIAN_PAT")
            .env_remove("JIRA_PAT")
            .env_remove("JIRA_URL")
            .env_remove("CONFLUENCE_URL");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let output = tokio::time::timeout(Duration::from_secs(20), cmd.args(args).output())
            .await
            .expect("CLI timed out")
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    async fn call_mcp_sync(bin: &PathBuf, profile: &TempDir) -> Value {
        let mut cmd = Command::new(bin);
        cmd.kill_on_drop(true)
            .current_dir(profile.path())
            .env("ATLASSIAN_CONFIG_DIR", profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE")
            .env_remove("ATLASSIAN_SITE_URL")
            .env_remove("ATLASSIAN_CONFLUENCE_URL")
            .env_remove("ATLASSIAN_DEPLOYMENT")
            .env_remove("ATLASSIAN_EMAIL")
            .env_remove("ATLASSIAN_API_TOKEN")
            .env_remove("ATLASSIAN_PAT")
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let req = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sync_workspace","arguments":{}}});
        stdin
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2024-11-05\"}}}}\n\
                     {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{req}\n"
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
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let line = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .find(|l| serde_json::from_str::<Value>(l).map(|v| v["id"] == 2).unwrap_or(false))
            .unwrap();
        serde_json::from_str(line).unwrap()
    }

    let bin = std::env::var_os("ATLASSIAN_WORKFLOW_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_atlassian")));

    let fail_a = Arc::new(AtomicBool::new(true));
    let fail_b = Arc::new(AtomicBool::new(true));
    let (url_a, srv_a) = spawn_sync_site(fail_a.clone(), "ALPHA").await;
    let (url_b, srv_b) = spawn_sync_site(fail_b.clone(), "BETA").await;
    let profile = tempfile::tempdir().unwrap();

    for (id, url) in [("site-a", &url_a), ("site-b", &url_b)] {
        let (code, _, err) = run_cli(
            &bin,
            &profile,
            &[],
            &["sites", "add", url, "--id", id, "--deployment", "datacenter", "--pat", "tok", "--json"],
        )
        .await;
        assert_eq!(code, 0, "sites add {id} failed: {err}");
    }

    // 1. Both sites return 500 -> CLI JSON ok=false & exit code 1, text mode shows failed sites & reasons, MCP isError=true with preserved JSON
    let (code_json, out_json, _err_json) = run_cli(&bin, &profile, &[], &["sync", "--json"]).await;
    assert_eq!(code_json, 1, "all-failed multi-site sync --json must exit 1; stdout={out_json}");
    let parsed: Value = serde_json::from_str(&out_json).expect("preserved JSON summary on stdout");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["summary"].as_str().unwrap().contains("0/2"));
    let sites_arr = parsed["sites"].as_array().unwrap();
    assert_eq!(sites_arr.len(), 2);
    assert!(sites_arr.iter().all(|s| s["error"].as_str().is_some_and(|e| e.contains("500"))));

    let (code_text, out_text, err_text) = run_cli(&bin, &profile, &[], &["sync"]).await;
    assert_eq!(code_text, 1, "all-failed multi-site sync text mode must exit 1");
    let combined_text = format!("{out_text}\n{err_text}");
    assert!(combined_text.contains("site-a") && combined_text.contains("site-b"), "{combined_text}");
    assert!(combined_text.contains("500") || combined_text.contains("Simulated upstream"), "{combined_text}");

    let mcp_all_fail = call_mcp_sync(&bin, &profile).await;
    assert_eq!(mcp_all_fail["result"]["isError"], true, "MCP sync_workspace must set isError=true when 0/2 succeed: {mcp_all_fail}");
    let mcp_text = mcp_all_fail["result"]["content"][0]["text"].as_str().unwrap();
    let mcp_payload: Value = serde_json::from_str(mcp_text).expect("MCP error preserves JSON summary");
    assert_eq!(mcp_payload["ok"], false);
    assert_eq!(mcp_payload["sites"].as_array().unwrap().len(), 2);

    // 2. Partial success (1/2): site-a succeeds, site-b returns 500 -> exit 0, 1/2, failure detail preserved
    fail_a.store(false, Ordering::SeqCst);
    let (code_partial, out_partial, err_partial) = run_cli(&bin, &profile, &[], &["sync", "--json"]).await;
    assert_eq!(code_partial, 0, "partial 1/2 sync must exit 0: {err_partial}");
    let partial_json: Value = serde_json::from_str(&out_partial).unwrap();
    assert_eq!(partial_json["ok"], true);
    assert!(partial_json["summary"].as_str().unwrap().contains("1/2"));
    let site_b_entry = partial_json["sites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["site_id"] == "site-b")
        .unwrap();
    assert!(site_b_entry["error"].as_str().is_some_and(|e| e.contains("500")));

    // 3. Control: 2/2 success -> exit 0, ok=true, MCP isError=false
    fail_b.store(false, Ordering::SeqCst);
    let (code_ok, out_ok, err_ok) = run_cli(&bin, &profile, &[], &["sync", "--json"]).await;
    assert_eq!(code_ok, 0, "2/2 sync failed: {err_ok}");
    let ok_json: Value = serde_json::from_str(&out_ok).unwrap();
    assert_eq!(ok_json["ok"], true);
    assert!(ok_json["summary"].as_str().unwrap().contains("2/2"));
    let mcp_ok = call_mcp_sync(&bin, &profile).await;
    assert_eq!(mcp_ok["result"]["isError"], false, "{mcp_ok}");

    // 4. Control: Legacy single-site mode (no configured sites) -> failure exits 1, success exits 0
    let legacy_profile = tempfile::tempdir().unwrap();
    fail_a.store(true, Ordering::SeqCst);
    let (code_leg_fail, _, _) = run_cli(
        &bin,
        &legacy_profile,
        &[
            ("ATLASSIAN_SITE_URL", &url_a),
            ("ATLASSIAN_CONFLUENCE_URL", &url_a),
            ("ATLASSIAN_DEPLOYMENT", "datacenter"),
            ("ATLASSIAN_PAT", "tok"),
        ],
        &["sync", "--json"],
    )
    .await;
    assert_eq!(code_leg_fail, 1, "legacy single-site failure must exit 1");

    fail_a.store(false, Ordering::SeqCst);
    let (code_leg_ok, out_leg_ok, err_leg_ok) = run_cli(
        &bin,
        &legacy_profile,
        &[
            ("ATLASSIAN_SITE_URL", &url_a),
            ("ATLASSIAN_CONFLUENCE_URL", &url_a),
            ("ATLASSIAN_DEPLOYMENT", "datacenter"),
            ("ATLASSIAN_PAT", "tok"),
        ],
        &["sync", "--json"],
    )
    .await;
    assert_eq!(code_leg_ok, 0, "legacy single-site success failed: {err_leg_ok}");
    let leg_json: Value = serde_json::from_str(&out_leg_ok).unwrap();
    assert_eq!(leg_json["ok"], true);

    srv_a.abort();
    srv_b.abort();
}

#[tokio::test]
async fn regression_atlassian_bug_06_dc_wiki_plain_text_replace_and_roundtrip() {
    let mock = Harness::new(false).await;
    {
        let mut st = mock.state.lock().unwrap();
        st.fields["description"] = json!(
            "*Check* feature_flag and a+b with max_retries.\nAlert! Watch canary now!\nSee !image.png! and [Docs|https://example.com/feature_flag] and {code}feature_flag{code}.\nDuplicate token here and token again."
        );
    }

    // 1. Replace `feature_flag` in prose -> `feature_flag #1!` (URL & {code} occurrences protected)
    let r1 = mock
        .cli(
            &[
                "edit",
                "ATL-1",
                "--replace",
                "feature_flag",
                "feature_flag #1!",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(r1["ok"], true);
    let view1 = mock.cli(&["view", "ATL-1", "--json"], true).await;
    let md1 = view1["issue"]["description_markdown"].as_str().unwrap();
    assert!(
        md1.contains("feature_flag #1!"),
        "view must show literal 'feature_flag #1!' without leaked backslashes; got: {md1}"
    );
    assert!(
        !md1.contains("\\_") && !md1.contains("\\#") && !md1.contains("\\!"),
        "view must decode wiki escapes; got: {md1}"
    );

    // 2. Second replacement on the same displayed literal `feature_flag #1!` -> `feature_flag #2!`
    let r2 = mock
        .cli(
            &[
                "edit",
                "ATL-1",
                "--replace",
                "feature_flag #1!",
                "feature_flag #2!",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(r2["ok"], true);
    let view2 = mock.cli(&["view", "ATL-1", "--json"], true).await;
    assert!(
        view2["issue"]["description_markdown"]
            .as_str()
            .unwrap()
            .contains("feature_flag #2!")
    );

    // 3. Replace `a+b`, `max_retries`, and `'Alert! Watch canary now!'`
    mock.cli(&["edit", "ATL-1", "--replace", "a+b", "x+y", "--json"], true)
        .await;
    mock.cli(
        &[
            "edit",
            "ATL-1",
            "--replace",
            "max_retries",
            "retry_limit",
            "--json",
        ],
        true,
    )
    .await;
    mock.cli(
        &[
            "edit",
            "ATL-1",
            "--replace",
            "Alert! Watch canary now!",
            "Notice! Watch canary now!",
            "--json",
        ],
        true,
    )
    .await;

    // 4. Control: replace inside *Check* preserves outer bold formatting
    mock.cli(
        &["edit", "ATL-1", "--replace", "Check", "Verify", "--json"],
        true,
    )
    .await;

    // 5. Control: ambiguous text matching twice fails with 0 additional writes
    let writes_before = mock.state.lock().unwrap().writes.len();
    let ambig = mock
        .cli(&["edit", "ATL-1", "--replace", "token", "item", "--json"], false)
        .await;
    assert!(
        ambig.as_str().unwrap().contains("more than one"),
        "expected ambiguity error; got: {ambig}"
    );
    assert_eq!(mock.state.lock().unwrap().writes.len(), writes_before);

    // 6. Control: real !image.png!, link URL, {code}, and outer *...* formatting remain intact
    let final_wiki = mock.state.lock().unwrap().fields["description"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(final_wiki.contains("*Verify*"), "outer bold preserved: {final_wiki}");
    assert!(final_wiki.contains("!image.png!"), "image preserved: {final_wiki}");
    assert!(
        final_wiki.contains("[Docs|https://example.com/feature_flag]"),
        "link URL preserved: {final_wiki}"
    );
    assert!(
        final_wiki.contains("{code}feature_flag{code}"),
        "code block preserved: {final_wiki}"
    );
    let final_view = mock.cli(&["view", "ATL-1", "--json"], true).await;
    let final_md = final_view["issue"]["description_markdown"].as_str().unwrap();
    assert!(final_md.contains("x+y"), "{final_md}");
    assert!(final_md.contains("retry_limit"), "{final_md}");
    assert!(final_md.contains("Notice! Watch canary now!"), "{final_md}");
}

#[tokio::test]
async fn regression_atlassian_bug_08_full_body_adf_roundtrip_and_no_empty_text() {
    fn count_empty_text_nodes(val: &Value) -> usize {
        let mut count = 0;
        if val.get("type").and_then(Value::as_str) == Some("text")
            && val.get("text").and_then(Value::as_str) == Some("")
        {
            count += 1;
        }
        if let Some(arr) = val.as_array() {
            for item in arr {
                count += count_empty_text_nodes(item);
            }
        } else if let Some(obj) = val.as_object() {
            for v in obj.values() {
                count += count_empty_text_nodes(v);
            }
        }
        count
    }

    let mock = Harness::new(true).await;
    let rich_adf = json!({
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "table",
                "attrs": { "isNumberColumnEnabled": false, "layout": "default" },
                "content": [
                    {
                        "type": "tableRow",
                        "content": [
                            { "type": "tableHeader", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "Expr" }] }] },
                            { "type": "tableHeader", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "Desc" }] }] }
                        ]
                    },
                    {
                        "type": "tableRow",
                        "content": [
                            { "type": "tableCell", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "a|b" }] }] },
                            { "type": "tableCell", "content": [{ "type": "paragraph", "content": [{ "type": "text", "text": "logical or" }] }] }
                        ]
                    }
                ]
            },
            {
                "type": "paragraph",
                "content": [
                    { "type": "text", "text": "Code: " },
                    { "type": "text", "text": "`x`", "marks": [{ "type": "code" }] },
                    { "type": "text", "text": " and " },
                    { "type": "text", "text": "npm test", "marks": [{ "type": "code" }] }
                ]
            },
            {
                "type": "bulletList",
                "content": [{
                    "type": "listItem",
                    "content": [
                        { "type": "paragraph", "content": [{ "type": "text", "text": "parent" }] },
                        {
                            "type": "bulletList",
                            "content": [{
                                "type": "listItem",
                                "content": [
                                    { "type": "paragraph", "content": [{ "type": "text", "text": "child" }] }
                                ]
                            }]
                        }
                    ]
                }]
            },
            {
                "type": "mediaSingle",
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "external",
                        "url": "https://example.com/arch.png",
                        "alt": "arch diagram"
                    }
                }]
            },
            {
                "type": "codeBlock",
                "attrs": { "language": "text" },
                "content": []
            }
        ]
    });
    mock.state.lock().unwrap().page_body = serde_json::to_string(&rich_adf).unwrap();

    // 1. view -> append unrelated line -> save full body
    let viewed = mock.cli(&["wiki", "view", "701", "--json"], true).await;
    let orig_md = viewed["page"]["body_markdown"].as_str().unwrap().to_string();
    let edited_md = format!("{orig_md}\n\nAdded unrelated trailing line.");

    let saved = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--body",
                &edited_md,
                "--json",
            ],
            true,
        )
        .await;
    let saved_adf: Value =
        serde_json::from_str(saved["page"]["body_raw"].as_str().unwrap()).unwrap();

    // Zero empty text nodes anywhere in the serialized ADF document
    assert_eq!(
        count_empty_text_nodes(&saved_adf),
        0,
        "ADF document must contain 0 empty text nodes: {saved_adf}"
    );

    let nodes = saved_adf["content"].as_array().unwrap();
    // Node 0: 2-column table with literal `a|b` in first body cell
    assert_eq!(nodes[0]["type"], "table");
    let data_cells = nodes[0]["content"][1]["content"].as_array().unwrap();
    assert_eq!(data_cells.len(), 2, "table must keep 2 columns: {data_cells:?}");
    assert_eq!(data_cells[0]["content"][0]["content"][0]["text"], "a|b");
    assert_eq!(data_cells[1]["content"][0]["content"][0]["text"], "logical or");

    // Node 1: inline code with backticks AND plain inline code preserved
    assert_eq!(nodes[1]["type"], "paragraph");
    let inlines = nodes[1]["content"].as_array().unwrap();
    assert!(
        inlines.iter().any(|n| n["text"] == "`x`"
            && n["marks"]
                .as_array()
                .is_some_and(|m| m.iter().any(|mk| mk["type"] == "code"))),
        "backtick inline code must roundtrip: {inlines:?}"
    );
    assert!(
        inlines.iter().any(|n| n["text"] == "npm test"
            && n["marks"]
                .as_array()
                .is_some_and(|m| m.iter().any(|mk| mk["type"] == "code"))),
        "plain inline code must roundtrip: {inlines:?}"
    );

    // Node 2: nested bulletList preserved inside first listItem
    assert_eq!(nodes[2]["type"], "bulletList");
    assert_eq!(nodes[2]["content"].as_array().unwrap().len(), 1);
    let parent_item = nodes[2]["content"][0]["content"].as_array().unwrap();
    assert_eq!(parent_item[0]["content"][0]["text"], "parent");
    assert_eq!(parent_item[1]["type"], "bulletList", "nested bulletList must stay nested: {parent_item:?}");
    assert_eq!(parent_item[1]["content"][0]["content"][0]["content"][0]["text"], "child");

    // Node 3: external mediaSingle/media preserved in order with URL
    assert_eq!(nodes[3]["type"], "mediaSingle", "external image must stay mediaSingle: {nodes:?}");
    assert_eq!(nodes[3]["content"][0]["type"], "media");
    assert_eq!(nodes[3]["content"][0]["attrs"]["type"], "external");
    assert_eq!(
        nodes[3]["content"][0]["attrs"]["url"],
        "https://example.com/arch.png"
    );

    // Node 4: empty codeBlock has no empty text child
    assert_eq!(nodes[4]["type"], "codeBlock");
    assert!(
        nodes[4]
            .get("content")
            .and_then(Value::as_array)
            .map_or(true, |arr| arr.is_empty()),
        "empty codeBlock must have empty/omitted content: {}",
        nodes[4]
    );
}

#[tokio::test]
async fn regression_atlassian_bug_10_rich_heading_section_selection() {
    let mock = Harness::new(true).await;
    let rich_headings_adf = json!({
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [
                    { "type": "text", "text": "Release " },
                    { "type": "status", "attrs": { "text": "READY", "color": "green" } }
                ]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Old status body" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [
                    { "type": "text", "text": "Ship " },
                    { "type": "date", "attrs": { "timestamp": "1795132800000" } }
                ]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Old date body" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [
                    { "type": "text", "text": "Spec " },
                    { "type": "inlineCard", "attrs": { "url": "https://docs.example.com/rfcs/1" } }
                ]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Old card body" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [
                    { "type": "text", "text": "Launch " },
                    { "type": "emoji", "attrs": { "shortName": ":rocket:", "text": "🚀" } }
                ]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Emoji body" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [
                    { "type": "text", "text": "Plain Bold", "marks": [{ "type": "strong" }] }
                ]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Bold body" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [{ "type": "text", "text": "Dup " }, { "type": "status", "attrs": { "text": "OPEN", "color": "blue" } }]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Dup 1" }] },
            {
                "type": "heading",
                "attrs": { "level": 2 },
                "content": [{ "type": "text", "text": "Dup " }, { "type": "status", "attrs": { "text": "OPEN", "color": "blue" } }]
            },
            { "type": "paragraph", "content": [{ "type": "text", "text": "Dup 2" }] }
        ]
    });
    mock.state.lock().unwrap().page_body = serde_json::to_string(&rich_headings_adf).unwrap();

    let view = mock.cli(&["wiki", "view", "701", "--json"], true).await;
    let toc = view["page"]["toc"].as_array().unwrap();
    let md_headings: Vec<String> = view["page"]["body_markdown"]
        .as_str()
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("## ").map(str::to_string))
        .collect();
    assert_eq!(toc.len(), 7);
    assert_eq!(md_headings.len(), 7);

    // 1. All 3 rich headings (status, date, inlineCard) match by BOTH TOC title AND Markdown heading title
    for (idx, expected_old) in [(0, "Old status body"), (1, "Old date body"), (2, "Old card body")] {
        let toc_title = toc[idx]["text"].as_str().unwrap();
        let md_title = md_headings[idx].as_str();
        for candidate in [toc_title, md_title] {
            let preview = mock
                .cli(
                    &[
                        "wiki",
                        "edit",
                        "701",
                        "--prev-version",
                        "7",
                        "--replace-section",
                        candidate,
                        "New section body",
                        "--dry-run",
                        "--json",
                    ],
                    true,
                )
                .await;
            assert_eq!(preview["preview"]["changed"], true, "heading '{candidate}' failed preview");
            let change = &preview["preview"]["changes"]["body"];
            let before_arr = change["before"].as_array().unwrap();
            assert_eq!(before_arr.len(), 1, "only one section body paragraph should change for '{candidate}'");
            assert_eq!(before_arr[0]["content"][0]["text"], expected_old);
            let after_doc = apply_body_preview(&rich_headings_adf, change);
            // Heading node and next sibling heading remain untouched
            assert_eq!(after_doc["content"][idx * 2], rich_headings_adf["content"][idx * 2]);
            assert_eq!(
                after_doc["content"][idx * 2 + 2],
                rich_headings_adf["content"][idx * 2 + 2]
            );
        }
    }

    // 2. Controls: prefix alone must NOT match; duplicate rendered heading fails with 0 writes; emoji & bold still match
    let prefix_err = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Release",
                "Should fail",
                "--dry-run",
                "--json",
            ],
            false,
        )
        .await;
    assert!(prefix_err.as_str().unwrap().contains("not found"));

    let dup_err = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--replace-section",
                "Dup OPEN",
                "Should fail",
                "--json",
            ],
            false,
        )
        .await;
    assert!(dup_err.as_str().unwrap().contains("more than one"));
    assert!(mock.state.lock().unwrap().writes.is_empty());

    for ok_heading in ["Launch 🚀", "Plain Bold", "**Plain Bold**"] {
        let preview = mock
            .cli(
                &[
                    "wiki",
                    "edit",
                    "701",
                    "--prev-version",
                    "7",
                    "--replace-section",
                    ok_heading,
                    "Control updated",
                    "--dry-run",
                    "--json",
                ],
                true,
            )
            .await;
        assert_eq!(preview["preview"]["changed"], true, "control heading '{ok_heading}' failed");
    }
    assert!(mock.state.lock().unwrap().writes.is_empty());
}
