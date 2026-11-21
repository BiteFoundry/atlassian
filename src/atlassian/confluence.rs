//! Confluence REST API (Cloud v2 `/wiki/api/v2` + CQL `/wiki/rest/api/search`, Data Center v1 `/rest/api/content`).

use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    adf::{
        blocks_to_markdown, extract_cross_refs, extract_toc, markdown_to_adf,
        markdown_to_storage_xhtml, parse_adf_str, parse_storage_xhtml,
    },
    atlassian::{
        body::{same_markdown, BodyEdit, BodyEditError},
        client::{
            first_nonempty_str, join_url, json_id_to_string, parse_attachment_item, HttpStatusError,
        },
        AtlassianClient,
    },
    config::DeploymentKind,
    models::{
        ConfluenceComment, CrossRefTarget, OutgoingPage, PageAncestor, PageDetail, PageSummary,
        PageUpdate, SpaceSummary,
    },
};

/// Subresource fetch health flags returned alongside [`PageDetail`] so callers can
/// preserve previously cached labels, attachments, or comments on transient subresource failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageSubresourceStatus {
    pub labels_ok: bool,
    pub attachments_ok: bool,
    pub comments_ok: bool,
}

impl AtlassianClient {
    pub async fn list_spaces(&self, limit: usize) -> Result<Vec<SpaceSummary>> {
        let conf_root = self.require_confluence_root()?;
        let lim = limit.clamp(1, 250);

        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{conf_root}/api/v2/spaces?limit={lim}"),
            DeploymentKind::DataCenter => {
                format!("{conf_root}/rest/api/space?limit={lim}&expand=description.plain,homepage")
            }
        };

        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("results")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        let spaces: Vec<SpaceSummary> = arr
            .iter()
            .map(|s| parse_space_summary(s, &conf_root))
            .collect();
        if let Ok(mut cache) = self.space_id_cache.write() {
            for s in &spaces {
                if !s.key.trim().is_empty() && !s.id.trim().is_empty() {
                    cache.insert(s.key.to_ascii_lowercase(), (s.id.clone(), s.key.clone()));
                }
            }
        }
        Ok(spaces)
    }

    pub async fn list_space_root_pages(
        &self,
        space_id_or_key: &str,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        let conf_root = self.require_confluence_root()?;
        let lim = limit.clamp(1, 250);
        let input = space_id_or_key.trim();
        if input.is_empty() {
            bail!("Confluence 스페이스 키 또는 ID를 입력하세요.");
        }

        match self.deployment() {
            DeploymentKind::Cloud => {
                let (space_id, known_key) = self.resolve_cloud_space_id(input).await?;
                let encoded_id = urlencoding::encode(&space_id);
                let url =
                    format!("{conf_root}/api/v2/spaces/{encoded_id}/pages?depth=root&limit={lim}");
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                Ok(arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_cloud_page_summary(p, &conf_root);
                        if summary.space_key.is_empty() {
                            if let Some(ref k) = known_key {
                                summary.space_key = k.clone();
                            }
                        }
                        summary
                    })
                    .filter(|p| p.parent_id.is_none())
                    .collect())
            }
            DeploymentKind::DataCenter => {
                let encoded_key = urlencoding::encode(input);
                let url = format!(
                    "{conf_root}/rest/api/space/{encoded_key}/content/page?depth=root&limit={lim}&expand=version,space"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .or_else(|| val.get("page").and_then(|p| p.get("results")))
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                Ok(arr
                    .iter()
                    .map(|p| {
                        let mut summary = parse_dc_page_summary(p, &conf_root);
                        if summary.space_key.is_empty() {
                            summary.space_key = input.to_string();
                        }
                        summary
                    })
                    .collect())
            }
        }
    }

    pub async fn list_child_pages(&self, page_id: &str) -> Result<Vec<PageSummary>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!("{conf_root}/api/v2/pages/{encoded_id}/children?limit=100");
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|p| parse_cloud_page_summary(p, &conf_root))
                    .collect())
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}/child/page?limit=100&expand=version,space"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|p| parse_dc_page_summary(p, &conf_root))
                    .collect())
            }
        }
    }

    /// Lightweight single-request Confluence page revision probe (version, title, updated_at;
    /// excludes body, labels, attachments, and comments).
    pub async fn get_page_revision(&self, page_id: &str) -> Result<PageSummary> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!("{conf_root}/api/v2/pages/{encoded_id}");
                let val: Value = self.get_json(&url).await?;
                Ok(parse_cloud_page_summary(&val, &conf_root))
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}?expand=version,space,ancestors"
                );
                let val: Value = self.get_json(&url).await?;
                Ok(parse_dc_page_summary(&val, &conf_root))
            }
        }
    }

    /// Lightweight single-request Confluence page attachments query for panel refresh.
    pub async fn list_page_attachments(
        &self,
        page_id: &str,
    ) -> Result<Vec<crate::models::AttachmentItem>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{conf_root}/api/v2/pages/{encoded_id}/attachments"),
            DeploymentKind::DataCenter => {
                format!("{conf_root}/rest/api/content/{encoded_id}/child/attachment")
            }
        };
        let val: Value = self.get_json(&url).await?;
        let arr = val
            .get("results")
            .and_then(|v| v.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        Ok(arr
            .iter()
            .map(|a| parse_attachment_item(a, &conf_root))
            .collect())
    }

    pub async fn get_page(&self, page_id: &str) -> Result<PageDetail> {
        let (detail, _) = self.get_page_with_meta(page_id).await?;
        Ok(detail)
    }

    pub async fn get_page_with_meta(
        &self,
        page_id: &str,
    ) -> Result<(PageDetail, PageSubresourceStatus)> {
        self.fetch_page(page_id, false).await
    }

    async fn fetch_page(
        &self,
        page_id: &str,
        require_native_body: bool,
    ) -> Result<(PageDetail, PageSubresourceStatus)> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let adf_url = format!(
                    "{conf_root}/api/v2/pages/{encoded_id}?body-format=atlas_doc_format"
                );
                let mut val: Value = self.get_json(&adf_url).await?;
                let mut body_raw = val
                    .get("body")
                    .and_then(|b| b.get("atlas_doc_format"))
                    .and_then(|a| a.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let (body_representation, blocks) = if !body_raw.trim().is_empty() {
                    ("atlas_doc_format".to_string(), parse_adf_str(&body_raw))
                } else {
                    let storage_url =
                        format!("{conf_root}/api/v2/pages/{encoded_id}?body-format=storage");
                    let storage_val: Value = self.get_json(&storage_url).await?;
                    let storage_body = storage_val
                        .pointer("/body/storage/value")
                        .and_then(Value::as_str);
                    if require_native_body && storage_body.is_none() {
                        return Err(
                            BodyEditError("Confluence did not return the original body").into()
                        );
                    }
                    body_raw = storage_body.unwrap_or("").to_owned();
                    if require_native_body || val.get("title").is_none() {
                        val = storage_val;
                    }
                    ("storage".to_string(), parse_storage_xhtml(&body_raw))
                };

                let summary = parse_cloud_page_summary(&val, &conf_root);
                if require_native_body {
                    validate_source_version(&val)?;
                }

                let (labels, labels_ok) = match self
                    .get_json::<Value>(&format!("{conf_root}/api/v2/pages/{encoded_id}/labels"))
                    .await
                {
                    Ok(v) => (
                        v.get("results")
                            .and_then(|r| r.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|l| {
                                        l.get("name").and_then(|n| n.as_str()).map(str::to_string)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        true,
                    ),
                    Err(_) => (Vec::new(), false),
                };

                let (attachments, attachments_ok) = match self
                    .get_json::<Value>(&format!(
                        "{conf_root}/api/v2/pages/{encoded_id}/attachments"
                    ))
                    .await
                {
                    Ok(v) => (
                        v.get("results")
                            .and_then(|r| r.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .map(|a| parse_attachment_item(a, &conf_root))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        true,
                    ),
                    Err(_) => (Vec::new(), false),
                };

                let (comments, comments_ok) = match self.list_page_comments(page_id).await {
                    Ok(c) => (c, true),
                    Err(_) => (Vec::new(), false),
                };
                let body_markdown = blocks_to_markdown(&blocks);
                let toc = extract_toc(&blocks);
                let linked_issue_keys = extract_linked_issue_keys(&blocks, &body_raw);

                Ok((
                    PageDetail {
                        summary,
                        ancestors: Vec::new(),
                        labels,
                        body_raw,
                        body_representation,
                        body_markdown,
                        blocks,
                        toc,
                        attachments,
                        comments,
                        linked_issue_keys,
                    },
                    PageSubresourceStatus {
                        labels_ok,
                        attachments_ok,
                        comments_ok,
                    },
                ))
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}?expand=body.storage,version,space,ancestors,metadata.labels,children.attachment"
                );
                let val: Value = self.get_json(&url).await?;
                let summary = parse_dc_page_summary(&val, &conf_root);
                if require_native_body {
                    validate_source_version(&val)?;
                }

                let ancestors = val
                    .get("ancestors")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|a| PageAncestor {
                                id: json_id_to_string(a.get("id")),
                                title: first_nonempty_str(a, &["title"]),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let labels = val
                    .get("metadata")
                    .and_then(|m| m.get("labels"))
                    .and_then(|l| l.get("results"))
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                let attachments = val
                    .get("children")
                    .and_then(|c| c.get("attachment"))
                    .and_then(|a| a.get("results"))
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|a| parse_attachment_item(a, &conf_root))
                            .collect()
                    })
                    .unwrap_or_default();

                let storage_body = val.pointer("/body/storage/value").and_then(Value::as_str);
                if require_native_body && storage_body.is_none() {
                    return Err(BodyEditError("Confluence did not return the original body").into());
                }
                let body_raw = storage_body.unwrap_or("").to_owned();
                let blocks = parse_storage_xhtml(&body_raw);
                let body_markdown = blocks_to_markdown(&blocks);
                let toc = extract_toc(&blocks);
                let linked_issue_keys = extract_linked_issue_keys(&blocks, &body_raw);
                let (comments, comments_ok) = match self.list_page_comments(page_id).await {
                    Ok(c) => (c, true),
                    Err(_) => (Vec::new(), false),
                };

                Ok((
                    PageDetail {
                        summary,
                        ancestors,
                        labels,
                        body_raw,
                        body_representation: "storage".to_string(),
                        body_markdown,
                        blocks,
                        toc,
                        attachments,
                        comments,
                        linked_issue_keys,
                    },
                    PageSubresourceStatus {
                        labels_ok: true,
                        attachments_ok: true,
                        comments_ok,
                    },
                ))
            }
        }
    }

    pub async fn create_page(&self, req: &OutgoingPage) -> Result<PageDetail> {
        let conf_root = self.require_confluence_root()?;

        let created_id = match self.deployment() {
            DeploymentKind::Cloud => {
                let (space_id, _) = self.resolve_cloud_space_id(&req.space_id_or_key).await?;
                let adf_str = serde_json::to_string(&markdown_to_adf(&req.body_markdown))
                    .context("ADF 직렬화 실패")?;
                let mut payload = Map::new();
                payload.insert("spaceId".into(), Value::String(space_id));
                payload.insert("status".into(), Value::String("current".into()));
                payload.insert("title".into(), Value::String(req.title.clone()));
                if let Some(ref pid) = req
                    .parent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload.insert("parentId".into(), Value::String((*pid).to_string()));
                }
                payload.insert(
                    "body".into(),
                    json!({
                        "representation": "atlas_doc_format",
                        "value": adf_str,
                    }),
                );

                let resp: Value = self
                    .post_json(&format!("{conf_root}/api/v2/pages"), &Value::Object(payload))
                    .await?;
                json_id_to_string(resp.get("id"))
            }
            DeploymentKind::DataCenter => {
                let storage_html = markdown_to_storage_xhtml(&req.body_markdown);
                let mut payload = Map::new();
                payload.insert("type".into(), Value::String("page".into()));
                payload.insert("title".into(), Value::String(req.title.clone()));
                payload.insert("space".into(), json!({ "key": req.space_id_or_key }));
                if let Some(ref pid) = req
                    .parent_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload.insert("ancestors".into(), json!([{ "id": pid }]));
                }
                payload.insert(
                    "body".into(),
                    json!({
                        "storage": {
                            "value": storage_html,
                            "representation": "storage",
                        }
                    }),
                );

                let resp: Value = self
                    .post_json(
                        &format!("{conf_root}/rest/api/content"),
                        &Value::Object(payload),
                    )
                    .await?;
                json_id_to_string(resp.get("id"))
            }
        };

        if created_id.is_empty() {
            bail!("페이지 생성 응답에 id가 없습니다.");
        }
        self.get_page(&created_id).await
    }

    pub(crate) async fn edit_page(
        &self,
        page_id: &str,
        title: Option<String>,
        body: Option<BodyEdit<'_>>,
        prev_version: Option<i32>,
        version_message: Option<String>,
    ) -> Result<PageDetail> {
        if title.is_none() && body.is_none() && version_message.is_none() {
            return Err(BodyEditError("at least one page change must be provided").into());
        }
        if title.as_ref().is_some_and(|title| title.trim().is_empty()) {
            return Err(BodyEditError("title must not be empty").into());
        }
        if body.is_some() && prev_version.unwrap_or(0) <= 0 {
            return Err(BodyEditError(
                "prev_version (>= 1) is required for body edits. Read the current page first and use its version_number.",
            ).into());
        }

        let (current, _) = self.fetch_page(page_id, true).await?;
        let version = current.summary.version_number;
        let prev_version = prev_version.filter(|&v| v > 0).unwrap_or(version);
        check_page_base_version(prev_version, version)?;

        let title = title.unwrap_or_else(|| current.summary.title.clone());
        let mut patch = PageUpdate {
            title,
            prev_version,
            version_message,
            body_raw_passthrough: Some(current.body_raw.clone()),
            body_representation: Some(current.body_representation.clone()),
            ..Default::default()
        };
        let body_changed = match body {
            None => false,
            Some(BodyEdit::Full(md)) if !same_markdown(md, &current.body_markdown) => {
                patch.body_markdown = md.to_owned();
                patch.body_raw_passthrough = None;
                patch.body_representation = None;
                true
            }
            Some(BodyEdit::Full(_)) => false,
            Some(BodyEdit::Append(md)) if md.trim().is_empty() => false,
            Some(edit) => {
                let updated = match current.body_representation.as_str() {
                    "atlas_doc_format" => edit
                        .adf(
                            serde_json::from_str(&current.body_raw)
                                .map_err(|_| BodyEditError("Confluence returned malformed ADF"))?,
                        )?
                        .map(|doc| serde_json::to_string(&doc))
                        .transpose()?,
                    "storage" => edit.storage(&current.body_raw)?,
                    _ => {
                        return Err(
                            BodyEditError("unsupported Confluence body representation").into()
                        )
                    }
                };
                if let Some(raw) = updated {
                    patch.body_raw_passthrough = Some(raw);
                    true
                } else {
                    false
                }
            }
        };
        if !body_changed && patch.title == current.summary.title && patch.version_message.is_none()
        {
            return Ok(current);
        }
        self.update_page(page_id, &patch).await
    }

    pub async fn update_page(&self, page_id: &str, patch: &PageUpdate) -> Result<PageDetail> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let next_version = patch.prev_version.checked_add(1).ok_or(BodyEditError(
            "prev_version exceeds the supported version range",
        ))?;
        let version_message = patch.version_message.clone().unwrap_or_default();

        match self.deployment() {
            DeploymentKind::Cloud => {
                let (representation, body_value) = match patch.body_raw_passthrough.as_deref() {
                    Some(raw) => {
                        let repr = patch
                            .body_representation
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| matches!(*s, "atlas_doc_format" | "storage"))
                            .unwrap_or("atlas_doc_format");
                        (repr, raw.to_string())
                    }
                    None => (
                        "atlas_doc_format",
                        serde_json::to_string(&markdown_to_adf(&patch.body_markdown))
                            .context("ADF 직렬화 실패")?,
                    ),
                };
                let payload = json!({
                    "id": page_id,
                    "status": "current",
                    "title": patch.title,
                    "body": {
                        "representation": representation,
                        "value": body_value,
                    },
                    "version": {
                        "number": next_version,
                        "message": version_message,
                    }
                });
                let url = format!("{conf_root}/api/v2/pages/{encoded_id}");
                let _: Value = self.put_json(&url, &payload).await?;
            }
            DeploymentKind::DataCenter => {
                let storage_html = match patch.body_raw_passthrough.as_deref() {
                    Some(raw) => raw.to_string(),
                    None => markdown_to_storage_xhtml(&patch.body_markdown),
                };
                let payload = json!({
                    "id": page_id,
                    "type": "page",
                    "title": patch.title,
                    "version": {
                        "number": next_version,
                        "message": version_message,
                    },
                    "body": {
                        "storage": {
                            "value": storage_html,
                            "representation": "storage",
                        }
                    }
                });
                let url = format!("{conf_root}/rest/api/content/{encoded_id}");
                let _: Value = self.put_json(&url, &payload).await?;
            }
        }

        self.get_page(page_id).await
    }

    pub async fn delete_page(&self, page_id: &str) -> Result<()> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let url = match self.deployment() {
            DeploymentKind::Cloud => format!("{conf_root}/api/v2/pages/{encoded_id}"),
            DeploymentKind::DataCenter => format!("{conf_root}/rest/api/content/{encoded_id}"),
        };
        self.delete_req(&url).await
    }

    pub async fn move_page(
        &self,
        page_id: &str,
        parent_id: &str,
        prev_version: Option<i32>,
    ) -> Result<PageSummary> {
        let (page_id, parent_id) = (page_id.trim(), parent_id.trim());
        if page_id.is_empty() || parent_id.is_empty() || page_id == parent_id {
            bail!("provide a page ID and a different parent page ID");
        }
        if prev_version.is_some_and(|version| version <= 0) {
            bail!("prev_version must be positive");
        }
        let conf_root = self.require_confluence_root()?;
        let page = urlencoding::encode(page_id);
        let parent = urlencoding::encode(parent_id);
        match self.deployment() {
            DeploymentKind::Cloud => {
                let current = self.get_page_revision(page_id).await?;
                if let Some(expected) = prev_version {
                    check_page_base_version(expected, current.version_number)?;
                }
                if current.parent_id.as_deref() == Some(parent_id) {
                    return Ok(current);
                }
                self.put_without_body(&format!(
                    "{conf_root}/rest/api/content/{page}/move/append/{parent}"
                ))
                .await?;
            }
            DeploymentKind::DataCenter => {
                let expected = prev_version.context(
                    "provide the current prev_version for a Confluence Data Center move",
                )?;
                let url = format!("{conf_root}/rest/api/content/{page}");
                let source: Value = self
                    .get_json(&format!("{url}?expand=version,space,ancestors"))
                    .await?;
                if source["type"] != "page" {
                    bail!("Confluence Data Center move requires a page as the source");
                }
                validate_source_version(&source)?;
                let current = parse_dc_page_summary(&source, &conf_root);
                check_page_base_version(expected, current.version_number)?;
                if current.parent_id.as_deref() == Some(parent_id) {
                    return Ok(current);
                }
                let target = self.get_page_revision(parent_id).await?;
                if current.space_key.is_empty() || target.space_key != current.space_key {
                    bail!(
                        "Confluence Data Center page moves require a parent in the current space"
                    );
                }
                let next = current
                    .version_number
                    .checked_add(1)
                    .context("page version exceeds supported range")?;
                self.put_json_empty(
                    &url,
                    &json!({
                        "id": page_id, "type": "page", "title": current.title,
                        "space": {"key": current.space_key},
                        "ancestors": [{"id": parent_id}],
                        "version": {"number": next}
                    }),
                )
                .await?;
            }
        }
        let after = self
            .get_page_revision(page_id)
            .await
            .context("Confluence accepted the page move. Reading it back failed")?;
        if after.parent_id.as_deref() != Some(parent_id) {
            bail!("Confluence accepted the page move. The read did not confirm parent {parent_id}");
        }
        Ok(after)
    }

    pub async fn add_page_labels(&self, page_id: &str, labels: &[String]) -> Result<Vec<String>> {
        let mut seen = HashSet::new();
        if labels.is_empty() {
            bail!("provide at least one label");
        }
        let names: Vec<_> = labels
            .iter()
            .map(|label| validate_page_label(label))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|name| seen.insert(name.to_ascii_lowercase()))
            .map(str::to_owned)
            .collect();
        let url = self.page_label_url(page_id)?;
        let body: Vec<_> = names
            .iter()
            .map(|name| json!({"prefix": "global", "name": name}))
            .collect();
        self.post_json_empty(&url, &body).await?;
        Ok(names)
    }

    pub async fn remove_page_label(&self, page_id: &str, label: &str) -> Result<()> {
        let label = urlencoding::encode(validate_page_label(label)?);
        let url = self.page_label_url(page_id)?;
        self.delete_req(&format!("{url}?name={label}")).await
    }

    fn page_label_url(&self, page_id: &str) -> Result<String> {
        if page_id.trim().is_empty() {
            bail!("page ID must not be empty");
        }
        let root = self.require_confluence_root()?;
        let page = urlencoding::encode(page_id.trim());
        Ok(format!("{root}/rest/api/content/{page}/label"))
    }

    pub async fn list_page_comments(&self, page_id: &str) -> Result<Vec<ConfluenceComment>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!(
                    "{conf_root}/api/v2/pages/{encoded_id}/footer-comments?body-format=atlas_doc_format"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|c| parse_confluence_comment(c, page_id))
                    .collect())
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/{encoded_id}/child/comment?expand=body.storage,version"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|c| parse_confluence_comment(c, page_id))
                    .collect())
            }
        }
    }

    pub async fn add_page_comment(
        &self,
        page_id: &str,
        body_markdown: &str,
    ) -> Result<ConfluenceComment> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());

        let resp: Value = match self.deployment() {
            DeploymentKind::Cloud => {
                let adf_str = serde_json::to_string(&markdown_to_adf(body_markdown))
                    .context("ADF 직렬화 실패")?;
                let url = format!("{conf_root}/api/v2/pages/{encoded_id}/footer-comments");
                let payload = json!({
                    "body": {
                        "representation": "atlas_doc_format",
                        "value": adf_str,
                    }
                });
                self.post_json(&url, &payload).await?
            }
            DeploymentKind::DataCenter => {
                let url = format!("{conf_root}/rest/api/content");
                let payload = json!({
                    "type": "comment",
                    "container": {
                        "id": page_id,
                        "type": "page",
                    },
                    "body": {
                        "storage": {
                            "value": markdown_to_storage_xhtml(body_markdown),
                            "representation": "storage",
                        }
                    }
                });
                self.post_json(&url, &payload).await?
            }
        };

        Ok(parse_confluence_comment(&resp, page_id))
    }

    pub async fn search_confluence_cql(
        &self,
        cql: &str,
        limit: usize,
    ) -> Result<Vec<PageSummary>> {
        let conf_root = self.require_confluence_root()?;
        let lim = limit.clamp(1, 100);
        let encoded_cql = urlencoding::encode(cql.trim());

        match self.deployment() {
            DeploymentKind::Cloud => {
                let url = format!("{conf_root}/rest/api/search?cql={encoded_cql}&limit={lim}");
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);

                let mut out = Vec::new();
                for item in arr {
                    let content = item.get("content").unwrap_or(item);
                    let kind = content
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("page");
                    if !matches!(kind, "page" | "blogpost" | "") {
                        continue;
                    }
                    let mut summary = parse_dc_page_summary(content, &conf_root);
                    if summary.title.is_empty() {
                        summary.title = first_nonempty_str(item, &["title"]);
                    }
                    if summary.web_url.is_empty() {
                        let raw_url = first_nonempty_str(item, &["url"]);
                        summary.web_url = join_url(&conf_root, &raw_url);
                    }
                    if summary.space_key.is_empty() {
                        summary.space_key = item
                            .get("resultGlobalContainer")
                            .and_then(|c| c.get("displayUrl"))
                            .and_then(|v| v.as_str())
                            .and_then(extract_space_key_from_webui)
                            .or_else(|| extract_space_key_from_webui(&summary.web_url))
                            .unwrap_or_default();
                    }
                    if summary.updated_at.is_empty() {
                        summary.updated_at = first_nonempty_str(item, &["lastModified"]);
                    }
                    if !summary.id.is_empty() || !summary.title.is_empty() {
                        out.push(summary);
                    }
                }
                Ok(out)
            }
            DeploymentKind::DataCenter => {
                let url = format!(
                    "{conf_root}/rest/api/content/search?cql={encoded_cql}&limit={lim}&expand=space,version"
                );
                let val: Value = self.get_json(&url).await?;
                let arr = val
                    .get("results")
                    .and_then(|v| v.as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                Ok(arr
                    .iter()
                    .map(|p| parse_dc_page_summary(p, &conf_root))
                    .collect())
            }
        }
    }

    async fn resolve_cloud_space_id(&self, space_id_or_key: &str) -> Result<(String, Option<String>)> {
        let trimmed = space_id_or_key.trim();
        if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Ok((trimmed.to_string(), None));
        }
        if let Ok(cache) = self.space_id_cache.read() {
            if let Some((id, key)) = cache.get(&trimmed.to_ascii_lowercase()) {
                return Ok((id.clone(), Some(key.clone())));
            }
        }
        let spaces = self.list_spaces(250).await?;
        let found = spaces
            .iter()
            .find(|s| s.key.eq_ignore_ascii_case(trimmed) || s.id == trimmed)
            .ok_or_else(|| anyhow!("Confluence 스페이스 '{}'을(를) 찾을 수 없습니다.", trimmed))?;
        Ok((found.id.clone(), Some(found.key.clone())))
    }
}

