use serde_json::json;

use crate::harness::{apply_body_preview, native_body, Harness};

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
        let page = mock
            .cli(&["wiki", "view", "701", "--json", "--full"], true)
            .await;
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


#[tokio::test]
async fn detail_defaults_to_compact_on_cli_and_mcp_for_live_and_offline() {
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
                vec!["view", "ATL-1", "--json", "--full"],
            ),
            (
                "get_confluence_page",
                "page",
                "page_id",
                "701",
                "body_markdown",
                "body_raw",
                vec!["wiki", "view", "701", "--json", "--full"],
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
                    if field == "issue" { 6 } else { 8 }
                );
                assert_eq!(default[field][markdown], expected[field][markdown]);
                assert_eq!(full[field][markdown], default[field][markdown]);
                assert_eq!(full[field][raw], expected[field][raw]);
                assert_eq!(full[field]["summary"], expected[field]["summary"]);

                let mut cli_default_args: Vec<&str> = cli_args[..cli_args.len() - 1].to_vec();
                if offline {
                    cli_default_args.push("--offline");
                }
                let mut cli_default = mock.cli(&cli_default_args, true).await;
                let mut mcp_without_hint = default.clone();
                cli_default[field].as_object_mut().unwrap().remove("hint");
                mcp_without_hint[field]
                    .as_object_mut()
                    .unwrap()
                    .remove("hint");
                assert_eq!(cli_default, mcp_without_hint, "{tool}, offline={offline}");
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
                "--full",
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
                "--full",
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
            json!({"page_id":"701","prev_version":8,"compact":false,
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
                "--full",
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
                "--full",
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
            json!({"key": "ATL-1", "summary": "Allowed in full mode", "compact": false}),
            true,
        )
        .await;
    assert_eq!(updated["issue"]["summary"]["summary"], "Allowed in full mode");
    assert_eq!(mock.state.lock().unwrap().writes.len(), 1);
}

