use std::{path::PathBuf, time::Duration};

use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::{net::TcpListener, process::Command};

use crate::harness::Harness;

#[tokio::test]
async fn cli_view_json_is_compact_and_full_restores_structure() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;

        let issue_default = mock.cli(&["view", "ATL-1", "--json"], true).await;
        let issue_compact = mock
            .cli(&["view", "ATL-1", "--json", "--compact"], true)
            .await;
        let issue_full = mock.cli(&["view", "ATL-1", "--json", "--full"], true).await;
        assert_eq!(issue_default, issue_compact);
        assert_eq!(
            issue_default["issue"]["hint"],
            "atlassian view ATL-1 --json --fields <field> (or --full)"
        );
        assert!(issue_default["issue"]["omitted"].is_object());
        assert!(issue_default["issue"].get("description_raw").is_none());
        assert!(issue_full["issue"]["description_raw"].is_string());

        let issue_off_default = mock
            .cli(&["view", "ATL-1", "--json", "--offline"], true)
            .await;
        let issue_off_compact = mock
            .cli(&["view", "ATL-1", "--json", "--compact", "--offline"], true)
            .await;
        let issue_off_full = mock
            .cli(&["view", "ATL-1", "--json", "--full", "--offline"], true)
            .await;
        assert_eq!(issue_off_default, issue_off_compact);
        assert_eq!(
            issue_off_default["issue"]["hint"],
            "atlassian view ATL-1 --json --fields <field> (or --full) --offline"
        );
        assert!(issue_off_full["issue"]["description_raw"].is_string());

        let page_default = mock.cli(&["wiki", "view", "701", "--json"], true).await;
        let page_compact = mock
            .cli(&["wiki", "view", "701", "--json", "--compact"], true)
            .await;
        let page_full = mock
            .cli(&["wiki", "view", "701", "--json", "--full"], true)
            .await;
        assert_eq!(page_default, page_compact);
        assert_eq!(
            page_default["page"]["hint"],
            "atlassian wiki view 701 --json --full"
        );
        assert!(page_default["page"]["omitted"].is_object());
        assert!(page_default["page"].get("body_raw").is_none());
        assert!(page_full["page"]["body_raw"].is_string());

        let page_off_default = mock
            .cli(&["wiki", "view", "701", "--json", "--offline"], true)
            .await;
        let page_off_compact = mock
            .cli(
                &["wiki", "view", "701", "--json", "--compact", "--offline"],
                true,
            )
            .await;
        let page_off_full = mock
            .cli(
                &["wiki", "view", "701", "--json", "--full", "--offline"],
                true,
            )
            .await;
        assert_eq!(page_off_default, page_off_compact);
        assert_eq!(
            page_off_default["page"]["hint"],
            "atlassian wiki view 701 --json --full --offline"
        );
        assert!(page_off_full["page"]["body_raw"].is_string());

        assert!(mock.state.lock().unwrap().writes.is_empty());
    }
}

