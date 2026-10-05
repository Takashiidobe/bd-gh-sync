use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    bd::{Bd, Bead, DELETED_LABEL, MOVED_LABEL, Workdir},
    github::GitHub,
    ids::{self, IdConfig},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Issues(Vec<u64>),
    SinceLast,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Auto,
    Dolt,
    Jsonl,
}

#[derive(Debug, Clone)]
pub struct Options {
    pub repo: String,
    pub mode: Mode,
    pub publish: bool,
    pub transport: Transport,
    pub jsonl: PathBuf,
    pub adopt_bd_created: bool,
    pub commit_message: String,
}

pub const COMMENT_MARKER: &str = "<!-- bd-comment:";
pub const LINKED_PR_COMMENT: &str = "Linked pull request:";
const EDITED_COMMENT: &str = "Edited on GitHub:";
const DELETED_COMMENT: &str = "Deleted on GitHub:";
const CLOSE_COMMENT_WINDOW: Duration = Duration::from_secs(10);
const FRESH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn is_sync_note(text: &str) -> bool {
    [LINKED_PR_COMMENT, EDITED_COMMENT, DELETED_COMMENT]
        .iter()
        .any(|prefix| text.starts_with(prefix))
}
const ATTEMPTS: u32 = 5;

pub async fn run(wd: &Workdir, gh: &GitHub, opts: &Options) -> Result<()> {
    let bd = Bd::new(wd);
    let transport = resolve_transport(wd, &bd, opts.transport).await?;
    info!(
        "transport: {}",
        if transport == Transport::Dolt {
            "dolt"
        } else {
            "jsonl"
        }
    );
    let mut sync = Sync {
        wd,
        bd,
        gh,
        opts,
        transport,
        mode: opts.mode.clone(),
        next_since: humantime::format_rfc3339_seconds(SystemTime::now() - Duration::from_secs(120))
            .to_string(),
        state_file: wd
            .dir
            .join(opts.jsonl.parent().unwrap_or(Path::new("")))
            .join("github-sync.json"),
    };
    let started = Instant::now();
    let result = match transport {
        Transport::Dolt => sync.via_dolt().await,
        _ => sync.via_jsonl().await,
    };
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        "sync pass completed"
    );
    result
}

async fn resolve_transport(wd: &Workdir, bd: &Bd<'_>, wanted: Transport) -> Result<Transport> {
    if wanted != Transport::Auto {
        return Ok(wanted);
    }
    let remote = wd
        .output(
            "git",
            &["ls-remote", "--exit-code", "origin", "refs/dolt/data"],
        )
        .await?;
    if remote.success {
        return Ok(Transport::Dolt);
    }
    if bd.config_get("sync.remote").await?.is_some() {
        bail!(
            "beads here sync through a Dolt remote (sync.remote), but origin has no Dolt data yet.\n\
             Run 'bd dolt push' once from your clone, or set the transport to jsonl."
        );
    }
    Ok(Transport::Jsonl)
}

struct GithubLinks {
    relations: BTreeSet<String>,
    mentions: BTreeSet<String>,
    pulls: BTreeMap<String, (u64, String)>,
    duplicates: BTreeMap<u64, u64>,
}

struct Pulled {
    commented: Vec<u64>,
    closed: BTreeMap<u64, String>,
    closing: BTreeMap<u64, Closing>,
}

#[derive(Debug, PartialEq)]
pub struct Closing {
    at: String,
    by: Option<String>,
}

struct Sync<'a> {
    wd: &'a Workdir,
    bd: Bd<'a>,
    gh: &'a GitHub,
    opts: &'a Options,
    transport: Transport,
    mode: Mode,
    next_since: String,
    state_file: PathBuf,
}

#[derive(Debug, PartialEq)]
pub struct IssueInfo {
    pub number: u64,
    pub is_pr: bool,
    pub bd_created: bool,
    pub comments: u64,
    pub state_reason: Option<String>,
    pub closing: Option<Closing>,
}

