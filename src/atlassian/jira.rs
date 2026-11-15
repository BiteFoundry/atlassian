//! Jira Platform REST API (Cloud v3 `/rest/api/3/search/jql` + ADF, Data Center v2 `/rest/api/2/search` + Wiki Markup).

use std::collections::HashSet;

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use reqwest::StatusCode;
use serde_json::{json, Map, Value};

use crate::{
    adf::{
        blocks_to_markdown, extract_cross_refs, markdown_to_adf, markdown_to_jira_wiki,
        parse_adf_value, parse_jira_wiki, parse_storage_xhtml,
    },
    atlassian::{
        client::{
            first_nonempty_str, json_id_to_string, parse_attachment_item, parse_user_summary,
            HttpStatusError,
        },
        AtlassianClient,
    },
    config::DeploymentKind,
    models::{
        CrossRefTarget, IssueComment, IssueDetail, IssuePriority, IssueStatus, IssueSummary,
        IssueTransition, IssueType, IssueUpdate, JqlSearchPage, LinkedConfluencePage,
        LinkedIssueItem, OutgoingIssue, ProjectSummary, SavedFilter, StatusCategory,
        SubtaskSummary, UserSummary,
    },
};

const SEARCH_FIELDS: &[&str] = &[
    "summary",
    "status",
    "priority",
    "issuetype",
    "assignee",
    "reporter",
    "parent",
    "labels",
    "comment",
    "attachment",
    "created",
    "updated",
    "customfield_10016",
    "customfield_10020",
];

impl AtlassianClient {
    fn jira_api_version(&self) -> &'static str {
        match self.deployment() {
            DeploymentKind::Cloud => "3",
            DeploymentKind::DataCenter => "2",
        }
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        let base = self.require_site_url()?;
        let val: Value = match self.deployment() {
            DeploymentKind::Cloud => {
                let search_url = format!("{base}/rest/api/3/project/search?maxResults=100");
                match self.get_json::<Value>(&search_url).await {
                    Ok(v) => v,
                    Err(err) => {
                        let should_fallback = err
                            .downcast_ref::<HttpStatusError>()
                            .map(|e| {
                                matches!(
                                    e.status,
                                    StatusCode::NOT_FOUND
                                        | StatusCode::METHOD_NOT_ALLOWED
                                        | StatusCode::BAD_REQUEST
                                )
                            })
                            .unwrap_or(false);
                        if should_fallback {
                            self.get_json::<Value>(&format!("{base}/rest/api/3/project"))
                                .await?
                        } else {
                            return Err(err);
                        }
                    }
                }
            }
            DeploymentKind::DataCenter => {
                self.get_json::<Value>(&format!("{base}/rest/api/2/project"))
                    .await?
            }
        };