#[tokio::test]
async fn write_responses_are_compact_by_default_and_full_restores_structure() {
    let mock = Harness::new(true).await;

    let created_compact = mock
        .cli(
            &[
                "create",
                "--project",
                "ATL",
                "--summary",
                "Compact create",
                "--body",
                "# Heading\n\nBody text",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(created_compact["ok"], true);
    assert!(created_compact["site_id"].is_string());
    assert_eq!(created_compact["issue"]["key"], "ATL-1");
    assert!(created_compact["issue"]["summary"].is_string());
    assert!(created_compact["issue"].get("description_raw").is_none());
    assert!(created_compact["issue"].get("description_blocks").is_none());
    assert!(created_compact["issue"].get("description_markdown").is_none());

    let created_full = mock
        .cli(
            &[
                "create",
                "--project",
                "ATL",
                "--summary",
                "Full create",
                "--body",
                "# Heading\n\nBody text",
                "--json",
                "--full",
            ],
            true,
        )
        .await;
    assert!(created_full["issue"]["description_raw"].is_string());

    let edited_compact = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "7",
                "--append",
                "More text",
                "--json",
            ],
            true,
        )
        .await;
    assert_eq!(edited_compact["ok"], true);
    assert!(edited_compact["site_id"].is_string());
    assert_eq!(edited_compact["page"]["id"], "701");
    assert_eq!(edited_compact["page"]["version_number"], 8);
    assert!(edited_compact["page"].get("body_raw").is_none());
    assert!(edited_compact["page"].get("blocks").is_none());
    assert!(edited_compact["page"].get("body_markdown").is_none());

    let edited_full = mock
        .cli(
            &[
                "wiki",
                "edit",
                "701",
                "--prev-version",
                "8",
                "--append",
                "Even more text",
                "--json",
                "--full",
            ],
            true,
        )
        .await;
    assert!(edited_full["page"]["body_raw"].is_string());

    let mcp_compact = mock
        .mcp(
            "create_confluence_page",
            json!({
                "space_id_or_key": "ENG",
                "title": "MCP Compact",
                "body_markdown": "# Title\n\nContent"
            }),
            true,
        )
        .await;
    assert_eq!(mcp_compact["ok"], true);
    assert_eq!(mcp_compact["page"]["id"], "701");
    assert!(mcp_compact["page"]["version_number"].is_number());
    assert!(mcp_compact["page"].get("body_raw").is_none());
    assert!(mcp_compact["page"].get("blocks").is_none());
    assert!(mcp_compact["page"].get("body_markdown").is_none());

    let mcp_full = mock
        .mcp(
            "create_confluence_page",
            json!({
                "space_id_or_key": "ENG",
                "title": "MCP Full",
                "body_markdown": "# Title\n\nContent",
                "compact": false
            }),
            true,
        )
        .await;
    assert!(mcp_full["page"]["body_raw"].is_string());
}

