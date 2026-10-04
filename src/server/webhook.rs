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
    Sync { repo: String, issues: BTreeSet<u64> },
    Ignored(String),
}

pub fn parse(event: &str, payload: &Value) -> Event {
    if event == "ping" {
        return Event::Ping;
    }
    let Some(repo) = payload["repository"]["full_name"].as_str() else {
        return Event::Ignored("no repository in payload".into());
    };
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn accepts_a_valid_signature() {
        let header = sign(b"secret", b"{}");
        assert!(verify(b"secret", b"{}", Some(&header)));
    }

    #[test]
    fn rejects_bad_signatures() {
        let header = sign(b"secret", b"{}");
        assert!(!verify(b"other", b"{}", Some(&header)));
        assert!(!verify(b"secret", b"{ }", Some(&header)));
        assert!(!verify(b"secret", b"{}", None));
        assert!(!verify(b"secret", b"{}", Some("sha256=zz")));
        assert!(!verify(
            b"secret",
            b"{}",
            Some(header.trim_start_matches("sha256="))
        ));
    }

    fn repo() -> Value {
        json!({"full_name": "acme/widgets"})
    }

    fn issue(n: u64, repo: &str) -> Value {
        json!({"number": n, "repository_url": format!("https://api.github.com/repos/{repo}")})
    }

    fn synced(issues: &[u64]) -> Event {
        Event::Sync {
            repo: "acme/widgets".into(),
            issues: issues.iter().copied().collect(),
        }
    }

    #[test]
    fn issue_and_comment_events() {
        let payload = json!({"repository": repo(), "issue": issue(4, "acme/widgets")});
        assert_eq!(parse("issues", &payload), synced(&[4]));
        assert_eq!(parse("issue_comment", &payload), synced(&[4]));
    }

    #[test]
    fn pull_request_comments_are_ignored() {
        let mut pr = issue(9, "acme/widgets");
        pr["pull_request"] = json!({"url": "x"});
        let payload = json!({"repository": repo(), "issue": pr});
        assert!(matches!(
            parse("issue_comment", &payload),
            Event::Ignored(_)
        ));
    }

    #[test]
    fn relation_events_pull_both_ends() {
        let sub = json!({"repository": repo(), "parent_issue": issue(4, "acme/widgets"), "sub_issue": issue(5, "acme/widgets")});
        assert_eq!(parse("sub_issues", &sub), synced(&[4, 5]));
        let dep = json!({"repository": repo(), "blocked_issue": issue(5, "acme/widgets"), "blocking_issue": issue(6, "acme/widgets")});
        assert_eq!(parse("issue_dependencies", &dep), synced(&[5, 6]));
        let rel = json!({"repository": repo(), "issue": issue(5, "acme/widgets"), "related_issue": issue(7, "acme/widgets")});
        assert_eq!(parse("issue_relates_to", &rel), synced(&[5, 7]));
    }

    #[test]
    fn other_repositories_issues_are_dropped() {
        let dep = json!({"repository": repo(), "blocked_issue": issue(5, "acme/widgets"), "blocking_issue": issue(6, "acme/other")});
        assert_eq!(parse("issue_dependencies", &dep), synced(&[5]));
        let only_other = json!({"repository": repo(), "issue": issue(3, "acme/other")});
        assert!(matches!(parse("issues", &only_other), Event::Ignored(_)));
    }

    #[test]
    fn ping_and_unknown_events() {
        assert_eq!(parse("ping", &json!({})), Event::Ping);
        assert!(matches!(
            parse("push", &json!({"repository": repo()})),
            Event::Ignored(_)
        ));
        assert!(matches!(parse("issues", &json!({})), Event::Ignored(_)));
    }
}
