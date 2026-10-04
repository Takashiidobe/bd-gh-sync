use std::collections::BTreeSet;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub fn verify(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(sig) = header
        .and_then(|h| h.strip_prefix("sha256="))
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes keys of any length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Ping,
    Projects,
    Sync { repo: String, issues: BTreeSet<u64> },
    Reconcile { repo: String },
    Ignored(String),
}

pub fn parse(event: &str, payload: &Value) -> Event {
    if event == "ping" {
        return Event::Ping;
    }
    if event == "projects_v2_item" {
        return Event::Projects;
    }
    let Some(repo) = payload["repository"]["full_name"].as_str() else {
        return Event::Ignored("no repository in payload".into());
    };
    if event == "pull_request" {
        return Event::Reconcile {
            repo: repo.to_string(),
        };
    }
    let keys: &[&str] = match event {
        "issue_comment" if !payload["issue"]["pull_request"].is_null() => {
            return Event::Ignored("comment on a pull request".into());
        }
        "issues" | "issue_comment" => &["issue"],
        "sub_issues" => &["parent_issue", "sub_issue"],
        "issue_dependencies" => &["blocked_issue", "blocking_issue"],
        "issue_relates_to" => &["issue", "related_issue"],
        other => return Event::Ignored(format!("{other} events are not synced")),
    };
    let own = format!("/repos/{repo}").to_lowercase();
    let issues: BTreeSet<u64> = keys
        .iter()
        .map(|key| &payload[*key])
        .filter(|issue| {
            issue["repository_url"]
                .as_str()
                .is_none_or(|url| url.to_lowercase().ends_with(&own))
        })
        .filter_map(|issue| issue["number"].as_u64())
        .collect();
    if issues.is_empty() {
        return Event::Ignored("no issues from this repository".into());
    }
    Event::Sync {
        repo: repo.to_string(),
        issues,
    }
}
