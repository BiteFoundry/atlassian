//! Full in-process mock Atlassian Cloud + Data Center E2E test suite.
//!
//! Modularized by domain while preserving the single `mock_atlassian_e2e` integration test binary:
//! - `server`: Shared in-process mock Jira/Confluence Cloud + DC HTTP router & fixtures.
//! - `core_e2e`: Core Cloud/DC/HTTP/MCP/Multi-site/TUI end-to-end flows.
//! - `regressions`: Cloud/DC bug regression & offline HTTP cache filter tests.
//! - `split_pat`: Data Center split-PAT & anonymous-200 auth regression tests.
//! - `paged_dc`: Data Center multi-page collection & target-site identity tests.

mod core_e2e;
mod paged_dc;
mod regressions;
mod server;
mod split_pat;