impl IssueInfo {
    pub fn from_json(issue: &Value) -> Option<Self> {
        let labels: Vec<&str> = issue["labels"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l["name"].as_str().or_else(|| l.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let has = |prefix: &str| labels.iter().any(|l| l.starts_with(prefix));
        Some(Self {
            number: issue["number"].as_u64()?,
            is_pr: !issue["pull_request"].is_null(),
            bd_created: has("type::") && has("priority::"),
            comments: issue["comments"].as_u64().unwrap_or(0),
            state_reason: (issue["state"] == "closed").then(|| {
                issue["state_reason"]
                    .as_str()
                    .unwrap_or("completed")
                    .to_string()
            }),
            closing: (issue["state"] == "closed")
                .then(|| issue["closed_at"].as_str())
                .flatten()
                .map(|at| Closing {
                    at: at.to_string(),
                    by: issue["closed_by"]["login"].as_str().map(str::to_string),
                }),
        })
    }
}

fn is_here(issue: &Value, repo: &str, number: u64) -> bool {
    issue["number"].as_u64() == Some(number)
        && issue["repository_url"].as_str().is_none_or(|url| {
            url.to_lowercase()
                .ends_with(&format!("/repos/{}", repo.to_lowercase()))
        })
}

pub fn issue_number(external_ref: &str, repo: &str) -> Option<u64> {
    let r = external_ref.trim().to_lowercase();
    if let Some(n) = r.strip_prefix("gh-").or_else(|| r.strip_prefix("github:")) {
        return n.parse().ok();
    }
    let (_, path) = r.split_once("github.com/")?;
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [owner, name, "issues", n] if format!("{owner}/{name}") == repo.to_lowercase() => {
            n.parse().ok()
        }
        _ => None,
    }
}

pub fn linked(beads: &[Bead], repo: &str) -> BTreeMap<u64, String> {
    beads
        .iter()
        .filter_map(|b| {
            Some((
                issue_number(b.external_ref.as_deref()?, repo)?,
                b.id.clone(),
            ))
        })
        .collect()
}

pub fn state_reason_for(close_reason: &str) -> &'static str {
    let reason = close_reason.to_lowercase();
    if reason.contains("duplicate") {
        "duplicate"
    } else if ["won't", "wont", "not planned", "not_planned", "invalid"]
        .iter()
        .any(|s| reason.contains(s))
    {
        "not_planned"
    } else {
        "completed"
    }
}

fn close_reason_for(state_reason: &str) -> Option<&'static str> {
    match state_reason {
        "completed" => Some("Completed"),
        "not_planned" => Some("Not planned"),
        "duplicate" => Some("Duplicate"),
        _ => None,
    }
}

pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

fn close_comment(github: &[Value], closing: &Closing) -> Option<(u64, String)> {
    let closed_at = humantime::parse_rfc3339(&closing.at).ok()?;
    github
        .iter()
        .filter(|c| {
            !c["body"]
                .as_str()
                .unwrap_or_default()
                .contains(COMMENT_MARKER)
        })
        .filter(|c| {
            closing
                .by
                .as_deref()
                .is_none_or(|by| c["user"]["login"].as_str() == Some(by))
        })
        .rfind(|c| {
            c["created_at"]
                .as_str()
                .and_then(|t| humantime::parse_rfc3339(t).ok())
                .and_then(|t| closed_at.duration_since(t).ok())
                .is_some_and(|gap| gap <= CLOSE_COMMENT_WINDOW)
        })
        .map(|c| {
            (
                c["id"].as_u64().unwrap_or_default(),
                normalize(c["body"].as_str().unwrap_or_default()),
            )
        })
        .filter(|(id, text)| *id != 0 && !text.is_empty())
}

pub type Tracked = BTreeMap<u64, (String, String)>;

#[derive(Debug, Default, PartialEq)]
pub struct CommentPlan {
    pub add: Vec<(u64, String, String)>,
    pub edited: Vec<(u64, String, String)>,
    pub deleted: Vec<(u64, String)>,
    pub tracked: Tracked,
}

pub fn plan_comments(
    github: &[Value],
    bead: &[(String, String)],
    tracked: &Tracked,
) -> CommentPlan {
    let mut have: Vec<(String, String)> = bead
        .iter()
        .map(|(a, t)| (a.clone(), normalize(t)))
        .collect();
    let mut consume = |key: &(String, String)| match have.iter().position(|h| h == key) {
        Some(i) => {
            have.remove(i);
            true
        }
        None => false,
    };
    let remote: Vec<(u64, String, String)> = github
        .iter()
        .filter_map(|c| {
            let body = c["body"].as_str().unwrap_or_default();
            if body.contains(COMMENT_MARKER) {
                return None;
            }
            Some((
                c["id"].as_u64()?,
                c["user"]["login"].as_str().unwrap_or("ghost").to_string(),
                normalize(body),
            ))
        })
        .collect();
    let mut plan = CommentPlan::default();
    for (id, author, text) in &remote {
        if let Some((_, old)) = tracked.get(id) {
            if old == text {
                consume(&(author.clone(), text.clone()));
            } else {
                plan.edited.push((*id, author.clone(), text.clone()));
            }
            plan.tracked.insert(*id, (author.clone(), text.clone()));
        }
    }
    for (id, author, text) in &remote {
        if tracked.contains_key(id) {
            continue;
        }
        let key = (author.clone(), text.clone());
        if !consume(&key) {
            plan.add.push((*id, author.clone(), text.clone()));
        }
        plan.tracked.insert(*id, key);
    }
    plan.deleted = tracked
        .iter()
        .filter(|(id, _)| !remote.iter().any(|r| r.0 == **id))
        .map(|(id, (author, _))| (*id, author.clone()))
        .collect();
    plan
}

#[derive(Debug, PartialEq)]
pub struct RelationPlan {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub base: BTreeSet<String>,
}

