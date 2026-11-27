//! Human-friendly fuzzy user/assignee resolver for CLI, HTTP API, and MCP.
//!
//! Resolves Atlassian users by `accountId`/username, email, or display name when a server rejects
//! a raw assignee string, returning structured candidate errors on ambiguity.

use anyhow::{bail, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::{
    atlassian::{
        client::{CodedError, HttpStatusError},
        AtlassianClient,
    },
    models::UserSummary,
};

const MAX_CANDIDATE_LIST: usize = 8;

/// Resolves an Atlassian user query against an in-memory slice of `UserSummary` records.
///
/// Priority:
/// 1. Exact `id` match (Cloud `accountId` or Data Center `name`/`key`, exact then case-insensitive)
/// 2. Exact case-insensitive `email` match
/// 3. Unique case-insensitive exact `display_name` match
/// 4. Unique case-insensitive prefix match on `display_name` or `email`
/// 5. Unique case-insensitive substring match on `display_name`, `email`, or `id`
pub fn resolve_user_from_slice(users: &[UserSummary], query: &str) -> Result<UserSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("User accountId, email, or display name is empty.");
    }

    // Priority 1: Exact accountId / username
    if let Some(exact_id) = users.iter().find(|u| u.id == q) {
        return Ok(exact_id.clone());
    }
    if let Some(ci_id) = users.iter().find(|u| u.id.eq_ignore_ascii_case(q)) {
        return Ok(ci_id.clone());
    }

    let q_lower = q.to_lowercase();

    // Priority 2: Exact case-insensitive email
    let exact_email: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.email
                .as_deref()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(q))
        })
        .collect();
    if exact_email.len() == 1 {
        return Ok(exact_email[0].clone());
    } else if exact_email.len() > 1 {
        bail!(
            "Ambiguous user email '{q}' matches {} users. Specify an exact accountId:\n{}",
            exact_email.len(),
            format_user_candidates(&exact_email)
        );
    }

    // Priority 3: Unique case-insensitive exact display_name
    let exact_name: Vec<&UserSummary> = users
        .iter()
        .filter(|u| u.display_name.trim().to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        bail!(
            "Ambiguous user '{q}' matches {} users with the same display name. Specify an email or accountId:\n{}",
            exact_name.len(),
            format_user_candidates(&exact_name)
        );
    }

    // Priority 4: Unique case-insensitive prefix on display_name or email
    let prefix_matches: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.display_name.trim().to_lowercase().starts_with(&q_lower)
                || u.email
                    .as_deref()
                    .is_some_and(|e| e.trim().to_lowercase().starts_with(&q_lower))
        })
        .collect();
    if prefix_matches.len() == 1 {
        return Ok(prefix_matches[0].clone());
    } else if prefix_matches.len() > 1 {
        bail!(
            "Ambiguous user prefix '{q}' matches {} users. Specify an email or accountId:\n{}",
            prefix_matches.len(),
            format_user_candidates(&prefix_matches)
        );
    }

    // Priority 5: Unique case-insensitive substring on display_name, email, or id
    let sub_matches: Vec<&UserSummary> = users
        .iter()
        .filter(|u| {
            u.display_name.to_lowercase().contains(&q_lower)
                || u.email
                    .as_deref()
                    .is_some_and(|e| e.to_lowercase().contains(&q_lower))
                || u.id.to_lowercase().contains(&q_lower)
        })
        .collect();
    match sub_matches.len() {
        1 => Ok(sub_matches[0].clone()),
        n if n > 1 => bail!(
            "Ambiguous user '{q}' matches {n} users. Specify an email or accountId:\n{}",
            format_user_candidates(&sub_matches)
        ),
        _ => bail!(
            "User not found: '{q}'. Provide an exact accountId, email address, or full display name."
        ),
    }
}