        let items = val
            .as_array()
            .or_else(|| val.get("values").and_then(|v| v.as_array()))
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(items.iter().map(parse_project_summary).collect())
    }

    pub async fn search_issues_jql(
        &self,
        jql: &str,
        limit: usize,
        page_token: Option<&str>,
        start_at: Option<usize>,
    ) -> Result<JqlSearchPage> {
        let base = self.require_site_url()?;
        let max_results = limit.clamp(1, 100);

        match self.deployment() {
            DeploymentKind::Cloud => {
                // CRITICAL 2026 CLOUD REQUIREMENT: /rest/api/3/search was removed (410 Gone);
                // Cloud MUST use POST /rest/api/3/search/jql with nextPageToken pagination.
                let url = format!("{base}/rest/api/3/search/jql");
                let mut payload = json!({
                    "jql": jql,
                    "maxResults": max_results,
                    "fields": SEARCH_FIELDS,
                });
                if let Some(token) = page_token.map(str::trim).filter(|s| !s.is_empty()) {
                    payload["nextPageToken"] = Value::String(token.to_string());
                }

                let val: Value = self.post_json(&url, &payload).await?;
                let issues = val
                    .get("issues")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| normalize_issue_summary(item, &base, self.deployment()))
                            .collect()
                    })
                    .unwrap_or_default();
                let next_page_token = val
                    .get("nextPageToken")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let total = val
                    .get("total")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);

                Ok(JqlSearchPage {
                    issues,
                    next_page_token,
                    next_start_at: None,
                    total,
                })
            }
            DeploymentKind::DataCenter => {
                let url = format!("{base}/rest/api/2/search");
                let start = start_at.unwrap_or(0);
                let payload = json!({
                    "jql": jql,
                    "maxResults": max_results,
                    "startAt": start,
                    "fields": SEARCH_FIELDS,
                });

                let val: Value = self.post_json(&url, &payload).await?;
                let issues: Vec<IssueSummary> = val
                    .get("issues")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|item| normalize_issue_summary(item, &base, self.deployment()))
                            .collect()
                    })
                    .unwrap_or_default();
                let total = val
                    .get("total")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize);
                let cur_start = val
                    .get("startAt")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(start);

                let next_start_at = if issues.is_empty() {
                    None
                } else if let Some(t) = total {
                    if cur_start + issues.len() < t {
                        Some(cur_start + issues.len())
                    } else {
                        None
                    }
                } else if issues.len() >= max_results {
                    Some(cur_start + issues.len())
                } else {
                    None
                };

                Ok(JqlSearchPage {
                    issues,
                    next_page_token: None,
                    next_start_at,
                    total,
                })
            }
        }
    }

    pub async fn get_issue(&self, key: &str) -> Result<IssueDetail> {
        let base = self.require_site_url()?;
        let encoded_key = urlencoding::encode(key);
        let url = match self.deployment() {
            DeploymentKind::Cloud => {
                format!("{base}/rest/api/3/issue/{encoded_key}?expand=renderedFields")
            }
            DeploymentKind::DataCenter => format!("{base}/rest/api/2/issue/{encoded_key}"),
        };

        let val: Value = self.get_json(&url).await?;
        let summary = normalize_issue_summary(&val, &base, self.deployment());
        let fields = val.get("fields").unwrap_or(&Value::Null);

        let (description_raw, description_blocks) = match self.deployment() {
            DeploymentKind::Cloud => {
                let desc_val = fields.get("description").unwrap_or(&Value::Null);
                let rendered_html = val
                    .get("renderedFields")
                    .and_then(|r| r.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !desc_val.is_null() {
                    if let Some(s) = desc_val.as_str() {
                        (s.to_string(), parse_jira_wiki(s))
                    } else {
                        let raw = serde_json::to_string(desc_val).unwrap_or_default();
                        let mut blocks = parse_adf_value(desc_val);
                        if blocks.is_empty() && !rendered_html.trim().is_empty() {
                            blocks = parse_storage_xhtml(rendered_html);
                        }
                        (raw, blocks)
                    }
                } else if !rendered_html.trim().is_empty() {
                    (
                        rendered_html.to_string(),
                        parse_storage_xhtml(rendered_html),
                    )
                } else {
                    (String::new(), Vec::new())
                }
            }
            DeploymentKind::DataCenter => {
                let raw = fields
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let blocks = parse_jira_wiki(&raw);
                (raw, blocks)
            }
        };
        let description_markdown = blocks_to_markdown(&description_blocks);

        let fix_versions = extract_name_array(fields.get("fixVersions"));
        let components = extract_name_array(fields.get("components"));
        let subtasks = parse_subtasks(fields.get("subtasks"));
        let linked_issues = parse_linked_issues(fields.get("issuelinks"));
        let attachments = fields
            .get("attachment")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|a| parse_attachment_item(a, &base))
                    .collect()
            })
            .unwrap_or_default();
        let comments: Vec<IssueComment> = fields
            .get("comment")
            .and_then(|c| c.get("comments"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|c| parse_issue_comment(c, &summary.key, self.deployment()))
                    .collect()
            })
            .unwrap_or_default();

        let mut linked_pages = self
            .get_issue_remote_confluence_links(key)
            .await
            .unwrap_or_default();
        collect_confluence_links_from_text(
            &description_blocks,
            &description_raw,
            &description_markdown,
            &comments,
            &mut linked_pages,
        );

        Ok(IssueDetail {
            summary,
            description_raw,
            description_markdown,
            description_blocks,
            fix_versions,
            components,
            subtasks,
            linked_issues,
            linked_pages,
            attachments,
            comments,
        })
    }

    pub async fn create_issue(&self, req: &OutgoingIssue) -> Result<IssueDetail> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let url = format!("{base}/rest/api/{ver}/issue");

        let mut fields = Map::new();
        fields.insert("project".into(), json!({ "key": req.project_key }));
        fields.insert("issuetype".into(), json!({ "name": req.issue_type }));
        fields.insert("summary".into(), Value::String(req.summary.clone()));
        fields.insert(
            "description".into(),
            match self.deployment() {
                DeploymentKind::Cloud => markdown_to_adf(&req.description_markdown),
                DeploymentKind::DataCenter => {
                    Value::String(markdown_to_jira_wiki(&req.description_markdown))
                }
            },
        );

        if let Some(ref p) = req.priority.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            fields.insert("priority".into(), json!({ "name": p }));
        }
        if let Some(ref aid) = req
            .assignee_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let assignee_obj = match self.deployment() {
                DeploymentKind::Cloud => json!({ "id": aid }),
                DeploymentKind::DataCenter => json!({ "name": aid }),
            };
            fields.insert("assignee".into(), assignee_obj);
        }
        if !req.labels.is_empty() {
            fields.insert("labels".into(), json!(req.labels));
        }
        if let Some(ref parent) = req
            .parent_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            fields.insert("parent".into(), json!({ "key": parent }));
        }

        let created: Value = self
            .post_json(&url, &json!({ "fields": fields }))
            .await?;
        let created_key = first_nonempty_str(&created, &["key", "id"]);
        if created_key.is_empty() {
            bail!("이슈 생성 응답에 key가 없습니다.");
        }
        self.get_issue(&created_key).await
    }

    pub async fn update_issue(&self, key: &str, patch: &IssueUpdate) -> Result<IssueDetail> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}");

        let mut fields = Map::new();
        if let Some(ref summary) = patch.summary {
            fields.insert("summary".into(), Value::String(summary.clone()));
        }
        if let Some(ref desc_md) = patch.description_markdown {
            fields.insert(
                "description".into(),
                match self.deployment() {
                    DeploymentKind::Cloud => markdown_to_adf(desc_md),
                    DeploymentKind::DataCenter => Value::String(markdown_to_jira_wiki(desc_md)),
                },
            );
        }
        if let Some(ref priority) = patch.priority {
            fields.insert("priority".into(), json!({ "name": priority }));
        }
        if let Some(ref assignee_opt) = patch.assignee_id {
            let val = match assignee_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(aid) => match self.deployment() {
                    DeploymentKind::Cloud => json!({ "id": aid }),
                    DeploymentKind::DataCenter => json!({ "name": aid }),
                },
                None => Value::Null,
            };
            fields.insert("assignee".into(), val);
        }
        if let Some(ref labels) = patch.labels {
            fields.insert("labels".into(), json!(labels));
        }

        if !fields.is_empty() {
            self.put_json_empty(&url, &json!({ "fields": fields }))
                .await?;
        }

        self.get_issue(key).await
    }

    pub async fn delete_issue(&self, key: &str) -> Result<()> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}");
        self.delete_req(&url).await
    }

    pub async fn list_transitions(&self, key: &str) -> Result<Vec<IssueTransition>> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/transitions");

        let val: Value = self.get_json(&url).await?;
        let items = val
            .get("transitions")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(items
            .iter()
            .map(|t| {
                let to = t.get("to").unwrap_or(&Value::Null);
                IssueTransition {
                    id: json_id_to_string(t.get("id")),
                    name: first_nonempty_str(t, &["name"]),
                    to_status: parse_issue_status(to),
                }
            })
            .collect())
    }

    pub async fn transition_issue(
        &self,
        key: &str,
        transition_id_or_name: &str,
        comment_markdown: Option<&str>,
    ) -> Result<()> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let target = transition_id_or_name.trim();
        if target.is_empty() {
            bail!("전환(transition) ID 또는 이름을 입력하세요.");
        }

        let resolved_id = if target.chars().all(|c| c.is_ascii_digit()) {
            target.to_string()
        } else {
            let transitions = self.list_transitions(key).await?;
            transitions
                .iter()
                .find(|t| t.id == target)
                .or_else(|| {
                    transitions
                        .iter()
                        .find(|t| t.name.eq_ignore_ascii_case(target))
                })
                .or_else(|| {
                    transitions
                        .iter()
                        .find(|t| t.to_status.name.eq_ignore_ascii_case(target))
                })
                .map(|t| t.id.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "이슈 {key}에서 전환 '{target}'을(를) 찾을 수 없습니다."
                    )
                })?
        };

        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/transitions");
        self.post_json_empty(&url, &json!({ "transition": { "id": resolved_id } }))
            .await?;

        if let Some(comment) = comment_markdown.map(str::trim).filter(|s| !s.is_empty()) {
            self.add_issue_comment(key, comment).await?;
        }

        Ok(())
    }

    pub async fn assign_issue(&self, key: &str, assignee_id: Option<&str>) -> Result<()> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/assignee");

        let clean_id = assignee_id.map(str::trim).filter(|s| !s.is_empty());
        let body = match self.deployment() {
            DeploymentKind::Cloud => json!({ "accountId": clean_id }),
            DeploymentKind::DataCenter => json!({ "name": clean_id }),
        };

        self.put_json_empty(&url, &body).await
    }

    pub async fn list_issue_comments(&self, key: &str) -> Result<Vec<IssueComment>> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/comment");

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("comments")
            .and_then(|v| v.as_array())
            .or_else(|| val.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        Ok(arr
            .iter()
            .map(|c| parse_issue_comment(c, key, self.deployment()))
            .collect())
    }

    pub async fn add_issue_comment(&self, key: &str, body_markdown: &str) -> Result<IssueComment> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/comment");

        let body_val = match self.deployment() {
            DeploymentKind::Cloud => json!({ "body": markdown_to_adf(body_markdown) }),
            DeploymentKind::DataCenter => {
                json!({ "body": markdown_to_jira_wiki(body_markdown) })
            }
        };

        let resp: Value = self.post_json(&url, &body_val).await?;
        Ok(parse_issue_comment(&resp, key, self.deployment()))
    }

    pub async fn search_assignable_users(
        &self,
        project_or_issue_key: &str,
        query: &str,
    ) -> Result<Vec<UserSummary>> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(project_or_issue_key.trim());
        let encoded_query = urlencoding::encode(query.trim());

        let scope_param = if project_or_issue_key.contains('-') {
            format!("issueKey={encoded_key}")
        } else {
            format!("project={encoded_key}")
        };
        let query_param = match self.deployment() {
            DeploymentKind::Cloud => format!("query={encoded_query}"),
            DeploymentKind::DataCenter => format!("username={encoded_query}"),
        };

        let url = format!(
            "{base}/rest/api/{ver}/user/assignable/search?{scope_param}&{query_param}&maxResults=50"
        );
        let val: Value = self.get_json(&url).await?;
        let arr = val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        Ok(arr
            .iter()
            .map(|u| parse_user_summary(u, self.deployment()))
            .collect())
    }

    pub async fn list_favourite_filters(&self) -> Result<Vec<SavedFilter>> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let url = format!("{base}/rest/api/{ver}/filter/favourite");

        let val: Value = self.get_json(&url).await?;
        let arr = val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        Ok(arr
            .iter()
            .map(|f| SavedFilter {
                id: json_id_to_string(f.get("id")),
                name: first_nonempty_str(f, &["name"]),
                jql: first_nonempty_str(f, &["jql"]),
                builtin: false,
            })
            .collect())
    }

    pub async fn get_issue_remote_confluence_links(
        &self,
        key: &str,
    ) -> Result<Vec<LinkedConfluencePage>> {
        let base = self.require_site_url()?;
        let ver = self.jira_api_version();
        let encoded_key = urlencoding::encode(key);
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/remotelink");

        let val: Value = self.get_json(&url).await?;
        let arr = val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let mut out = Vec::new();
        for item in arr {
            let app_type = item
                .get("application")
                .and_then(|a| a.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let obj = item.get("object").unwrap_or(&Value::Null);
            let web_url = first_nonempty_str(obj, &["url"]);
            let title = first_nonempty_str(obj, &["title"]);
            let global_id = item
                .get("globalId")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if app_type.contains("confluence")
                || web_url.contains("/pages/")
                || web_url.contains("pageId=")
                || global_id.contains("pageId=")
            {
                let (url_page_id, space_key) = parse_confluence_url_parts(&web_url);
                let page_id = url_page_id
                    .or_else(|| parse_page_id_from_query(global_id))
                    .unwrap_or_default();
                let resolved_title = if title.is_empty() {
                    web_url.clone()
                } else {
                    title
                };
                push_unique_linked_page(
                    &mut out,
                    LinkedConfluencePage {
                        page_id,
                        title: resolved_title,
                        space_key,
                        web_url,
                    },
                );
            }
        }
        Ok(out)
    }

    pub fn my_open_issues_jql() -> &'static str {
        "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
    }
}