pub fn plan_relations(
    remote: &BTreeSet<String>,
    base: &BTreeSet<String>,
    have: &BTreeSet<String>,
    linked: &BTreeMap<u64, String>,
) -> RelationPlan {
    let is_linked = |edge: &&String| {
        let parts: Vec<&str> = edge.split(' ').collect();
        parts.len() == 3
            && [parts[0], parts[2]]
                .iter()
                .all(|n| n.parse().is_ok_and(|n: u64| linked.contains_key(&n)))
    };
    let remote: BTreeSet<String> = remote.iter().filter(is_linked).cloned().collect();
    RelationPlan {
        add: remote
            .difference(base)
            .filter(|e| !have.contains(*e))
            .cloned()
            .collect(),
        remove: base
            .difference(&remote)
            .filter(|e| have.contains(*e))
            .cloned()
            .collect(),
        base: remote,
    }
}

const RELATIONS_QUERY: &str = r#"query($owner: String!, $repo: String!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    issues(first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        parent { number repository { nameWithOwner } }
        blockedBy(first: 100) { nodes { number repository { nameWithOwner } } }
        duplicateOf { number repository { nameWithOwner } }
        closedByPullRequestsReferences(first: 10, includeClosedPrs: false) {
          nodes { url author { login } }
        }
        timelineItems(first: 100, itemTypes: [CROSS_REFERENCED_EVENT]) {
          nodes {
            ... on CrossReferencedEvent {
              source { ... on Issue { number repository { nameWithOwner } } }
            }
          }
        }
      }
    }
  }
}"#;