fn parse_space_summary(val: &Value, conf_root: &str) -> SpaceSummary {
    let key = first_nonempty_str(val, &["key"]);
    let id = {
        let raw = json_id_to_string(val.get("id"));
        if raw.is_empty() {
            key.clone()
        } else {
            raw
        }
    };
    let homepage_id = val
        .get("homepageId")
        .or_else(|| val.get("homepage").and_then(|h| h.get("id")))
        .map(|v| json_id_to_string(Some(v)))
        .filter(|s| !s.is_empty());

    let description = val
        .get("description")
        .and_then(|d| {
            d.get("plain")
                .and_then(|p| p.get("value"))
                .and_then(|v| v.as_str())
                .or_else(|| d.as_str())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);

    SpaceSummary {
        id,
        key,
        name: first_nonempty_str(val, &["name"]),
        space_type: first_nonempty_str(val, &["type"]).to_ascii_lowercase(),
        homepage_id,
        description,
        web_url,
        pinned: false,
    }
}

fn validate_source_version(page: &Value) -> Result<()> {
    page.pointer("/version/number")
        .and_then(Value::as_i64)
        .and_then(|version| i32::try_from(version).ok())
        .filter(|&version| version > 0)
        .ok_or_else(|| BodyEditError("Confluence did not return a valid current version").into())
        .map(|_| ())
}

fn check_page_base_version(expected: i32, actual: i32) -> Result<()> {
    if expected != actual {
        return Err(HttpStatusError {
            status: reqwest::StatusCode::CONFLICT,
            message: format!(
                "Confluence version conflict: expected {expected}, current version is {actual}"
            ),
            body: String::new(),
        }
        .into());
    }
    Ok(())
}

fn validate_page_label(label: &str) -> Result<&str> {
    let label = label.trim();
    if label.is_empty() || label.chars().any(char::is_whitespace) {
        bail!("label names must be nonempty and contain no whitespace");
    }
    Ok(label)
}

fn parse_cloud_page_summary(val: &Value, conf_root: &str) -> PageSummary {
    let id = json_id_to_string(val.get("id"));
    let space_id = json_id_to_string(val.get("spaceId"));
    let parent_id = val
        .get("parentId")
        .and_then(|v| {
            if v.is_null() {
                None
            } else {
                let s = json_id_to_string(Some(v));
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            }
        });
    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);
    let space_key = first_nonempty_str(val, &["spaceKey"])
        .if_empty(|| extract_space_key_from_webui(webui).unwrap_or_default());

    let version = val.get("version").unwrap_or(&Value::Null);
    let version_number = version.get("number").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
    let author_name = version
        .get("authorId")
        .or_else(|| val.get("authorId"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let updated_at = first_nonempty_str(version, &["createdAt"])
        .if_empty(|| first_nonempty_str(val, &["createdAt"]));
    let position = val.get("position").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

    PageSummary {
        id,
        space_id,
        space_key,
        parent_id,
        title: first_nonempty_str(val, &["title"]),
        status: first_nonempty_str(val, &["status"]).if_empty(|| "current".into()),
        version_number,
        author_name,
        updated_at,
        position,
        has_children: false,
        web_url,
    }
}

fn parse_dc_page_summary(val: &Value, conf_root: &str) -> PageSummary {
    let id = json_id_to_string(val.get("id"));
    let space = val.get("space").unwrap_or(&Value::Null);
    let space_id = json_id_to_string(space.get("id"));
    let space_key = first_nonempty_str(space, &["key"]);
    let webui = val
        .get("_links")
        .and_then(|l| l.get("webui"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let web_url = join_url(conf_root, webui);
    let space_key = if space_key.is_empty() {
        extract_space_key_from_webui(webui).unwrap_or_default()
    } else {
        space_key
    };

    let parent_id = val
        .get("ancestors")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .map(|a| json_id_to_string(a.get("id")))
        .filter(|s| !s.is_empty());

    let version = val.get("version").unwrap_or(&Value::Null);
    let version_number = version.get("number").and_then(|v| v.as_i64()).unwrap_or(1) as i32;
    let author_name = version
        .get("by")
        .and_then(|b| b.get("displayName"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let updated_at = first_nonempty_str(version, &["when"]);
    let position = val
        .get("extensions")
        .and_then(|e| e.get("position"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;

    PageSummary {
        id,
        space_id,
        space_key,
        parent_id,
        title: first_nonempty_str(val, &["title"]),
        status: first_nonempty_str(val, &["status"]).if_empty(|| "current".into()),
        version_number,
        author_name,
        updated_at,
        position,
        has_children: false,
        web_url,
    }
}

fn parse_confluence_comment(val: &Value, page_id: &str) -> ConfluenceComment {
    let id = json_id_to_string(val.get("id"));
    let version = val.get("version").unwrap_or(&Value::Null);
    let author_name = version
        .get("by")
        .and_then(|b| b.get("displayName"))
        .and_then(|v| v.as_str())
        .or_else(|| version.get("authorId").and_then(|v| v.as_str()))
        .or_else(|| val.get("authorId").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let created_at = first_nonempty_str(version, &["createdAt", "when"])
        .if_empty(|| first_nonempty_str(val, &["createdAt"]));

    let body = val.get("body").unwrap_or(&Value::Null);
    let adf_raw = body
        .get("atlas_doc_format")
        .and_then(|a| a.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let storage_raw = body
        .get("storage")
        .and_then(|s| s.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let (body_raw, blocks) = if !adf_raw.trim().is_empty() {
        (adf_raw.to_string(), parse_adf_str(adf_raw))
    } else if !storage_raw.trim().is_empty() {
        (storage_raw.to_string(), parse_storage_xhtml(storage_raw))
    } else {
        (String::new(), Vec::new())
    };
    let body_markdown = blocks_to_markdown(&blocks);

    ConfluenceComment {
        id,
        page_id: page_id.to_string(),
        author_name,
        body_raw,
        body_markdown,
        blocks,
        created_at,
    }
}

fn extract_linked_issue_keys(blocks: &[crate::models::DocBlock], raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for xref in extract_cross_refs(blocks, raw) {
        if let CrossRefTarget::JiraIssue { key, .. } = xref {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
    }
    keys
}

fn extract_space_key_from_webui(webui: &str) -> Option<String> {
    Regex::new(r"/spaces/([^/?#]+)")
        .ok()
        .and_then(|re| re.captures(webui))
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

trait StrFallback {
    fn if_empty<F: FnOnce() -> String>(self, f: F) -> String;
}

impl StrFallback for String {
    fn if_empty<F: FnOnce() -> String>(self, f: F) -> String {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredCredentials};
    use axum::{
        extract::Query,
        http::StatusCode as AxumStatus,
        routing::{get, post},
        Json, Router,
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn make_client(site_url: &str, conf_url: &str, deployment: DeploymentKind) -> AtlassianClient {
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: site_url.to_string(),
                token: "tok".into(),
                deployment,
            },
        );
        AtlassianClient::with_base_urls(site_url, conf_url, deployment, auth).unwrap()
    }

    #[tokio::test]
    async fn test_confluence_cloud_v2_spaces_pages_crud_comments_and_cql() {
        let updated_version: Arc<Mutex<Option<i64>>> = Arc::new(Mutex::new(None));
        let uv = updated_version.clone();

        let app = Router::new()
            .route(
                "/wiki/api/v2/spaces",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "999",
                            "key": "ENG",
                            "name": "Engineering",
                            "type": "global",
                            "homepageId": "100",
                            "_links": { "webui": "/spaces/ENG/overview" }
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/spaces/999/pages",
                get(|| async {
                    Json(json!({
                        "results": [
                            {
                                "id": "100",
                                "spaceId": "999",
                                "parentId": null,
                                "title": "Root Home",
                                "status": "current",
                                "version": { "number": 1, "createdAt": "2026-11-01T00:00:00Z" },
                                "_links": { "webui": "/spaces/ENG/pages/100/Root+Home" }
                            },
                            {
                                "id": "101",
                                "spaceId": "999",
                                "parentId": "100",
                                "title": "Child Filtered Out",
                                "status": "current",
                                "version": { "number": 1 }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/100/children",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "101",
                            "spaceId": "999",
                            "parentId": "100",
                            "title": "Child Page",
                            "status": "current",
                            "version": { "number": 2 }
                        }]
                    }))
                }),
            )
            .route(
                "/wiki/api/v2/pages",
                post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["spaceId"], "999");
                    assert_eq!(body["body"]["representation"], "atlas_doc_format");
                    Json(json!({ "id": "200" }))
                }),
            )
            .route(
                "/wiki/api/v2/pages/200",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let fmt = q.get("body-format").map(String::as_str).unwrap_or("");
                    if fmt == "atlas_doc_format" {
                        Json(json!({
                            "id": "200",
                            "spaceId": "999",
                            "title": "New Spec",
                            "status": "current",
                            "version": { "number": 3, "createdAt": "2026-11-15T12:00:00Z" },
                            "_links": { "webui": "/spaces/ENG/pages/200/New+Spec" },
                            "body": {
                                "atlas_doc_format": {
                                    "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}"
                                }
                            }
                        }))
                    } else {
                        Json(json!({ "id": "200", "body": { "storage": { "value": "<p>fallback</p>" } } }))
                    }
                })
                .put(move |Json(body): Json<Value>| {
                    let uv = uv.clone();
                    async move {
                        *uv.lock().unwrap() = body["version"]["number"].as_i64();
                        Json(json!({ "id": "200" }))
                    }
                })
                .delete(|| async { AxumStatus::NO_CONTENT }),
            )
            .route(
                "/wiki/api/v2/pages/200/labels",
                get(|| async { Json(json!({ "results": [{ "name": "architecture" }] })) }),
            )
            .route(
                "/wiki/api/v2/pages/200/attachments",
                get(|| async { Json(json!({ "results": [] })) }),
            )
            .route(
                "/wiki/api/v2/pages/200/footer-comments",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "cm-1",
                            "version": { "authorId": "acc-1", "createdAt": "2026-11-15T12:01:00Z" },
                            "body": {
                                "atlas_doc_format": { "value": "{\"type\":\"doc\",\"version\":1,\"content\":[]}" }
                            }
                        }]
                    }))
                })
                .post(|Json(body): Json<Value>| async move {
                    assert_eq!(body["body"]["representation"], "atlas_doc_format");
                    Json(json!({
                        "id": "cm-2",
                        "version": { "authorId": "acc-1", "createdAt": "2026-11-15T12:02:00Z" },
                        "body": body["body"]
                    }))
                }),
            )
            .route(
                "/wiki/rest/api/search",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    assert!(q.get("cql").unwrap().contains("Spec"));
                    Json(json!({
                        "results": [{
                            "content": {
                                "id": "200",
                                "type": "page",
                                "title": "New Spec",
                                "space": { "key": "ENG" },
                                "version": { "number": 3 },
                                "_links": { "webui": "/spaces/ENG/pages/200/New+Spec" }
                            }
                        }]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, "", DeploymentKind::Cloud);

        let spaces = client.list_spaces(25).await.unwrap();
        assert_eq!(spaces[0].id, "999");
        assert_eq!(spaces[0].key, "ENG");

        let roots = client.list_space_root_pages("ENG", 25).await.unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, "100");
        assert_eq!(roots[0].space_key, "ENG");

        let children = client.list_child_pages("100").await.unwrap();
        assert_eq!(children[0].id, "101");

        let created = client
            .create_page(&OutgoingPage {
                space_id_or_key: "ENG".into(),
                parent_id: Some("100".into()),
                title: "New Spec".into(),
                body_markdown: "# Hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(created.summary.id, "200");
        assert_eq!(created.labels, vec!["architecture"]);
        assert_eq!(created.comments.len(), 1);

        let _updated = client
            .update_page(
                "200",
                &PageUpdate {
                    title: "Updated Spec".into(),
                    body_markdown: "Updated body".into(),
                    prev_version: 3,
                    version_message: Some("bump".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(*updated_version.lock().unwrap(), Some(4));

        let comment = client.add_page_comment("200", "LGTM").await.unwrap();
        assert_eq!(comment.id, "cm-2");

        let hits = client
            .search_confluence_cql("title ~ \"Spec\"", 10)
            .await
            .unwrap();
        assert_eq!(hits[0].id, "200");
        assert_eq!(hits[0].space_key, "ENG");

        client.delete_page("200").await.unwrap();
    }

    #[tokio::test]
    async fn test_confluence_datacenter_v1_fallback() {
        let app = Router::new()
            .route(
                "/rest/api/space",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "key": "DC",
                            "name": "DC Space",
                            "type": "global",
                            "_links": { "webui": "/display/DC" }
                        }]
                    }))
                }),
            )
            .route(
                "/rest/api/space/DC/content/page",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "501",
                            "title": "DC Home",
                            "space": { "key": "DC" },
                            "version": { "number": 2, "when": "2026-11-10T00:00:00Z" },
                            "_links": { "webui": "/display/DC/DC+Home" }
                        }]
                    }))
                }),
            )
            .route(
                "/rest/api/content/501",
                get(|| async {
                    Json(json!({
                        "id": "501",
                        "title": "DC Home",
                        "space": { "key": "DC" },
                        "version": { "number": 2, "by": { "displayName": "Admin" }, "when": "2026-11-10T00:00:00Z" },
                        "ancestors": [{ "id": "500", "title": "Top" }],
                        "metadata": { "labels": { "results": [{ "name": "ops" }] } },
                        "children": { "attachment": { "results": [] } },
                        "body": { "storage": { "value": "<p>DC body</p>" } }
                    }))
                })
                .put(|Json(body): Json<Value>| async move {
                    assert_eq!(body["version"]["number"], 3);
                    assert_eq!(body["body"]["storage"]["representation"], "storage");
                    Json(json!({ "id": "501" }))
                }),
            )
            .route(
                "/rest/api/content/501/child/comment",
                get(|| async { Json(json!({ "results": [] })) }),
            )
            .route(
                "/rest/api/content/search",
                get(|| async {
                    Json(json!({
                        "results": [{
                            "id": "501",
                            "title": "DC Home",
                            "space": { "key": "DC" },
                            "version": { "number": 2 }
                        }]
                    }))
                }),
            );

        let base_url = spawn_router(app).await;
        let client = make_client(&base_url, &base_url, DeploymentKind::DataCenter);

        let spaces = client.list_spaces(10).await.unwrap();
        assert_eq!(spaces[0].key, "DC");

        let roots = client.list_space_root_pages("DC", 10).await.unwrap();
        assert_eq!(roots[0].id, "501");

        let page = client.get_page("501").await.unwrap();
        assert_eq!(page.body_representation, "storage");
        assert_eq!(page.ancestors[0].id, "500");
        assert_eq!(page.labels, vec!["ops"]);

        let updated = client
            .update_page(
                "501",
                &PageUpdate {
                    title: "DC Home v3".into(),
                    body_markdown: "Updated".into(),
                    prev_version: 2,
                    version_message: None,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.summary.id, "501");

        let hits = client.search_confluence_cql("text ~ \"DC\"", 10).await.unwrap();
        assert_eq!(hits[0].id, "501");
    }
}