/// After the server rejects an assignee value, try resolving once by name or email.
/// If `None`, the caller returns the original error as-is.
pub(crate) async fn resolve_rejected_assignee(
    client: &AtlassianClient,
    scope_key: &str,
    raw: &str,
    err: &anyhow::Error,
    require_assignee_field: bool,
) -> Option<Result<UserSummary>> {
    let http_err = err.downcast_ref::<HttpStatusError>()?;
    if !matches!(
        http_err.status,
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
    ) {
        return None;
    }
    if require_assignee_field {
        let has_assignee_error = serde_json::from_str::<Value>(&http_err.body)
            .ok()
            .and_then(|v| v.get("errors").and_then(|e| e.as_object()).cloned())
            .is_some_and(|obj| obj.contains_key("assignee"));
        if !has_assignee_error {
            return None;
        }
    }
    let q = raw.trim();
    if q.is_empty() {
        return None;
    }
    let users = client.search_assignable_users(scope_key, q).await.ok()?;
    match users.len() {
        0 => None,
        1 => Some(Ok(users.into_iter().next().unwrap())),
        _ => match resolve_user_from_slice(&users, q) {
            Ok(user) => Some(Ok(user)),
            Err(_) => {
                let refs: Vec<&UserSummary> = users.iter().collect();
                let candidates: Vec<Value> = users
                    .iter()
                    .map(|u| {
                        json!({
                            "id": u.id,
                            "display_name": u.display_name,
                            "email": u.email,
                        })
                    })
                    .collect();
                Some(Err(CodedError {
                    code: "ambiguous_user",
                    message: format!(
                        "Multiple users ({}) match '{q}'. Specify an accountId:\n{}",
                        users.len(),
                        format_user_candidates(&refs),
                    ),
                    details: Some(json!({ "candidates": candidates })),
                }
                .into()))
            }
        },
    }
}

fn format_user_candidates(candidates: &[&UserSummary]) -> String {
    let mut lines: Vec<String> = candidates
        .iter()
        .take(MAX_CANDIDATE_LIST)
        .map(|u| match u.email.as_deref() {
            Some(email) if !email.is_empty() => {
                format!("  - {} <{}> (id: {})", u.display_name, email, u.id)
            }
            _ => format!("  - {} (id: {})", u.display_name, u.id),
        })
        .collect();
    if candidates.len() > MAX_CANDIDATE_LIST {
        lines.push(format!(
            "  ... and {} more",
            candidates.len() - MAX_CANDIDATE_LIST
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_user(id: &str, display_name: &str, email: Option<&str>) -> UserSummary {
        UserSummary {
            id: id.into(),
            display_name: display_name.into(),
            email: email.map(str::to_string),
            avatar_url: None,
            active: true,
        }
    }

    #[test]
    fn test_resolve_user_by_account_id_email_and_partial_name() {
        let users = vec![
            sample_user("acc-100", "Alice Smith", Some("alice.smith@acme.com")),
            sample_user("acc-200", "Bob Jones", Some("bob.jones@acme.com")),
            sample_user("acc-300", "Alice Cooper", Some("alice.cooper@acme.com")),
        ];

        assert_eq!(
            resolve_user_from_slice(&users, "acc-200").unwrap().id,
            "acc-200"
        );
        assert_eq!(
            resolve_user_from_slice(&users, "ALICE.SMITH@ACME.COM")
                .unwrap()
                .id,
            "acc-100"
        );
        assert_eq!(
            resolve_user_from_slice(&users, "Bob Jones").unwrap().id,
            "acc-200"
        );
        assert_eq!(
            resolve_user_from_slice(&users, "Cooper").unwrap().id,
            "acc-300"
        );

        let amb = resolve_user_from_slice(&users, "Alice")
            .unwrap_err()
            .to_string();
        assert!(amb.contains("Alice Smith"));
        assert!(amb.contains("Alice Cooper"));
        assert!(amb.contains("acc-100"));
        assert!(amb.contains("acc-300"));

        assert!(resolve_user_from_slice(&users, "charlie").is_err());
    }
}