impl Sync<'_> {
    fn repo(&self) -> &str {
        &self.opts.repo
    }

    async fn state_get(&self, key: &str) -> Result<Option<String>> {
        match self.transport {
            Transport::Dolt => self.bd.kv_get(&format!("bd-gh-sync.{key}")).await,
            _ => {
                let Ok(text) = std::fs::read_to_string(&self.state_file) else {
                    return Ok(None);
                };
                let state: Value = serde_json::from_str(&text)
                    .with_context(|| format!("parsing {}", self.state_file.display()))?;
                Ok(state[key]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string))
            }
        }
    }

    async fn state_set(&self, key: &str, value: &str) -> Result<()> {
        match self.transport {
            Transport::Dolt => self.bd.kv_set(&format!("bd-gh-sync.{key}"), value).await,
            _ => {
                let mut state: Value = std::fs::read_to_string(&self.state_file)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_else(|| json!({}));
                state[key] = Value::String(value.to_string());
                if let Some(dir) = self.state_file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(
                    &self.state_file,
                    format!("{}\n", serde_json::to_string_pretty(&state)?),
                )?;
                Ok(())
            }
        }
    }

    async fn describe_issues(
        &self,
        since: Option<&str>,
        linked: &BTreeMap<u64, String>,
    ) -> Result<(Vec<IssueInfo>, Vec<u64>)> {
        let repo = self.repo();
        let mut missing = Vec::new();
        let issues = match (&self.mode, since) {
            (Mode::Issues(numbers), _) => {
                let mut issues = Vec::new();
                for n in numbers {
                    match self
                        .gh
                        .send(Method::GET, &format!("repos/{repo}/issues/{n}"), None)
                        .await
                    {
                        Ok(resp) if resp.ok() && is_here(&resp.body, repo, *n) => {
                            issues.push(resp.body)
                        }
                        Ok(resp) if resp.ok() || matches!(resp.status, 404 | 410) => {
                            missing.push(*n)
                        }
                        Ok(resp) => warn!("#{n}: could not fetch (HTTP {}), skipping", resp.status),
                        Err(e) => warn!("#{n}: could not fetch, skipping: {e:#}"),
                    }
                }
                issues
            }
            (_, Some(since)) => {
                self.gh
                    .get_all(&format!(
                        "repos/{repo}/issues?state=all&per_page=100&since={since}"
                    ))
                    .await?
            }
            (_, None) => {
                let issues = self
                    .gh
                    .get_all(&format!("repos/{repo}/issues?state=all&per_page=100"))
                    .await?;
                let listed: BTreeSet<u64> =
                    issues.iter().filter_map(|i| i["number"].as_u64()).collect();
                missing.extend(linked.keys().filter(|n| !listed.contains(n)));
                issues
            }
        };
        Ok((
            issues.iter().filter_map(IssueInfo::from_json).collect(),
            missing,
        ))
    }

    async fn resolve_gone(&self, missing: &[u64], linked: &BTreeMap<u64, String>) -> Result<()> {
        if missing.is_empty() {
            return Ok(());
        }
        let beads = self.bd.export().await?;
        for n in missing {
            let Some(bead) = linked
                .get(n)
                .and_then(|id| beads.iter().find(|b| &b.id == id))
            else {
                continue;
            };
            if bead.detached() {
                continue;
            }
            let resp = match self
                .gh
                .send(
                    Method::GET,
                    &format!("repos/{}/issues/{n}", self.repo()),
                    None,
                )
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    warn!("#{n}: could not check whether it was deleted or moved: {e:#}");
                    continue;
                }
            };
            let id = &bead.id;
            if matches!(resp.status, 404 | 410) {
                let done = async {
                    self.bd.label_add(id, DELETED_LABEL).await?;
                    if !bead.is_closed() {
                        self.bd
                            .set_close_reason(id, "Issue deleted on GitHub")
                            .await?;
                    }
                    anyhow::Ok(())
                };
                match done.await {
                    Ok(()) => info!(
                        "#{n} was deleted on GitHub: closed {id} and marked it {DELETED_LABEL}"
                    ),
                    Err(e) => warn!("#{n}: could not mark {id} as deleted: {e:#}"),
                }
            } else if resp.ok() && !is_here(&resp.body, self.repo(), *n) {
                let Some(url) = resp.body["html_url"].as_str() else {
                    continue;
                };
                let done = async {
                    self.bd.set_external_ref(id, url).await?;
                    self.bd.label_add(id, MOVED_LABEL).await
                };
                match done.await {
                    Ok(()) => info!("#{n} was transferred to {url}: relinked {id}"),
                    Err(e) => warn!("#{n}: could not relink {id} to {url}: {e:#}"),
                }
            }
        }
        Ok(())
    }

    async fn pull_issues(&self, since: Option<&str>) -> Result<Pulled> {
        let linked = linked(&self.bd.export().await?, self.repo());
        let mut to_pull = Vec::new();
        let mut commented = Vec::new();
        let mut closed = BTreeMap::new();
        let mut closing = BTreeMap::new();
        let tracked = self.tracked_comments().await?;
        let (issues, missing) = self.describe_issues(since, &linked).await?;
        self.resolve_gone(&missing, &linked).await?;
        for issue in issues {
            if issue.is_pr {
                continue;
            }
            if !linked.contains_key(&issue.number)
                && issue.bd_created
                && !self.opts.adopt_bd_created
            {
                info!(
                    "#{} was created by bd but its bead is not published yet; skipping (it syncs once that clone pushes)",
                    issue.number
                );
                continue;
            }
            to_pull.push(issue.number);
            if let Some(reason) = issue.state_reason {
                closed.insert(issue.number, reason);
            }
            if let Some(info) = issue.closing {
                closing.insert(issue.number, info);
            }
            if issue.comments > 0 || tracked.contains_key(&issue.number) {
                commented.push(issue.number);
            }
        }
        if to_pull.is_empty() {
            info!("no issues to pull");
            return Ok(Pulled {
                commented,
                closed,
                closing,
            });
        }
        let list: Vec<String> = to_pull.iter().map(u64::to_string).collect();
        info!("pulling {} issue(s): {}", to_pull.len(), list.join(" "));
        let out = self.bd.github_pull(&to_pull).await?;
        for line in out
            .combined()
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with("Warning:"))
        {
            info!("{line}");
        }
        Ok(Pulled {
            commented,
            closed,
            closing,
        })
    }

    async fn import_close_reasons(
        &self,
        closed: &BTreeMap<u64, String>,
        from_comment: &BTreeSet<u64>,
    ) -> Result<()> {
        if closed.is_empty() {
            return Ok(());
        }
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        for (n, state_reason) in closed {
            if from_comment.contains(n) {
                continue;
            }
            let Some(label) = close_reason_for(state_reason) else {
                continue;
            };
            let Some(bead) = linked
                .get(n)
                .and_then(|id| beads.iter().find(|b| &b.id == id))
            else {
                continue;
            };
            let current = bead.close_reason.as_deref().unwrap_or_default();
            let superseded = bead.dependencies().iter().any(|d| d.kind == "supersedes");
            if !bead.is_closed()
                || state_reason_for(current) == state_reason
                || (superseded && state_reason == "duplicate")
            {
                continue;
            }
            match self.bd.set_close_reason(&bead.id, label).await {
                Ok(()) => info!("#{n}: close reason set to '{label}'"),
                Err(e) => warn!("#{n}: could not set the close reason: {e:#}"),
            }
        }
        Ok(())
    }

    async fn tracked_comments(&self) -> Result<BTreeMap<u64, Tracked>> {
        match self.state_get("comments").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded comments"),
            None => Ok(BTreeMap::new()),
        }
    }

    async fn import_comments(
        &self,
        numbers: &[u64],
        closing: &BTreeMap<u64, Closing>,
    ) -> Result<(bool, BTreeSet<u64>)> {
        let mut from_comment = BTreeSet::new();
        if numbers.is_empty() {
            return Ok((false, from_comment));
        }
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        let before = self.tracked_comments().await?;
        let mut all = before.clone();
        for n in numbers {
            let Some(id) = linked.get(n) else { continue };
            let Some(bead) = beads.iter().find(|b| &b.id == id) else {
                continue;
            };
            let github = match self
                .gh
                .get_all(&format!(
                    "repos/{}/issues/{n}/comments?per_page=100",
                    self.repo()
                ))
                .await
            {
                Ok(comments) => comments,
                Err(e) => {
                    warn!("#{n}: could not fetch comments, skipping: {e:#}");
                    continue;
                }
            };
            let have: Vec<(String, String)> = bead
                .comments()
                .iter()
                .map(|c| (c.author.clone().unwrap_or_default(), c.text.clone()))
                .collect();
            let previous = before.get(n).cloned().unwrap_or_default();
            let mut plan = plan_comments(&github, &have, &previous);
            let mut tracked = plan.tracked.clone();
            let revert = |cid: u64, tracked: &mut Tracked| match previous.get(&cid) {
                Some(entry) => tracked.insert(cid, entry.clone()),
                None => tracked.remove(&cid),
            };
            let closer = closing
                .get(n)
                .filter(|_| bead.is_closed())
                .and_then(|c| close_comment(&github, c));
            if let Some((cid, text)) = closer {
                plan.add.retain(|(added, ..)| *added != cid);
                from_comment.insert(*n);
                if bead.close_reason.as_deref().map(normalize).as_deref() != Some(text.as_str()) {
                    match self.bd.set_close_reason(id, &text).await {
                        Ok(()) => info!("#{n}: closing comment became the close reason"),
                        Err(e) => {
                            warn!("#{n}: could not set the close reason: {e:#}");
                            revert(cid, &mut tracked);
                        }
                    }
                }
            }
            for (cid, author, text) in &plan.add {
                match self.bd.comment_add(id, author, text).await {
                    Ok(()) => info!("#{n}: imported a comment by {author}"),
                    Err(e) => {
                        warn!("#{n}: could not import a comment by {author}: {e:#}");
                        revert(*cid, &mut tracked);
                    }
                }
            }
            for (cid, author, text) in &plan.edited {
                let note = format!("{EDITED_COMMENT} {text}");
                match self.bd.comment_add(id, author, &note).await {
                    Ok(()) => info!("#{n}: noted an edit by {author}"),
                    Err(e) => {
                        warn!("#{n}: could not note an edit by {author}: {e:#}");
                        revert(*cid, &mut tracked);
                    }
                }
            }
            for (cid, author) in &plan.deleted {
                let note = format!("{DELETED_COMMENT} a comment by {author}");
                match self.bd.comment_add(id, author, &note).await {
                    Ok(()) => info!("#{n}: noted a deleted comment by {author}"),
                    Err(e) => {
                        warn!("#{n}: could not note a deleted comment by {author}: {e:#}");
                        tracked.insert(*cid, previous[cid].clone());
                    }
                }
            }
            if tracked.is_empty() {
                all.remove(n);
            } else {
                all.insert(*n, tracked);
            }
        }
        if all == before {
            return Ok((false, from_comment));
        }
        self.state_set("comments", &serde_json::to_string(&all)?)
            .await?;
        Ok((true, from_comment))
    }

    async fn github_relations(&self) -> Result<GithubLinks> {
        let (owner, name) = self
            .repo()
            .split_once('/')
            .context("repository must be owner/name")?;
        let nodes = self
            .gh
            .graphql_nodes(
                RELATIONS_QUERY,
                json!({"owner": owner, "repo": name}),
                &["repository", "issues"],
            )
            .await?;
        let own = |r: &Value| {
            r["repository"]["nameWithOwner"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(self.repo()))
        };
        let mut edges = BTreeSet::new();
        let mut mentions = BTreeSet::new();
        let mut pulls = BTreeMap::new();
        let mut duplicates = BTreeMap::new();
        for node in &nodes {
            let Some(n) = node["number"].as_u64() else {
                continue;
            };
            if let (true, Some(m)) = (
                own(&node["duplicateOf"]),
                node["duplicateOf"]["number"].as_u64(),
            ) {
                duplicates.insert(n, m);
            }
            for pr in node["closedByPullRequestsReferences"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(url) = pr["url"].as_str() {
                    pulls.insert(
                        url.to_string(),
                        (
                            n,
                            pr["author"]["login"]
                                .as_str()
                                .unwrap_or("ghost")
                                .to_string(),
                        ),
                    );
                }
            }
            for event in node["timelineItems"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let source = &event["source"];
                if let (true, Some(m)) = (own(source), source["number"].as_u64())
                    && m != n
                {
                    mentions.insert(format!("{} related {}", n.min(m), n.max(m)));
                }
            }
            if let (true, Some(p)) = (own(&node["parent"]), node["parent"]["number"].as_u64()) {
                edges.insert(format!("{n} parent-child {p}"));
            }
            for blocker in node["blockedBy"]["nodes"].as_array().into_iter().flatten() {
                if let (true, Some(b)) = (own(blocker), blocker["number"].as_u64()) {
                    edges.insert(format!("{n} blocks {b}"));
                }
            }
        }
        Ok(GithubLinks {
            relations: edges,
            mentions,
            pulls,
            duplicates,
        })
    }

    async fn sync_relations(&self) -> Result<bool> {
        let GithubLinks {
            relations: remote,
            mentions,
            pulls,
            duplicates,
        } = match self.github_relations().await {
            Ok(found) => found,
            Err(e) => {
                warn!("could not read relations from GitHub; skipping them: {e:#}");
                return Ok(false);
            }
        };
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        let number_of: BTreeMap<&str, u64> =
            linked.iter().map(|(n, id)| (id.as_str(), *n)).collect();
        let have: BTreeSet<String> = beads
            .iter()
            .flat_map(Bead::relations)
            .filter_map(|d| {
                Some(format!(
                    "{} {} {}",
                    number_of.get(d.issue_id.as_str())?,
                    d.kind,
                    number_of.get(d.depends_on_id.as_str())?
                ))
            })
            .collect();
        let base: BTreeSet<String> = match self.state_get("relations").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded relations")?,
            None => BTreeSet::new(),
        };
        let plan = plan_relations(&remote, &base, &have, &linked);

        let ids = |edge: &str| -> (String, String, String) {
            let parts: Vec<&str> = edge.split(' ').collect();
            let id = |n: &str| linked[&n.parse::<u64>().unwrap()].clone();
            (id(parts[0]), parts[1].to_string(), id(parts[2]))
        };
        let mut next_base = plan.base.clone();
        for edge in &plan.add {
            let (from, kind, to) = ids(edge);
            let shown = edge_for_log(edge);
            match self.bd.dep_add(&from, &to, &kind).await {
                Ok(()) => info!("{shown}: added"),
                Err(e) => {
                    warn!("{shown}: could not add; will retry next run: {e:#}");
                    next_base.remove(edge);
                }
            }
        }
        for edge in &plan.remove {
            let (from, _, to) = ids(edge);
            match self.bd.dep_remove(&from, &to).await {
                Ok(()) => info!("{}: removed", edge_for_log(edge)),
                Err(e) => warn!("{}: could not remove: {e:#}", edge_for_log(edge)),
            }
        }
        let mut changed = false;
        if next_base != base {
            self.state_set("relations", &serde_json::to_string(&next_base)?)
                .await?;
            changed = true;
        }
        let duplicated = self.import_duplicates(&linked, &duplicates).await?;
        let mentioned = self.import_mentions(&linked, &mentions).await?;
        let pulled = self.import_pulls(&linked, &pulls).await?;
        Ok(changed || duplicated || mentioned || pulled)
    }

    async fn import_duplicates(
        &self,
        linked: &BTreeMap<u64, String>,
        duplicates: &BTreeMap<u64, u64>,
    ) -> Result<bool> {
        let beads = self.bd.export().await?;
        let mut changed = false;
        for (n, canonical) in duplicates {
            let (Some(id), Some(target)) = (linked.get(n), linked.get(canonical)) else {
                continue;
            };
            let superseded = beads.iter().any(|b| {
                &b.id == id
                    && b.dependencies()
                        .iter()
                        .any(|d| d.kind == "supersedes" && &d.depends_on_id == target)
            });
            if superseded {
                continue;
            }
            match self.bd.dep_add(id, target, "supersedes").await {
                Ok(()) => {
                    info!("#{n} supersedes link to #{canonical}: added (duplicate on GitHub)");
                    changed = true;
                }
                Err(e) => warn!("#{n}: could not link the duplicate of #{canonical}: {e:#}"),
            }
        }
        Ok(changed)
    }

    async fn import_pulls(
        &self,
        linked: &BTreeMap<u64, String>,
        pulls: &BTreeMap<String, (u64, String)>,
    ) -> Result<bool> {
        let seen: BTreeSet<String> = match self.state_get("pulls").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded pulls")?,
            None => BTreeSet::new(),
        };
        let next: BTreeSet<String> = pulls
            .iter()
            .filter(|(_, (n, _))| linked.contains_key(n))
            .map(|(url, _)| url.clone())
            .collect();
        let mut recorded = next.clone();
        let beads = self.bd.export().await?;
        for url in next.difference(&seen) {
            let (n, author) = &pulls[url];
            let id = &linked[n];
            let Some(bead) = beads.iter().find(|b| &b.id == id) else {
                continue;
            };
            if bead.is_closed() {
                continue;
            }
            if bead.status.as_str() == Some("open") {
                match self.bd.set_status(id, "in_progress").await {
                    Ok(()) => info!("#{n}: in progress, pull request {url} is open"),
                    Err(e) => {
                        warn!("#{n}: could not set the status; will retry next run: {e:#}");
                        recorded.remove(url);
                        continue;
                    }
                }
            }
            let text = format!("{LINKED_PR_COMMENT} {url}");
            if bead
                .comments()
                .iter()
                .any(|c| c.text.contains(url.as_str()))
            {
                continue;
            }
            if let Err(e) = self.bd.comment_add(id, author, &text).await {
                warn!("#{n}: could not record the pull request; will retry next run: {e:#}");
                recorded.remove(url);
            }
        }
        if recorded == seen {
            return Ok(false);
        }
        self.state_set("pulls", &serde_json::to_string(&recorded)?)
            .await?;
        Ok(true)
    }

    async fn import_mentions(
        &self,
        linked: &BTreeMap<u64, String>,
        mentions: &BTreeSet<String>,
    ) -> Result<bool> {
        let beads = self.bd.export().await?;
        let imported: BTreeSet<String> = match self.state_get("mentions").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded mentions")?,
            None => BTreeSet::new(),
        };
        let number_of: BTreeMap<&str, u64> =
            linked.iter().map(|(n, id)| (id.as_str(), *n)).collect();
        let have: BTreeSet<String> = beads
            .iter()
            .flat_map(Bead::dependencies)
            .filter_map(|d| {
                let (a, b) = (
                    *number_of.get(d.issue_id.as_str())?,
                    *number_of.get(d.depends_on_id.as_str())?,
                );
                Some(format!("{} related {}", a.min(b), a.max(b)))
            })
            .collect();
        let both_linked = |edge: &str| {
            let parts: Vec<&str> = edge.split(' ').collect();
            parts.len() == 3
                && [parts[0], parts[2]]
                    .iter()
                    .all(|n| n.parse().is_ok_and(|n: u64| linked.contains_key(&n)))
        };
        let next: BTreeSet<String> = mentions
            .iter()
            .filter(|e| both_linked(e))
            .cloned()
            .collect();
        let mut recorded = next.clone();
        for edge in next.difference(&imported).filter(|e| !have.contains(*e)) {
            let parts: Vec<&str> = edge.split(' ').collect();
            let id = |n: &str| linked[&n.parse::<u64>().unwrap()].clone();
            let shown = edge_for_log(edge);
            match self
                .bd
                .dep_add(&id(parts[0]), &id(parts[2]), "related")
                .await
            {
                Ok(()) => info!("{shown}: imported from a cross-reference"),
                Err(e) => {
                    warn!("{shown}: could not add; will retry next run: {e:#}");
                    recorded.remove(edge);
                }
            }
        }
        if recorded == imported {
            return Ok(false);
        }
        self.state_set("mentions", &serde_json::to_string(&recorded)?)
            .await?;
        Ok(true)
    }

    async fn id_config(&self) -> IdConfig {
        let mut config = IdConfig::default();
        let read = |key: &'static str| async move { self.bd.config_get(key).await.ok().flatten() };
        if let Some(n) = read("min_hash_length").await.and_then(|v| v.parse().ok()) {
            config.min_len = n;
        }
        if let Some(n) = read("max_hash_length").await.and_then(|v| v.parse().ok()) {
            config.max_len = n;
        }
        if let Some(p) = read("max_collision_prob")
            .await
            .and_then(|v| v.parse().ok())
        {
            config.max_collision = p;
        }
        config
    }

    async fn fresh_beads(&self) -> Result<BTreeMap<String, u64>> {
        match self.state_get("fresh").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded new beads"),
            None => Ok(BTreeMap::new()),
        }
    }

    async fn shorten_imports(&self) -> Result<()> {
        let beads = self.bd.export().await?;
        let mut taken: BTreeSet<String> = beads.iter().map(|b| b.id.clone()).collect();
        let mut config = None;
        let mut fresh = self.fresh_beads().await?;
        let mut renamed = false;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        for bead in &beads {
            let Some(prefix) = ids::imported_prefix(&bead.id) else {
                continue;
            };
            let Some(number) = bead
                .external_ref
                .as_deref()
                .and_then(|r| issue_number(r, self.repo()))
            else {
                continue;
            };
            let config = match config {
                Some(config) => config,
                None => *config.insert(self.id_config().await),
            };
            let len = ids::adaptive_len(taken.len() + 1, &config);
            let key = format!("{}#{number}", self.repo().to_lowercase());
            let id = ids::short_id(prefix, &key, len, &config, &taken);
            match self.bd.rename(&bead.id, &id).await {
                Ok(()) => {
                    info!("#{number}: renamed {} to {id}", bead.id);
                    taken.remove(&bead.id);
                    taken.insert(id.clone());
                    fresh.insert(id, now);
                    renamed = true;
                }
                Err(e) => warn!("#{number}: could not rename {}: {e:#}", bead.id),
            }
        }
        if renamed {
            self.state_set("fresh", &serde_json::to_string(&fresh)?)
                .await?;
        }
        Ok(())
    }

    async fn nest_fresh(&self) -> Result<()> {
        let mut fresh = self.fresh_beads().await?;
        if fresh.is_empty() {
            return Ok(());
        }
        let before = fresh.clone();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        fresh.retain(|_, at| now.saturating_sub(*at) < FRESH_TTL.as_secs());
        loop {
            let beads = self.bd.export().await?;
            let mut taken: BTreeSet<String> = beads.iter().map(|b| b.id.clone()).collect();
            fresh.retain(|id, _| taken.contains(id));
            let parent_of = |id: &str| -> Option<String> {
                beads
                    .iter()
                    .find(|b| b.id == id)?
                    .dependencies()
                    .iter()
                    .find(|d| d.kind == "parent-child" && d.issue_id == id)
                    .map(|d| d.depends_on_id.clone())
            };
            let nested = |id: &str, parent: &str| id.starts_with(&format!("{parent}."));
            let next = fresh.keys().find_map(|id| {
                let parent = parent_of(id)?;
                let waiting = fresh.contains_key(&parent)
                    && parent_of(&parent).is_some_and(|grand| !nested(&parent, &grand));
                (!nested(id, &parent) && !waiting).then(|| (id.clone(), parent))
            });
            let Some((id, parent)) = next else {
                break;
            };
            let new = ids::next_child(&parent, &taken);
            let mut moves = vec![(id.clone(), new.clone())];
            taken.insert(new.clone());
            for below in ids::descendants(&id, &taken) {
                let renamed = format!("{new}{}", &below[id.len()..]);
                moves.push((below, renamed));
            }
            let mut failed = false;
            for (from, to) in &moves {
                match self.bd.rename(from, to).await {
                    Ok(()) => info!("renamed {from} to {to} under {parent}"),
                    Err(e) => {
                        warn!("could not rename {from} to {to}: {e:#}");
                        failed = true;
                        break;
                    }
                }
            }
            fresh.remove(&id);
            if failed {
                break;
            }
            for (from, to) in moves.iter().skip(1) {
                if let Some(at) = fresh.remove(from) {
                    fresh.insert(to.clone(), at);
                }
            }
        }
        if fresh != before {
            self.state_set("fresh", &serde_json::to_string(&fresh)?)
                .await?;
        }
        Ok(())
    }

    async fn sync_github(&mut self) -> Result<bool> {
        let before = self.bd.export_raw().await?;
        let mut since = None;
        if self.mode == Mode::SinceLast {
            since = self.state_get("since").await?;
            match &since {
                Some(since) => info!("pulling issues updated since {since}"),
                None => {
                    info!("no previous sync recorded; reconciling every issue");
                    self.mode = Mode::All;
                }
            }
        }
        let pulled = self.pull_issues(since.as_deref()).await?;
        if let Err(e) = self.shorten_imports().await {
            warn!("could not shorten imported bead ids: {e:#}");
        }
        let (comments_changed, from_comment) = self
            .import_comments(&pulled.commented, &pulled.closing)
            .await?;
        self.import_close_reasons(&pulled.closed, &from_comment)
            .await?;
        let relations_changed = self.sync_relations().await?;
        if let Err(e) = self.nest_fresh().await {
            warn!("could not nest new beads under their parents: {e:#}");
        }
        let changed =
            relations_changed || comments_changed || self.bd.export_raw().await? != before;
        if changed && !matches!(self.mode, Mode::Issues(_)) {
            self.state_set("since", &self.next_since).await?;
        }
        Ok(changed)
    }

    async fn via_dolt(&mut self) -> Result<()> {
        self.bd.bootstrap().await?;
        for attempt in 1..=ATTEMPTS {
            self.bd.dolt_commit("bd-gh-sync: local state").await?;
            if let Err(e) = self.bd.dolt_pull().await {
                warn!("bd dolt pull failed; syncing on top of the local data: {e:#}");
            }
            let changed = self.sync_github().await?;
            if !self.opts.publish {
                return Ok(());
            }
            if !changed {
                info!("pull changed nothing; nothing to push");
                return Ok(());
            }
            self.bd.dolt_commit("bd-gh-sync: pull from GitHub").await?;
            match self.bd.dolt_push().await {
                Ok(()) => {
                    info!("pushed beads to the Dolt remote");
                    return Ok(());
                }
                Err(e) => {
                    warn!("Dolt push rejected (attempt {attempt}); pulling and retrying: {e:#}")
                }
            }
            tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
        }
        bail!("giving up after repeated push races")
    }

    async fn via_jsonl(&mut self) -> Result<()> {
        let wd = self.wd;
        let branch = wd
            .run("git", &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string();
        let jsonl = self.opts.jsonl.to_string_lossy().into_owned();
        let state_file = self.state_file.to_string_lossy().into_owned();
        self.bd.bootstrap().await?;
        for attempt in 1..=ATTEMPTS {
            self.sync_github().await?;
            if let Some(dir) = wd.dir.join(&self.opts.jsonl).parent() {
                std::fs::create_dir_all(dir)?;
            }
            self.bd.export_to(&self.opts.jsonl).await?;
            if !self.opts.publish {
                return Ok(());
            }
            wd.run("git", &["add", "--", &jsonl]).await?;
            if self.state_file.exists() {
                wd.run("git", &["add", "--", &state_file]).await?;
            }
            if wd
                .output("git", &["diff", "--cached", "--quiet"])
                .await?
                .success
            {
                info!("export unchanged; nothing to commit");
                return Ok(());
            }
            wd.run(
                "git",
                &[
                    "commit",
                    "-q",
                    "-m",
                    &self.opts.commit_message,
                    "-m",
                    "[skip ci]",
                ],
            )
            .await?;
            if wd
                .output("git", &["push", "-q", "origin", &format!("HEAD:{branch}")])
                .await?
                .success
            {
                let head = wd.run("git", &["rev-parse", "--short", "HEAD"]).await?;
                info!("pushed {} to {branch}", head.trim());
                return Ok(());
            }
            warn!("push rejected (attempt {attempt}); rebuilding on the latest {branch}");
            wd.run("git", &["fetch", "-q", "origin", &branch]).await?;
            wd.run(
                "git",
                &["reset", "-q", "--hard", &format!("origin/{branch}")],
            )
            .await?;
            if let Err(e) = self.bd.import(&self.opts.jsonl).await {
                warn!("could not import the newer export: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
        }
        bail!("giving up after repeated push races")
    }
}

fn edge_for_log(edge: &str) -> String {
    let parts: Vec<&str> = edge.split(' ').collect();
    format!("#{} {} #{}", parts[0], parts[1], parts[2])
}