#[tokio::test]
async fn assignee_resolution_handles_me_name_fallback_ambiguity_and_exact_id() {
    let mock = Harness::new(true).await;
    let search_count = || {
        mock.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(_, p)| p.contains("/user/assignable/search"))
            .count()
    };

    // 1. Exact ID succeeds with 0 search requests.
    let exact = mock
        .cli(&["assign", "ATL-1", "--to", "acc-bob", "--json"], true)
        .await;
    assert_eq!(exact["ok"], true);
    assert!(exact["site_id"].is_string());
    assert_eq!(exact["assignee_id"], "acc-bob");
    assert!(exact.get("resolved_assignee").is_none());
    assert_eq!(search_count(), 0);

    // 2. Display name fails initial PUT, searches once, retries with acc-bob, succeeds.
    let by_name = mock
        .cli(&["assign", "ATL-1", "--to", "Bob Lee", "--json"], true)
        .await;
    assert_eq!(by_name["ok"], true);
    assert_eq!(by_name["assignee_id"], "acc-bob");
    assert_eq!(by_name["resolved_assignee"]["id"], "acc-bob");
    assert_eq!(by_name["resolved_assignee"]["display_name"], "Bob Lee");
    assert_eq!(search_count(), 1);
    assert_eq!(mock.state.lock().unwrap().fields["assignee"]["accountId"], "acc-bob");

    // 3. Ambiguous name ("Bob" -> Bob Lee & Bob Park) fails with ambiguous_user and leaves assignee unchanged.
    let ambig_out = tokio::time::timeout(
        Duration::from_secs(20),
        mock.process()
            .args(["assign", "ATL-1", "--to", "Bob", "--json"])
            .output(),
    )
    .await
    .expect("CLI timed out")
    .unwrap();
    assert_eq!(ambig_out.status.code(), Some(1));
    let ambig_json: Value = serde_json::from_slice(&ambig_out.stdout).unwrap();
    assert_eq!(ambig_json["ok"], false);
    assert_eq!(ambig_json["error"]["code"], "ambiguous_user");
    assert_eq!(
        ambig_json["error"]["details"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(mock.state.lock().unwrap().fields["assignee"]["accountId"], "acc-bob");

    // 4. MCP create_jira_issue and update_jira_issue resolve assignee_id: "me" to acc-test.
    let mcp_created = mock
        .mcp(
            "create_jira_issue",
            json!({"project_key": "ATL", "summary": "MCP me create", "assignee_id": "me"}),
            true,
        )
        .await;
    assert_eq!(mcp_created["ok"], true);
    assert_eq!(
        mock.state.lock().unwrap().fields["assignee"]["accountId"],
        "acc-test"
    );

    let mcp_updated = mock
        .mcp(
            "update_jira_issue",
            json!({"key": "ATL-1", "assignee_id": "me"}),
            true,
        )
        .await;
    assert_eq!(mcp_updated["ok"], true);
    let last_put = mock
        .state
        .lock()
        .unwrap()
        .writes
        .iter()
        .rev()
        .find(|(m, p, _)| m == "PUT" && p == "/rest/api/3/issue/ATL-1")
        .unwrap()
        .2
        .clone();
    assert_eq!(last_put["fields"]["assignee"]["id"], "acc-test");
}

#[tokio::test]
async fn json_errors_emit_structured_stdout_and_mcp_error_json() {
    let mock = Harness::new(true).await;

    let run_cli_raw = |args: &[&str]| {
        let mut cmd = mock.process();
        cmd.args(args);
        async move {
            tokio::time::timeout(Duration::from_secs(20), cmd.output())
                .await
                .expect("CLI timed out")
                .unwrap()
        }
    };

    // 1. 404 with --json emits structured JSON object on stdout + exit 1.
    let not_found = run_cli_raw(&["view", "ATL-99999", "--json"]).await;
    assert_eq!(not_found.status.code(), Some(1));
    let nf_json: Value = serde_json::from_slice(&not_found.stdout).unwrap();
    assert_eq!(nf_json["ok"], false);
    assert_eq!(nf_json["error"]["code"], "not_found");
    assert_eq!(nf_json["error"]["http_status"], 404);
    assert!(!not_found.stderr.is_empty());

    // 2. Version conflict with --json emits conflict + http_status 409.
    let conflict = run_cli_raw(&[
        "wiki",
        "edit",
        "701",
        "--prev-version",
        "1",
        "--append",
        "stale",
        "--json",
    ])
    .await;
    assert_eq!(conflict.status.code(), Some(1));
    let conflict_json: Value = serde_json::from_slice(&conflict.stdout).unwrap();
    assert_eq!(conflict_json["ok"], false);
    assert_eq!(conflict_json["error"]["code"], "conflict");
    assert_eq!(conflict_json["error"]["http_status"], 409);

    // 3. Same 404 without --json keeps stdout empty.
    let text_nf = run_cli_raw(&["view", "ATL-99999"]).await;
    assert_eq!(text_nf.status.code(), Some(1));
    assert!(
        text_nf.stdout.is_empty(),
        "text mode error must not write stdout: {}",
        String::from_utf8_lossy(&text_nf.stdout)
    );

    // 4. MCP get_jira_issue 404 returns structured JSON in content[0].text with isError: true.
    let mcp_err_text = mock
        .mcp("get_jira_issue", json!({"key": "ATL-99999"}), false)
        .await;
    let mcp_err: Value = serde_json::from_str(mcp_err_text.as_str().unwrap()).unwrap();
    assert_eq!(mcp_err["ok"], false);
    assert_eq!(mcp_err["error"]["code"], "not_found");
    assert_eq!(mcp_err["error"]["http_status"], 404);
}

#[tokio::test]
async fn cli_view_rejects_compact_with_full() {
    let mock = Harness::new(true).await;

    for (args, expected_fragment) in [
        (
            vec!["view", "ATL-1", "--json", "--compact", "--full"],
            "cannot be used with",
        ),
        (vec!["view", "ATL-1", "--full"], "--json"),
    ] {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            mock.process().args(&args).output(),
        )
        .await
        .expect("CLI timed out")
        .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "stdout not empty for {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.trim().is_empty() && stderr.contains(expected_fragment),
            "unexpected stderr for {args:?}: {stderr}"
        );
    }

    assert!(
        mock.state.lock().unwrap().requests.is_empty(),
        "usage error must not issue any upstream requests"
    );
}

