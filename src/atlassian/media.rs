//! Attachment upload (`X-Atlassian-Token: no-check`) and authenticated byte/file download for Jira & Confluence.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::{
    atlassian::{
        client::{join_url, parse_attachment_item},
        AtlassianClient,
    },
    config::DeploymentKind,
    models::{AttachmentItem, DownloadedFile},
};

impl AtlassianClient {
    pub async fn upload_issue_attachment(
        &self,
        issue_key: &str,
        file_path: &Path,
    ) -> Result<Vec<AttachmentItem>> {
        let base = self.require_site_url()?;
        let ver = match self.deployment() {
            DeploymentKind::Cloud => "3",
            DeploymentKind::DataCenter => "2",
        };
        let encoded_key = urlencoding::encode(issue_key.trim());
        let url = format!("{base}/rest/api/{ver}/issue/{encoded_key}/attachments");

        let (file_name, mime_type, bytes) = read_upload_file(file_path).await?;
        let val = self
            .post_multipart(&url, "file", &file_name, &mime_type, bytes, None)
            .await?;

        Ok(parse_attachment_list(&val, &base))
    }

    pub async fn upload_page_attachment(
        &self,
        page_id: &str,
        file_path: &Path,
    ) -> Result<Vec<AttachmentItem>> {
        let conf_root = self.require_confluence_root()?;
        let encoded_id = urlencoding::encode(page_id.trim());
        let url = format!("{conf_root}/rest/api/content/{encoded_id}/child/attachment");

        let (file_name, mime_type, bytes) = read_upload_file(file_path).await?;
        let minor_edit =
            (self.deployment() == DeploymentKind::Cloud).then_some(("minorEdit", "true"));
        let val = self
            .post_multipart(&url, "file", &file_name, &mime_type, bytes, minor_edit)
            .await?;

        Ok(parse_attachment_list(&val, &conf_root))
    }

    pub async fn fetch_attachment_bytes(&self, attachment: &AttachmentItem) -> Result<Vec<u8>> {
        let raw_url = attachment.content_url.trim();
        if raw_url.is_empty() {
            bail!(
                "Download URL for attachment '{}' is empty.",
                attachment.filename
            );
        }
        let resolved_url = if raw_url.starts_with("http://") || raw_url.starts_with("https://") {
            raw_url.to_string()
        } else {
            let base = self.require_site_url()?;
            join_url(&base, raw_url)
        };
        self.get_bytes(&resolved_url).await
    }

    pub async fn download_attachment(
        &self,
        attachment: &AttachmentItem,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        let bytes = self.fetch_attachment_bytes(attachment).await?;
        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let safe_name = sanitize_windows_filename(&attachment.filename);
        let target_path = deduplicate_path(dest_dir, &safe_name);
        let final_filename = target_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        write_bytes_atomic(&target_path, &bytes).await?;

        Ok(DownloadedFile {
            path: target_path,
            filename: final_filename,
            size: bytes.len() as u64,
            mime_type: attachment.mime_type.clone(),
        })
    }
}

async fn read_upload_file(file_path: &Path) -> Result<(String, String, Vec<u8>)> {
    let bytes = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("Failed to read attachment file: {}", file_path.display()))?;
    let file_name = file_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("attachment.bin")
        .to_string();
    let mime_type = guess_mime_type(file_path);
    Ok((file_name, mime_type.to_string(), bytes))
}

fn parse_attachment_list(val: &Value, base_url: &str) -> Vec<AttachmentItem> {
    if let Some(arr) = val.as_array() {
        return arr
            .iter()
            .map(|a| parse_attachment_item(a, base_url))
            .collect();
    }
    if let Some(arr) = val.get("results").and_then(|v| v.as_array()) {
        return arr
            .iter()
            .map(|a| parse_attachment_item(a, base_url))
            .collect();
    }
    if val.is_object() && (val.get("id").is_some() || val.get("filename").is_some()) {
        return vec![parse_attachment_item(val, base_url)];
    }
    Vec::new()
}

fn guess_mime_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "txt" | "log" | "csv" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

pub(crate) fn sanitize_windows_filename(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim();

    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                || (c as u32) < 0x20
            {
                '_'
            } else {
                c
            }
        })
        .collect();

    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    let mut name = if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    };

    // Windows ignores trailing spaces in the stem, so `CON .txt` still names the device.
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([' ', '.']);
    if is_windows_reserved_name(stem) {
        name = format!("_{name}");
    }
    cap_filename_bytes(name)
}

/// Longest file name, in bytes, that the common file systems accept.
const MAX_FILENAME_BYTES: usize = 255;

