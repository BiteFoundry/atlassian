//! Unified Atlassian REST client (`Jira Cloud v3 / DC v2`, `Jira Agile v1`, `Confluence Cloud v2 / DC v1`, and binary media).

pub mod agile;
pub(crate) mod body;
pub mod client;
pub mod collect;
pub mod confluence;
pub mod jira;
pub mod media;

pub use client::{AtlassianClient, SiteClientPool};

impl AtlassianClient {
    /// Asks Jira and Confluence separately who the credential belongs to. Jira is probed
    /// when a site URL is configured, Confluence when a Confluence root resolves. A failure
    /// on one product is recorded, never fatal by itself.
    pub async fn probe_identity(&self) -> client::IdentityProbe {
        let jira_fut = async {
            if crate::config::normalize_base_url(self.site_url()).is_empty() {
                None
            } else {
                Some(
                    self.get_myself()
                        .await
                        .map_err(|err| client::ProbeFailure::from_error(&err)),
                )
            }
        };
        let confluence_fut = async {
            if self.confluence_root_url().trim().is_empty() {
                None
            } else {
                Some(
                    self.get_confluence_current_user()
                        .await
                        .map_err(|err| client::ProbeFailure::from_error(&err)),
                )
            }
        };
        let (jira, confluence) = tokio::join!(jira_fut, confluence_fut);
        client::IdentityProbe { jira, confluence }
    }
}