#[tokio::test]
async fn detail_marks_failed_subresources_incomplete() {
    for cloud in [true, false] {
        let mock = Harness::new(cloud).await;
        {
            let mut st = mock.state.lock().unwrap();
            st.fail_remotelink = true;
            st.fail_labels = true;
        }

        let issue_compact = mock.cli(&["view", "ATL-1", "--json"], true).await;
        assert!(
            issue_compact["issue"]["omitted"]["linked_pages"].is_null(),
            "cloud={cloud}: {issue_compact}"
        );
        let issue_full = mock
            .cli(&["view", "ATL-1", "--json", "--full"], true)
            .await;
        assert_eq!(
            issue_full["issue"]["incomplete"],
            json!(["linked_pages"]),
            "cloud={cloud}: {issue_full}"
        );

        let page_compact = mock
            .mcp("get_confluence_page", json!({ "page_id": "701" }), true)
            .await;
        assert!(
            page_compact["page"]["omitted"]["labels"].is_null(),
            "cloud={cloud}: {page_compact}"
        );
        let page_full = mock
            .mcp(
                "get_confluence_page",
                json!({ "page_id": "701", "compact": false }),
                true,
            )
            .await;
        assert_eq!(
            page_full["page"]["incomplete"],
            json!(["labels"]),
            "cloud={cloud}: {page_full}"
        );

        // Control: when failures are cleared, counts are integers and `incomplete` is absent
        {
            let mut st = mock.state.lock().unwrap();
            st.fail_remotelink = false;
            st.fail_labels = false;
        }

        let issue_ok_compact = mock.cli(&["view", "ATL-1", "--json"], true).await;
        assert_eq!(issue_ok_compact["issue"]["omitted"]["linked_pages"], 0);
        let issue_ok_full = mock
            .cli(&["view", "ATL-1", "--json", "--full"], true)
            .await;
        assert!(issue_ok_full["issue"].get("incomplete").is_none());

        let page_ok_compact = mock
            .mcp("get_confluence_page", json!({ "page_id": "701" }), true)
            .await;
        assert_eq!(page_ok_compact["page"]["omitted"]["labels"], 2);
        let page_ok_full = mock
            .mcp(
                "get_confluence_page",
                json!({ "page_id": "701", "compact": false }),
                true,
            )
            .await;
        assert!(page_ok_full["page"].get("incomplete").is_none());

        // Truncated issue comments (total 25, returned 20): omitted.comments == 25, full incomplete includes "comments"
        {
            let mut st = mock.state.lock().unwrap();
            let comments: Vec<Value> = (1..=20)
                .map(|i| {
                    json!({
                        "id": format!("c-{i}"),
                        "author": { "accountId": "acc-1", "displayName": "Alice" },
                        "body": if cloud {
                            json!({"version": 1, "type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": format!("Comment {i}")}]}]})
                        } else {
                            json!(format!("Comment {i}"))
                        },
                        "created": "2026-11-20T00:00:00Z",
                        "updated": "2026-11-20T00:00:00Z"
                    })
                })
                .collect();
            st.fields["comment"] = json!({ "total": 25, "maxResults": 20, "startAt": 0, "comments": comments });
        }
        let trunc_compact = mock.cli(&["view", "ATL-1", "--json"], true).await;
        assert_eq!(trunc_compact["issue"]["omitted"]["comments"], 25);
        let trunc_full = mock
            .cli(&["view", "ATL-1", "--json", "--full"], true)
            .await;
        assert_eq!(trunc_full["issue"]["summary"]["comment_count"], 25);
        assert_eq!(trunc_full["issue"]["comments"].as_array().unwrap().len(), 20);
        assert_eq!(trunc_full["issue"]["incomplete"], json!(["comments"]));
    }
}