/// Shortens the stem on a character boundary so that the name fits and keeps its extension.
fn cap_filename_bytes(name: String) -> String {
    if name.len() <= MAX_FILENAME_BYTES {
        return name;
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot < MAX_FILENAME_BYTES => name.split_at(dot),
        _ => (name.as_str(), ""),
    };
    let mut end = (MAX_FILENAME_BYTES - extension.len()).min(stem.len());
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    let mut stem = &stem[..end];
    if extension.is_empty() {
        // The cut must not leave the trailing dot or space that Windows strips.
        stem = stem.trim_end_matches(['.', ' ']);
        if stem.is_empty() {
            return "attachment".to_string();
        }
    }
    format!("{stem}{extension}")
}

fn is_windows_reserved_name(stem: &str) -> bool {
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn deduplicate_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
    let initial = dest_dir.join(safe_name);
    if !initial.exists() {
        return initial;
    }

    let path_obj = Path::new(safe_name);
    let stem = path_obj
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(safe_name);
    let ext = path_obj.extension().and_then(|s| s.to_str());

    let mut idx = 1usize;
    loop {
        let candidate_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({idx}).{e}"),
            _ => format!("{stem} ({idx})"),
        };
        let candidate = dest_dir.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
        idx += 1;
    }
}

async fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("Failed to write temporary attachment file: {}", tmp.display()))?;
    if let Err(err) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err).with_context(|| format!("Failed to save attachment file: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredCredentials};
    use axum::{
        body::Bytes,
        extract::DefaultBodyLimit,
        http::{header, HeaderMap, StatusCode as AxumStatus},
        routing::{get, post},
        Json, Router,
    };
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    async fn spawn_router(app: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[test]
    fn test_windows_filename_sanitization() {
        assert_eq!(sanitize_windows_filename("a<b>c:d/e\\f|g?h*i.png"), "f_g_h_i.png");
        assert_eq!(sanitize_windows_filename("a<b>c:d|e?f*g.png"), "a_b_c_d_e_f_g.png");
        assert_eq!(sanitize_windows_filename("../../secret?.txt"), "secret_.txt");
        assert_eq!(sanitize_windows_filename("report:final?.pdf"), "report_final_.pdf");
        assert_eq!(sanitize_windows_filename("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_windows_filename("   ...  "), "attachment");
    }

    #[test]
    fn sanitize_filename_handles_reserved_stem_with_trailing_space() {
        // Windows ignores trailing spaces in the stem, so `CON .txt` still names the console device.
        assert_eq!(sanitize_windows_filename("CON .txt"), "_CON .txt");
        assert_eq!(sanitize_windows_filename("nul  .tar.gz"), "_nul  .tar.gz");
        assert_eq!(sanitize_windows_filename("CONSOLE .txt"), "CONSOLE .txt");
    }

    #[test]
    fn sanitize_filename_caps_length_at_255_bytes() {
        let ascii = sanitize_windows_filename(&format!("{}.png", "a".repeat(300)));
        assert_eq!(ascii.len(), 255);
        assert_eq!(ascii, format!("{}.png", "a".repeat(251)));

        // A multi-byte stem is cut on a character boundary and keeps the extension.
        let korean = sanitize_windows_filename(&format!("{}.pdf", "가".repeat(100)));
        assert!(korean.len() <= 255);
        assert_eq!(korean, format!("{}.pdf", "가".repeat(83)));

        assert_eq!(sanitize_windows_filename(&"b".repeat(400)), "b".repeat(255));
        assert_eq!(sanitize_windows_filename("report.final.pdf"), "report.final.pdf");
    }

    #[tokio::test]
    async fn test_upload_issue_and_page_attachments_with_no_check_header() {
        let app = Router::new()
            .route(
                "/rest/api/3/issue/ENG-1/attachments",
                post(|headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(
                        headers.get("x-atlassian-token").unwrap().to_str().unwrap(),
                        "no-check"
                    );
                    let content_type = headers
                        .get("content-type")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                    assert!(content_type.starts_with("multipart/form-data"));
                    let body_str = String::from_utf8_lossy(&body);
                    assert!(body_str.contains("name=\"file\""));
                    assert!(body_str.contains("filename=\"note.txt\""));
                    assert!(body_str.contains("hello world"));
                    Json(json!([{
                        "id": "att-1",
                        "filename": "note.txt",
                        "mimeType": "text/plain",
                        "size": 11,
                        "created": "2026-11-15T00:00:00Z",
                        "content": "/rest/api/3/attachment/content/att-1"
                    }]))
                }),
            )
            .route(
                "/wiki/rest/api/content/900/child/attachment",
                post(|headers: HeaderMap, body: Bytes| async move {
                    assert_eq!(
                        headers.get("x-atlassian-token").unwrap().to_str().unwrap(),
                        "no-check"
                    );
                    let body_str = String::from_utf8_lossy(&body);
                    assert!(body_str.contains("name=\"file\""));
                    assert!(body_str.contains("filename=\"note.txt\""));
                    assert!(body_str.contains("hello world"));
                    Json(json!({
                        "results": [{
                            "id": "att-2",
                            "title": "note.txt",
                            "extensions": { "fileSize": 11 },
                            "_links": { "download": "/download/attachments/900/note.txt" }
                        }]
                    }))
                }),
            )
            .layer(DefaultBodyLimit::disable());

        let base_url = spawn_router(app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "secret-tok".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let dir = tempdir().unwrap();
        let file_path = dir.path().join("note.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        let issue_atts = client
            .upload_issue_attachment("ENG-1", &file_path)
            .await
            .unwrap();
        assert_eq!(issue_atts.len(), 1);
        assert_eq!(issue_atts[0].filename, "note.txt");
        assert_eq!(issue_atts[0].size, 11);
        assert_eq!(
            issue_atts[0].content_url,
            format!("{base_url}/rest/api/3/attachment/content/att-1")
        );

        let page_atts = client
            .upload_page_attachment("900", &file_path)
            .await
            .unwrap();
        assert_eq!(page_atts.len(), 1);
        assert_eq!(page_atts[0].filename, "note.txt");
        assert_eq!(
            page_atts[0].content_url,
            format!("{base_url}/wiki/download/attachments/900/note.txt")
        );
    }

    #[tokio::test]
    async fn test_download_attachment_redirect_auth_and_collision_dedup() {
        // CDN server expects NO Authorization header (cross-origin signed redirect).
        let cdn_app = Router::new().route(
            "/signed-blob",
            get(|headers: HeaderMap| async move {
                assert!(
                    headers.get("authorization").is_none(),
                    "cross-origin CDN redirect must not receive Authorization header"
                );
                b"PNGDATA".to_vec()
            }),
        );
        let cdn_url = spawn_router(cdn_app).await;
        let cdn_target = format!("{cdn_url}/signed-blob");

        let api_app = Router::new()
            .route(
                "/rest/api/3/attachment/content/10",
                get(move |headers: HeaderMap| {
                    let cdn_target = cdn_target.clone();
                    async move {
                        assert_eq!(
                            headers.get("authorization").unwrap().to_str().unwrap(),
                            "Bearer my-pat"
                        );
                        (
                            AxumStatus::FOUND,
                            [(header::LOCATION, cdn_target)],
                        )
                    }
                }),
            )
            .route(
                "/rest/api/3/attachment/content/11",
                get(|headers: HeaderMap| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer my-pat"
                    );
                    (
                        AxumStatus::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "/rest/api/3/attachment/final/11")],
                    )
                }),
            )
            .route(
                "/rest/api/3/attachment/final/11",
                get(|headers: HeaderMap| async move {
                    assert_eq!(
                        headers.get("authorization").unwrap().to_str().unwrap(),
                        "Bearer my-pat"
                    );
                    b"SAMEORIGIN".to_vec()
                }),
            );

        let base_url = spawn_router(api_app).await;
        let auth = AuthManager::with_credentials(
            "test",
            StoredCredentials::BearerPat {
                site_url: base_url.clone(),
                token: "my-pat".into(),
                deployment: DeploymentKind::Cloud,
            },
        );
        let client =
            AtlassianClient::with_base_urls(&base_url, "", DeploymentKind::Cloud, auth).unwrap();

        let dest = tempdir().unwrap();

        let att_cross = AttachmentItem {
            id: "10".into(),
            filename: "bad:name?.png".into(),
            mime_type: "image/png".into(),
            size: 7,
            author_name: None,
            created_at: String::new(),
            content_url: "/rest/api/3/attachment/content/10".into(),
            thumbnail_url: None,
        };

        let d1 = client.download_attachment(&att_cross, dest.path()).await.unwrap();
        assert_eq!(d1.filename, "bad_name_.png");
        assert_eq!(std::fs::read(&d1.path).unwrap(), b"PNGDATA");

        // Second download of same filename deduplicates to `bad_name_ (1).png`
        let att_same = AttachmentItem {
            content_url: format!("{base_url}/rest/api/3/attachment/content/11"),
            ..att_cross
        };
        let d2 = client.download_attachment(&att_same, dest.path()).await.unwrap();
        assert_eq!(d2.filename, "bad_name_ (1).png");
        assert_eq!(std::fs::read(&d2.path).unwrap(), b"SAMEORIGIN");
    }
}
