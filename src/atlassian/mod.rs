//! Unified Atlassian REST client (`Jira Cloud v3 / DC v2`, `Jira Agile v1`, `Confluence Cloud v2 / DC v1`, and binary media).

pub mod agile;
pub(crate) mod body;
pub mod client;
pub mod collect;
pub mod confluence;
pub mod jira;
pub mod media;

pub use client::{AtlassianClient, SiteClientPool};
