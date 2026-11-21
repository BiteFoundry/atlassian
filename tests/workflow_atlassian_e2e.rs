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
    version: i32,
    sprint: i32,
    epic: String,
    labels: Vec<String>,
    attachments: Vec<Value>,
    worklogs: Vec<Value>,
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
            version: 7,
            sprint: 4,
            epic: String::new(),
            labels: vec!["keep".into(), "legacy/ops".into()],
            attachments: Vec::new(),
            worklogs: Vec::new(),
            writes: Vec::new(),
        }
    }

    fn page(&self, id: &str) -> Value {
        let parent = (id == "701").then_some(self.parent);
        let version = if id == "701" { self.version } else { 1 };
        if self.cloud {
            json!({"id": id, "spaceId": "77", "spaceKey": "ENG", "parentId": parent,
                "title": format!("Page {id}"), "status": "current", "version": {"number": version},
                "body": {"atlas_doc_format": {"value": ADF}}})
        } else {
            let ancestors: Vec<_> = parent.into_iter().map(|p| json!({"id": p})).collect();
            json!({"id": id, "space": {"id": 77, "key": if id == "704" {"OTHER"} else {"ENG"}},
                "title": format!("Page {id}"), "type": "page", "version": {"number": version},
                "ancestors": ancestors, "body": {"storage": {"value": XHTML}},
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
    if method != Method::GET {
        state
            .writes
            .push((method.to_string(), path.to_string(), body.clone()));
    }
    let get = method == Method::GET;

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
                .all(|name| name == "parent" || name.starts_with("customfield_")));
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
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_jira")));
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

    async fn cli(&self, args: &[&str], success: bool) -> Value {
        let result =
            tokio::time::timeout(Duration::from_secs(20), self.process().args(args).output())
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

    async fn mcp(&self, name: &str, args: Value, success: bool) -> Value {
        let mut cmd = self.process();
        let mut child = cmd
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let call = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":args}});
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2024-11-05\"}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n{call}\n"
        ).as_bytes()).await.unwrap();
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
        let response = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|result| result["id"] == 2)
            .unwrap();
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