#[tokio::test]
async fn cli_issues_emits_continuation_and_accepts_tokens() {
    let all_issues: Vec<Value> = (1..=120)
        .map(|i| {
            json!({
                "id": format!("{i}"),
                "key": format!("ENG-{i}"),
                "fields": {
                    "project": { "key": "ENG", "name": "Engineering" },
                    "summary": format!("Issue {i}"),
                    "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                    "issuetype": { "id": "1", "name": "Task", "subtask": false },
                    "updated": format!("2026-11-20T00:{:02}:{:02}Z", i / 60, i % 60)
                }
            })
        })
        .collect();

    let router = Router::new()
        .route(
            "/rest/api/3/search/jql",
            axum::routing::post({
                let issues = all_issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body
                            .get("nextPageToken")
                            .and_then(Value::as_str)
                            .and_then(|t| t.strip_prefix("off:"))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        let slice: Vec<Value> =
                            issues.iter().skip(start).take(max).cloned().collect();
                        let next_tok = if start + slice.len() < issues.len() {
                            Some(format!("off:{}", start + slice.len()))
                        } else {
                            None
                        };
                        let mut resp = json!({ "issues": slice });
                        if let Some(t) = next_tok {
                            resp["nextPageToken"] = json!(t);
                        }
                        Json(resp)
                    }
                }
            }),
        )
        .route(
            "/rest/api/2/search",
            axum::routing::post({
                let issues = all_issues.clone();
                move |Json(body): Json<Value>| {
                    let issues = issues.clone();
                    async move {
                        let max = body["maxResults"].as_u64().unwrap_or(50) as usize;
                        let start = body["startAt"].as_u64().unwrap_or(0) as usize;
                        let slice: Vec<Value> =
                            issues.iter().skip(start).take(max).cloned().collect();
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

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let srv = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let bin = std::env::var_os("ATLASSIAN_WORKFLOW_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_atlassian")));

    async fn run_raw(
        bin: &PathBuf,
        profile: &TempDir,
        url: &str,
        cloud: bool,
        args: &[&str],
    ) -> std::process::Output {
        let mut cmd = Command::new(bin);
        cmd.kill_on_drop(true)
            .current_dir(profile.path())
            .env("ATLASSIAN_CONFIG_DIR", profile.path())
            .env("ATLASSIAN_DISABLE_KEYRING", "1")
            .env("ATLASSIAN_SITE_URL", url)
            .env("ATLASSIAN_CONFLUENCE_URL", url)
            .env(
                "ATLASSIAN_DEPLOYMENT",
                if cloud { "cloud" } else { "datacenter" },
            )
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env_remove("ATLASSIAN_SITE");
        if cloud {
            cmd.env("ATLASSIAN_EMAIL", "test@example.invalid")
                .env("ATLASSIAN_API_TOKEN", "tok");
        } else {
            cmd.env("ATLASSIAN_PAT", "pat");
        }
        tokio::time::timeout(Duration::from_secs(20), cmd.args(args).output())
            .await
            .expect("CLI timed out")
            .unwrap()
    }

    for cloud in [true, false] {
        let profile = tempfile::tempdir().unwrap();
        let out1 = run_raw(
            &bin,
            &profile,
            &url,
            cloud,
            &["issues", "--jql", "project = ENG", "--limit", "500", "--json"],
        )
        .await;
        assert!(
            out1.status.success(),
            "cloud={cloud}: {}",
            String::from_utf8_lossy(&out1.stderr)
        );
        let p1: Value = serde_json::from_slice(&out1.stdout).unwrap();
        assert_eq!(p1["issues"].as_array().unwrap().len(), 100);

        let out2 = if cloud {
            assert_eq!(p1["next_page_token"], "off:100");
            assert!(p1["next_start_at"].is_null());
            assert!(p1["total"].is_null());
            run_raw(
                &bin,
                &profile,
                &url,
                true,
                &[
                    "issues",
                    "--jql",
                    "project = ENG",
                    "--limit",
                    "500",
                    "--page-token",
                    "off:100",
                    "--json",
                ],
            )
            .await
        } else {
            assert!(p1["next_page_token"].is_null());
            assert_eq!(p1["next_start_at"], 100);
            assert_eq!(p1["total"], 120);
            run_raw(
                &bin,
                &profile,
                &url,
                false,
                &[
                    "issues",
                    "--jql",
                    "project = ENG",
                    "--limit",
                    "500",
                    "--start-at",
                    "100",
                    "--json",
                ],
            )
            .await
        };
        assert!(
            out2.status.success(),
            "cloud={cloud} page 2: {}",
            String::from_utf8_lossy(&out2.stderr)
        );
        let p2: Value = serde_json::from_slice(&out2.stdout).unwrap();
        assert_eq!(p2["issues"].as_array().unwrap().len(), 20);
        assert!(p2["next_page_token"].is_null());
        assert!(p2["next_start_at"].is_null());
        if cloud {
            assert!(p2["total"].is_null());
        } else {
            assert_eq!(p2["total"], 120);
        }

        // Error controls: mutually exclusive tokens and offline + start-at
        let err_both = run_raw(
            &bin,
            &profile,
            &url,
            cloud,
            &["issues", "--page-token", "x", "--start-at", "1", "--json"],
        )
        .await;
        assert_eq!(err_both.status.code(), Some(2));
        assert!(err_both.stdout.is_empty());

        let err_off = run_raw(
            &bin,
            &profile,
            &url,
            cloud,
            &["issues", "--offline", "--start-at", "1", "--json"],
        )
        .await;
        assert_eq!(err_off.status.code(), Some(2));
        assert!(err_off.stdout.is_empty());

        // Search --remote / --jql continuation tokens
        let s_out1 = run_raw(
            &bin,
            &profile,
            &url,
            cloud,
            &["search", "project = ENG", "--jql", "--limit", "500", "--json"],
        )
        .await;
        assert!(
            s_out1.status.success(),
            "cloud={cloud} search p1: {}",
            String::from_utf8_lossy(&s_out1.stderr)
        );
        let sp1: Value = serde_json::from_slice(&s_out1.stdout).unwrap();
        assert_eq!(sp1["issues"].as_array().unwrap().len(), 100);
        let s_out2 = if cloud {
            assert_eq!(sp1["next_page_token"], "off:100");
            assert!(sp1["total"].is_null());
            run_raw(
                &bin,
                &profile,
                &url,
                true,
                &[
                    "search",
                    "Fixture",
                    "--remote",
                    "--limit",
                    "500",
                    "--page-token",
                    "off:100",
                    "--json",
                ],
            )
            .await
        } else {
            assert_eq!(sp1["next_start_at"], 100);
            assert_eq!(sp1["total"], 120);
            run_raw(
                &bin,
                &profile,
                &url,
                false,
                &[
                    "search",
                    "Fixture",
                    "--remote",
                    "--limit",
                    "500",
                    "--start-at",
                    "100",
                    "--json",
                ],
            )
            .await
        };
        assert!(
            s_out2.status.success(),
            "cloud={cloud} search p2: {}",
            String::from_utf8_lossy(&s_out2.stderr)
        );
        let sp2: Value = serde_json::from_slice(&s_out2.stdout).unwrap();
        assert_eq!(sp2["issues"].as_array().unwrap().len(), 20);
        assert!(sp2["next_page_token"].is_null());
        assert!(sp2["next_start_at"].is_null());

        // Search without --remote/--jql rejects continuation tokens (exit 1)
        let err_search = run_raw(
            &bin,
            &profile,
            &url,
            cloud,
            &["search", "foo", "--start-at", "1", "--json"],
        )
        .await;
        assert_eq!(err_search.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&err_search.stderr)
                .contains("--page-token and --start-at require --remote or --jql")
        );
    }

    srv.abort();
}
