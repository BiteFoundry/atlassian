//! CLI and stdio MCP workflows against documented Cloud and Data Center routes.
//!
//! Modularized by domain while preserving the single `workflow_atlassian_e2e` integration test binary:
//! - `harness`: Upstream mock Cloud/DC fixture, subprocess CLI/MCP runner, and body helpers.
//! - `workflows`: Jira/Confluence write, dry-run, section-edit, and read-only MCP workflows.
//! - `regressions`: Tree preservation, auth/doctor, multi-site sync, wiki/ADF roundtrip regressions.
//! - `cli_contracts`: Compact/full JSON, assignee resolution, structured errors, and pagination contracts.

mod cli_contracts;
mod harness;
mod regressions;
mod workflows;
