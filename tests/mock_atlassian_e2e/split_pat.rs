use std::sync::{Arc, RwLock};

use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::json;

use atlassian_cli::{
    atlassian::AtlassianClient,
    auth::{AuthManager, StoredCredentials},
    config::{AppConfig, DeploymentKind},
    storage::Storage,
};

use crate::server::{check_bearer_auth, ensure_isolated_env, spawn_server};

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
                    return *resp;
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
                return *resp;
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
    AppConfig {
        site_url: format!("{base_url}/jira"),
        confluence_url: format!("{base_url}/wiki"),
        deployment: DeploymentKind::DataCenter,
        default_site: Some("corp".into()),
        sites: vec![atlassian_cli::config::SiteProfile {
            id: "corp".into(),
            name: "Corp DC".into(),
            site_url: format!("{base_url}/jira"),
            confluence_url: format!("{base_url}/wiki"),
            deployment: DeploymentKind::DataCenter,
            account_id: "corp".into(),
            confluence_account_id: confluence_account.map(str::to_string),
            ..Default::default()
        }],
        ..AppConfig::default()
    }
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
    storage
        .upsert_spaces_for_site("corp", std::slice::from_ref(&cached))
        .unwrap();
    let _ = sync::bootstrap_workspace_for_site("corp", &client, &storage).await;
    let after = storage.list_spaces_for_site("corp").unwrap();
    assert_eq!(
        after.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(),
        vec!["DOCS"],
        "the spaces cache must not be pruned or overwritten"
    );
    assert_eq!(after[0].name, "Docs");
}