pub(crate) fn normalize_issue_summary(
    val: &Value,
    site_url: &str,
    deployment: DeploymentKind,
) -> IssueSummary {
    let id = json_id_to_string(val.get("id"));
    let key = first_nonempty_str(val, &["key"]);
    let fields = val.get("fields").unwrap_or(&Value::Null);

    let project_key = fields
        .get("project")
        .and_then(|p| p.get("key"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| key.split('-').next().unwrap_or("").to_string());

    let summary = first_nonempty_str(fields, &["summary"]);
    let status = parse_issue_status(fields.get("status").unwrap_or(&Value::Null));

    let priority = fields
        .get("priority")
        .filter(|v| !v.is_null())
        .map(|p| IssuePriority {
            id: json_id_to_string(p.get("id")),
            name: first_nonempty_str(p, &["name"]),
        });

    let it = fields.get("issuetype").unwrap_or(&Value::Null);
    let it_name = first_nonempty_str(it, &["name"]);
    let issue_type = IssueType {
        id: json_id_to_string(it.get("id")),
        name: it_name.clone(),
        subtask: it.get("subtask").and_then(|v| v.as_bool()).unwrap_or(false),
        epic: it.get("hierarchyLevel").and_then(|v| v.as_i64()) == Some(1)
            || it_name.eq_ignore_ascii_case("epic"),
    };

    let assignee = fields
        .get("assignee")
        .filter(|v| !v.is_null())
        .map(|u| parse_user_summary(u, deployment));
    let reporter = fields
        .get("reporter")
        .filter(|v| !v.is_null())
        .map(|u| parse_user_summary(u, deployment));

    let parent = fields.get("parent").filter(|v| !v.is_null());
    let parent_key = parent
        .and_then(|p| p.get("key"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let parent_summary = parent
        .and_then(|p| p.get("fields"))
        .and_then(|f| f.get("summary"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let labels = fields
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let story_points = fields.get("customfield_10016").and_then(|v| v.as_f64());
    let (sprint_id, sprint_name) = parse_sprint_field(fields.get("customfield_10020"));

    let comment_count = fields
        .get("comment")
        .and_then(|c| {
            c.get("total")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .or_else(|| c.get("comments").and_then(|v| v.as_array()).map(|a| a.len()))
        })
        .unwrap_or(0);

    let attachment_count = fields
        .get("attachment")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let created_at = first_nonempty_str(fields, &["created"]);
    let updated_at = first_nonempty_str(fields, &["updated"]);
    let web_url = if !site_url.trim().is_empty() && !key.is_empty() {
        format!("{}/browse/{}", site_url.trim_end_matches('/'), key)
    } else {
        String::new()
    };

    IssueSummary {
        id,
        key,
        project_key,
        summary,
        status,
        priority,
        issue_type,
        assignee,
        reporter,
        parent_key,
        parent_summary,
        labels,
        story_points,
        sprint_id,
        sprint_name,
        comment_count,
        attachment_count,
        created_at,
        updated_at,
        web_url,
    }
}

fn parse_project_summary(val: &Value) -> ProjectSummary {
    let lead_name = val
        .get("lead")
        .and_then(|l| l.get("displayName"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let avatar_url = val.get("avatarUrls").and_then(|urls| {
        ["48x48", "32x32", "24x24", "16x16"]
            .iter()
            .find_map(|k| urls.get(*k).and_then(|v| v.as_str()))
            .map(str::to_string)
    });
    ProjectSummary {
        id: json_id_to_string(val.get("id")),
        key: first_nonempty_str(val, &["key"]),
        name: first_nonempty_str(val, &["name"]),
        project_type: first_nonempty_str(val, &["projectTypeKey"]),
        lead_name,
        avatar_url,
        pinned: false,
    }
}

fn parse_issue_status(val: &Value) -> IssueStatus {
    let cat_key = val
        .get("statusCategory")
        .and_then(|c| {
            c.get("key")
                .or_else(|| c.get("name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");
    IssueStatus {
        id: json_id_to_string(val.get("id")),
        name: first_nonempty_str(val, &["name"]),
        category: StatusCategory::from_atlassian_key(cat_key),
    }
}

fn parse_sprint_field(val: Option<&Value>) -> (Option<i64>, Option<String>) {
    let Some(val) = val else {
        return (None, None);
    };
    if val.is_null() {
        return (None, None);
    }

    if let Some(arr) = val.as_array() {
        if arr.is_empty() {
            return (None, None);
        }
        // Prefer active sprint, else last sprint in list.
        let picked = arr
            .iter()
            .find(|item| {
                item.get("state")
                    .and_then(|v| v.as_str())
                    .map(|s| s.eq_ignore_ascii_case("active"))
                    .unwrap_or(false)
            })
            .or_else(|| arr.last())
            .unwrap();
        return extract_single_sprint(picked);
    }

    extract_single_sprint(val)
}

fn extract_single_sprint(val: &Value) -> (Option<i64>, Option<String>) {
    if let Some(obj) = val.as_object() {
        let id = obj.get("id").and_then(|v| v.as_i64());
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return (id, name);
    }
    // Data Center GreenHopper legacy string format: `...Sprint@...[id=42,rapidViewId=1,state=ACTIVE,name=Sprint 42,...]`
    if let Some(raw) = val.as_str() {
        let id = Regex::new(r"\bid=(\d+)")
            .ok()
            .and_then(|re| re.captures(raw))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok());
        let name = Regex::new(r"\bname=([^,\]]+)")
            .ok()
            .and_then(|re| re.captures(raw))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string());
        return (id, name);
    }
    (None, None)
}

fn extract_name_array(val: Option<&Value>) -> Vec<String> {
    val.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_subtasks(val: Option<&Value>) -> Vec<SubtaskSummary> {
    val.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|sub| {
                    let fields = sub.get("fields").unwrap_or(&Value::Null);
                    SubtaskSummary {
                        key: first_nonempty_str(sub, &["key"]),
                        summary: first_nonempty_str(fields, &["summary"]),
                        status: parse_issue_status(fields.get("status").unwrap_or(&Value::Null)),
                        issue_type: fields
                            .get("issuetype")
                            .and_then(|it| it.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        assignee_name: fields
                            .get("assignee")
                            .and_then(|a| a.get("displayName"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_linked_issues(val: Option<&Value>) -> Vec<LinkedIssueItem> {
    let Some(arr) = val.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for link in arr {
        let link_type = link
            .get("type")
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if let Some(outward) = link.get("outwardIssue").filter(|v| !v.is_null()) {
            let relation_label = link
                .get("type")
                .and_then(|t| t.get("outward"))
                .and_then(|v| v.as_str())
                .unwrap_or(&link_type)
                .to_string();
            let fields = outward.get("fields").unwrap_or(&Value::Null);
            out.push(LinkedIssueItem {
                link_type: link_type.clone(),
                direction: "outward".into(),
                relation_label,
                key: first_nonempty_str(outward, &["key"]),
                summary: first_nonempty_str(fields, &["summary"]),
                status: parse_issue_status(fields.get("status").unwrap_or(&Value::Null)),
            });
        }
        if let Some(inward) = link.get("inwardIssue").filter(|v| !v.is_null()) {
            let relation_label = link
                .get("type")
                .and_then(|t| t.get("inward"))
                .and_then(|v| v.as_str())
                .unwrap_or(&link_type)
                .to_string();
            let fields = inward.get("fields").unwrap_or(&Value::Null);
            out.push(LinkedIssueItem {
                link_type,
                direction: "inward".into(),
                relation_label,
                key: first_nonempty_str(inward, &["key"]),
                summary: first_nonempty_str(fields, &["summary"]),
                status: parse_issue_status(fields.get("status").unwrap_or(&Value::Null)),
            });
        }
    }
    out
}

fn parse_issue_comment(val: &Value, issue_key: &str, deployment: DeploymentKind) -> IssueComment {
    let id = json_id_to_string(val.get("id"));
    let author = val
        .get("author")
        .filter(|v| !v.is_null())
        .map(|u| parse_user_summary(u, deployment))
        .unwrap_or_default();

    let body_val = val.get("body").unwrap_or(&Value::Null);
    let (body_raw, blocks) = if let Some(s) = body_val.as_str() {
        (s.to_string(), parse_jira_wiki(s))
    } else if !body_val.is_null() {
        let raw = serde_json::to_string(body_val).unwrap_or_default();
        (raw, parse_adf_value(body_val))
    } else {
        (String::new(), Vec::new())
    };
    let body_markdown = blocks_to_markdown(&blocks);

    IssueComment {
        id,
        issue_key: issue_key.to_string(),
        author,
        body_raw,
        body_markdown,
        blocks,
        created_at: first_nonempty_str(val, &["created"]),
        updated_at: first_nonempty_str(val, &["updated"]),
    }
}

fn collect_confluence_links_from_text(
    desc_blocks: &[crate::models::DocBlock],
    desc_raw: &str,
    desc_md: &str,
    comments: &[IssueComment],
    out: &mut Vec<LinkedConfluencePage>,
) {
    let mut scan_sources: Vec<(&[crate::models::DocBlock], &str)> = vec![
        (desc_blocks, desc_raw),
        (&[], desc_md),
    ];
    for c in comments {
        scan_sources.push((&c.blocks, &c.body_raw));
        scan_sources.push((&[], &c.body_markdown));
    }

    let url_re = Regex::new(
        r#"https?://[^\s"'<>)\]]+(?:/pages/\d+[^\s"'<>)\]]*|pageId=\d+[^\s"'<>)\]]*)"#,
    )
    .ok();

    for (blocks, text) in scan_sources {
        for xref in extract_cross_refs(blocks, text) {
            if let CrossRefTarget::ConfluencePage { page_id, title } = xref {
                if !page_id.is_empty() {
                    push_unique_linked_page(
                        out,
                        LinkedConfluencePage {
                            page_id: page_id.clone(),
                            title: title.unwrap_or_else(|| format!("Page {page_id}")),
                            space_key: None,
                            web_url: String::new(),
                        },
                    );
                }
            }
        }
        if let Some(ref re) = url_re {
            for m in re.find_iter(text) {
                let found_url = m.as_str().trim_end_matches(['.', ',', ';']);
                let (page_id_opt, space_key) = parse_confluence_url_parts(found_url);
                if let Some(page_id) = page_id_opt {
                    push_unique_linked_page(
                        out,
                        LinkedConfluencePage {
                            page_id: page_id.clone(),
                            title: format!("Page {page_id}"),
                            space_key,
                            web_url: found_url.to_string(),
                        },
                    );
                }
            }
        }
    }
}

fn parse_confluence_url_parts(url: &str) -> (Option<String>, Option<String>) {
    let space_key = Regex::new(r"/spaces/([^/?#]+)/pages/")
        .ok()
        .and_then(|re| re.captures(url))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());

    let page_id = Regex::new(r"/pages/(\d+)")
        .ok()
        .and_then(|re| re.captures(url))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .or_else(|| parse_page_id_from_query(url));

    (page_id, space_key)
}

fn parse_page_id_from_query(input: &str) -> Option<String> {
    Regex::new(r"pageId=(\d+)")
        .ok()
        .and_then(|re| re.captures(input))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

fn push_unique_linked_page(out: &mut Vec<LinkedConfluencePage>, candidate: LinkedConfluencePage) {
    let seen_ids: HashSet<String> = out
        .iter()
        .filter(|p| !p.page_id.is_empty())
        .map(|p| p.page_id.clone())
        .collect();
    if !candidate.page_id.is_empty() && seen_ids.contains(&candidate.page_id) {
        if let Some(existing) = out.iter_mut().find(|p| p.page_id == candidate.page_id) {
            if existing.web_url.is_empty() && !candidate.web_url.is_empty() {
                existing.web_url = candidate.web_url;
            }
            if existing.space_key.is_none() && candidate.space_key.is_some() {
                existing.space_key = candidate.space_key;
            }
        }
        return;
    }
    if candidate.page_id.is_empty() && out.iter().any(|p| p.web_url == candidate.web_url) {
        return;
    }
    out.push(candidate);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredCredentials};
    use axum::{
        extract::Path as AxumPath,
        http::StatusCode as AxumStatus,
        routing::{get, post, put},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn make_client(base_url: &str, deployment: DeploymentKind) -> AtlassianClient {
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.to_string(),
                token: "tok".into(),
                deployment,
            },
        );
        AtlassianClient::with_base_urls(base_url, "", deployment, auth).unwrap()
    }

    #[tokio::test]
    async fn test_cloud_search_jql_uses_v3_search_jql_and_next_page_token() {
        let captured_bodies: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured_bodies.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/search/jql",
                post(move |Json(body): Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        cap.lock().unwrap().push(body.clone());
                        let page = body.get("nextPageToken").and_then(|v| v.as_str());
                        if page.is_none() {
                            Json(json!({
                                "issues": [{
                                    "id": "1001",
                                    "key": "PROJ-1",
                                    "fields": {
                                        "summary": "Cloud search v3",
                                        "status": { "id": "1", "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                                        "issuetype": { "id": "10", "name": "Task", "subtask": false },
                                        "labels": ["backend"],
                                        "customfield_10016": 3.0,
                                        "customfield_10020": [{ "id": 7, "name": "Sprint 7", "state": "active" }],
                                        "comment": { "total": 2 },
                                        "attachment": []
                                    }
                                }],
                                "nextPageToken": "tok-page-2",
                                "total": 2
                            }))
                        } else {
                            Json(json!({
                                "issues": [{
                                    "id": "1002",
                                    "key": "PROJ-2",
                                    "fields": {
                                        "summary": "Second page issue",
                                        "status": { "id": "3", "name": "Done", "statusCategory": { "key": "done" } },
                                        "issuetype": { "id": "10", "name": "Bug", "subtask": false }
                                    }
                                }],
                                "total": 2
                            }))
                        }
                    }
                }),
            )
            .route(
                "/rest/api/3/search",
                post(|| async { (AxumStatus::GONE, "410 Gone") }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, DeploymentKind::Cloud);

        let p1 = client
            .search_issues_jql("project = PROJ", 50, None, None)
            .await
            .unwrap();
        assert_eq!(p1.issues.len(), 1);
        assert_eq!(p1.issues[0].key, "PROJ-1");
        assert_eq!(p1.issues[0].status.category, StatusCategory::InProgress);
        assert_eq!(p1.issues[0].story_points, Some(3.0));
        assert_eq!(p1.issues[0].sprint_id, Some(7));
        assert_eq!(p1.next_page_token.as_deref(), Some("tok-page-2"));

        let p2 = client
            .search_issues_jql("project = PROJ", 50, p1.next_page_token.as_deref(), None)
            .await
            .unwrap();
        assert_eq!(p2.issues.len(), 1);
        assert_eq!(p2.issues[0].key, "PROJ-2");
        assert!(p2.next_page_token.is_none());

        let bodies = captured_bodies.lock().unwrap();
        assert!(bodies[0].get("nextPageToken").is_none());
        assert_eq!(bodies[1]["nextPageToken"], "tok-page-2");
    }

    #[tokio::test]
    async fn test_datacenter_search_jql_uses_v2_search_and_start_at() {
        let app = Router::new().route(
            "/rest/api/2/search",
            post(|Json(body): Json<Value>| async move {
                let start_at = body["startAt"].as_u64().unwrap();
                if start_at == 0 {
                    Json(json!({
                        "startAt": 0,
                        "maxResults": 1,
                        "total": 2,
                        "issues": [{
                            "id": "2001",
                            "key": "DC-1",
                            "fields": {
                                "summary": "DC Issue 1",
                                "status": { "id": "1", "name": "Open", "statusCategory": { "key": "new" } },
                                "issuetype": { "id": "3", "name": "Task", "subtask": false }
                            }
                        }]
                    }))
                } else {
                    Json(json!({
                        "startAt": 1,
                        "maxResults": 1,
                        "total": 2,
                        "issues": [{
                            "id": "2002",
                            "key": "DC-2",
                            "fields": {
                                "summary": "DC Issue 2",
                                "status": { "id": "3", "name": "Closed", "statusCategory": { "key": "done" } },
                                "issuetype": { "id": "3", "name": "Task", "subtask": false }
                            }
                        }]
                    }))
                }
            }),
        );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, DeploymentKind::DataCenter);

        let p1 = client
            .search_issues_jql("project = DC", 1, None, Some(0))
            .await
            .unwrap();
        assert_eq!(p1.issues[0].key, "DC-1");
        assert_eq!(p1.next_start_at, Some(1));

        let p2 = client
            .search_issues_jql("project = DC", 1, None, p1.next_start_at)
            .await
            .unwrap();
        assert_eq!(p2.issues[0].key, "DC-2");
        assert!(p2.next_start_at.is_none());
    }

    #[tokio::test]
    async fn test_issue_crud_transitions_assignee_and_remote_confluence_links() {
        let transition_called: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let comment_called: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let assigned_to: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));

        let tc = transition_called.clone();
        let cc = comment_called.clone();
        let at = assigned_to.clone();

        let app = Router::new()
            .route(
                "/rest/api/3/issue",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["fields"]["project"]["key"], "ENG");
                    assert_eq!(body["fields"]["assignee"]["id"], "acc-9");
                    Json(json!({ "id": "500", "key": "ENG-42" }))
                }),
            )
            .route(
                "/rest/api/3/issue/{key}",
                get(|AxumPath(key): AxumPath<String>| async move {
                    Json(json!({
                        "id": "500",
                        "key": key,
                        "fields": {
                            "summary": "Spec issue",
                            "description": {
                                "type": "doc",
                                "version": 1,
                                "content": [{
                                    "type": "paragraph",
                                    "content": [{
                                        "type": "text",
                                        "text": "See https://acme.atlassian.net/wiki/spaces/ENG/pages/998877/Design"
                                    }]
                                }]
                            },
                            "status": { "id": "1", "name": "To Do", "statusCategory": { "key": "new" } },
                            "issuetype": { "id": "1", "name": "Story", "subtask": false },
                            "fixVersions": [{ "name": "v1.0" }],
                            "components": [{ "name": "API" }],
                            "subtasks": [],
                            "issuelinks": [],
                            "attachment": [],
                            "comment": { "comments": [] }
                        }
                    }))
                })
                .put(|AxumPath(key): AxumPath<String>, Json(_body): Json<Value>| async move {
                    let _ = key;
                    AxumStatus::NO_CONTENT
                })
                .delete(|AxumPath(_key): AxumPath<String>| async move { AxumStatus::NO_CONTENT }),
            )
            .route(
                "/rest/api/3/issue/{key}/remotelink",
                get(|| async {
                    Json(json!([
                        {
                            "application": { "type": "com.atlassian.confluence" },
                            "object": {
                                "url": "https://acme.atlassian.net/wiki/spaces/ENG/pages/112233/Architecture",
                                "title": "Architecture Spec"
                            }
                        }
                    ]))
                }),
            )
            .route(
                "/rest/api/3/issue/{key}/transitions",
                get(|| async {
                    Json(json!({
                        "transitions": [
                            { "id": "11", "name": "Start Progress", "to": { "id": "3", "name": "In Progress", "statusCategory": { "key": "indeterminate" } } },
                            { "id": "21", "name": "Complete", "to": { "id": "5", "name": "Done", "statusCategory": { "key": "done" } } }
                        ]
                    }))
                })
                .post(move |Json(body): Json<Value>| {
                    let tc = tc.clone();
                    async move {
                        tc.lock()
                            .unwrap()
                            .push(body["transition"]["id"].as_str().unwrap().to_string());
                        AxumStatus::NO_CONTENT
                    }
                }),
            )
            .route(
                "/rest/api/3/issue/{key}/comment",
                post(move |Json(body): Json<Value>| {
                    let cc = cc.clone();
                    async move {
                        cc.lock().unwrap().push(body["body"].to_string());
                        Json(json!({
                            "id": "c-1",
                            "author": { "accountId": "acc-1", "displayName": "Alice" },
                            "body": body["body"],
                            "created": "2026-11-15T10:00:00Z",
                            "updated": "2026-11-15T10:00:00Z"
                        }))
                    }
                }),
            )
            .route(
                "/rest/api/3/issue/{key}/assignee",
                put(move |Json(body): Json<Value>| {
                    let at = at.clone();
                    async move {
                        *at.lock().unwrap() = Some(body);
                        AxumStatus::NO_CONTENT
                    }
                }),
            )
            .route(
                "/rest/api/3/project/search",
                get(|| async { AxumStatus::NOT_FOUND }),
            )
            .route(
                "/rest/api/3/project",
                get(|| async {
                    Json(json!([
                        { "id": "1", "key": "ENG", "name": "Engineering", "projectTypeKey": "software" }
                    ]))
                }),
            )
            .route(
                "/rest/api/3/filter/favourite",
                get(|| async {
                    Json(json!([
                        { "id": "99", "name": "My Bugs", "jql": "issuetype = Bug" }
                    ]))
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, DeploymentKind::Cloud);

        let projects = client.list_projects().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].key, "ENG");

        let created = client
            .create_issue(&OutgoingIssue {
                project_key: "ENG".into(),
                issue_type: "Story".into(),
                summary: "Spec issue".into(),
                description_markdown: "Hello".into(),
                priority: Some("High".into()),
                assignee_id: Some("acc-9".into()),
                labels: vec!["api".into()],
                parent_key: None,
            })
            .await
            .unwrap();
        assert_eq!(created.summary.key, "ENG-42");
        assert_eq!(created.fix_versions, vec!["v1.0"]);
        assert_eq!(created.components, vec!["API"]);
        assert!(created.linked_pages.iter().any(|p| p.page_id == "112233" && p.title == "Architecture Spec"));
        assert!(created.linked_pages.iter().any(|p| p.page_id == "998877"));

        // Transition by status name ("done") + comment
        client
            .transition_issue("ENG-42", "done", Some("Closing out"))
            .await
            .unwrap();
        // Transition by numeric ID without comment
        client.transition_issue("ENG-42", "11", None).await.unwrap();
        assert_eq!(
            *transition_called.lock().unwrap(),
            vec!["21".to_string(), "11".to_string()]
        );
        assert_eq!(comment_called.lock().unwrap().len(), 1);

        client.assign_issue("ENG-42", Some("acc-77")).await.unwrap();
        assert_eq!(assigned_to.lock().unwrap().as_ref().unwrap()["accountId"], "acc-77");
        client.assign_issue("ENG-42", None).await.unwrap();
        assert!(assigned_to.lock().unwrap().as_ref().unwrap()["accountId"].is_null());

        let updated = client
            .update_issue(
                "ENG-42",
                &IssueUpdate {
                    summary: Some("Updated".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.summary.key, "ENG-42");

        let filters = client.list_favourite_filters().await.unwrap();
        assert_eq!(filters[0].name, "My Bugs");

        client.delete_issue("ENG-42").await.unwrap();
    }
}
