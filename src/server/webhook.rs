use std::collections::BTreeSet;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use crate::sync::{Change, CommentAction, FieldChange, known_label};

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
    Changes { repo: String, changes: Vec<Change> },
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
    let number_in_repo = |issue: &Value| {
        issue["repository_url"]
            .as_str()
            .is_none_or(|url| url.to_lowercase().ends_with(&own))
            .then(|| issue["number"].as_u64())
            .flatten()
    };
    if let Some(change) = change_of(event, payload, &number_in_repo) {
        return Event::Changes {
            repo: repo.to_string(),
            changes: vec![change],
        };
    }
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

fn latest(a: &Value, b: &Value) -> String {
    let at = |issue: &Value| issue["updated_at"].as_str().unwrap_or_default().to_string();
    at(a).max(at(b))
}

fn change_of(
    event: &str,
    payload: &Value,
    number_in_repo: &dyn Fn(&Value) -> Option<u64>,
) -> Option<Change> {
    let action = payload["action"].as_str()?;
    let added = match action.rsplit_once('_').map(|(_, last)| last) {
        Some("added") => Some(true),
        Some("removed") => Some(false),
        _ => None,
    };
    match event {
        "issue_comment" => {
            let action = match action {
                "created" => CommentAction::Created,
                "edited" => CommentAction::Edited,
                "deleted" => CommentAction::Deleted,
                _ => return None,
            };
            payload["comment"]["id"].as_u64()?;
            Some(Change::Comment {
                issue: number_in_repo(&payload["issue"])?,
                action,
                comment: payload["comment"].clone(),
            })
        }
        "issues" => {
            if !matches!(
                action,
                "edited" | "labeled" | "unlabeled" | "assigned" | "unassigned" | "closed" | "reopened"
            ) || !payload["issue"]["pull_request"].is_null()
            {
                return None;
            }
            let label = payload["label"]["name"].as_str().map(str::to_string);
            if matches!(action, "labeled" | "unlabeled")
                && !label.as_deref().is_some_and(known_label)
            {
                return None;
            }
            number_in_repo(&payload["issue"])?;
            Some(Change::Field(FieldChange {
                action: action.to_string(),
                label,
                changed: payload["changes"]
                    .as_object()
                    .map(|c| c.keys().cloned().collect())
                    .unwrap_or_default(),
                sender: payload["sender"]["login"].as_str().map(str::to_string),
                issue: payload["issue"].clone(),
            }))
        }
        "sub_issues" => {
            let parent = number_in_repo(&payload["parent_issue"])?;
            let sub = number_in_repo(&payload["sub_issue"])?;
            Some(Change::Relation {
                edge: format!("{sub} parent-child {parent}"),
                added: added?,
                at: latest(&payload["parent_issue"], &payload["sub_issue"]),
            })
        }
        "issue_dependencies" => {
            let blocked = number_in_repo(&payload["blocked_issue"])?;
            let blocking = number_in_repo(&payload["blocking_issue"])?;
            Some(Change::Relation {
                edge: format!("{blocked} blocks {blocking}"),
                added: added?,
                at: latest(&payload["blocked_issue"], &payload["blocking_issue"]),
            })
        }
        _ => None,
    }
}
