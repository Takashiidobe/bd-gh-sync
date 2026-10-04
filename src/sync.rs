use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::{
    bd::{Bd, Bead, Workdir},
    github::GitHub,
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
    match transport {
        Transport::Dolt => sync.via_dolt().await,
        _ => sync.via_jsonl().await,
    }
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
        })
    }
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

pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

pub fn missing_comments(github: &[Value], bead: &[(String, String)]) -> Vec<(String, String)> {
    let mut have: Vec<(String, String)> = bead
        .iter()
        .map(|(a, t)| (a.clone(), normalize(t)))
        .collect();
    let mut missing = Vec::new();
    for c in github {
        let body = c["body"].as_str().unwrap_or_default();
        if body.contains(COMMENT_MARKER) {
            continue;
        }
        let key = (
            c["user"]["login"].as_str().unwrap_or("ghost").to_string(),
            normalize(body),
        );
        match have.iter().position(|h| *h == key) {
            Some(i) => {
                have.remove(i);
            }
            None => missing.push(key),
        }
    }
    missing
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

    async fn describe_issues(&self, since: Option<&str>) -> Result<Vec<IssueInfo>> {
        let repo = self.repo();
        let issues = match (&self.mode, since) {
            (Mode::Issues(numbers), _) => {
                let mut issues = Vec::new();
                for n in numbers {
                    match self.gh.get(&format!("repos/{repo}/issues/{n}")).await {
                        Ok(issue) => issues.push(issue),
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
                self.gh
                    .get_all(&format!("repos/{repo}/issues?state=all&per_page=100"))
                    .await?
            }
        };
        Ok(issues.iter().filter_map(IssueInfo::from_json).collect())
    }

    async fn pull_issues(&self, since: Option<&str>) -> Result<Vec<u64>> {
        let linked = linked(&self.bd.export().await?, self.repo());
        let mut to_pull = Vec::new();
        let mut commented = Vec::new();
        for issue in self.describe_issues(since).await? {
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
            if issue.comments > 0 {
                commented.push(issue.number);
            }
        }
        if to_pull.is_empty() {
            info!("no issues to pull");
            return Ok(commented);
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
        Ok(commented)
    }

    async fn import_comments(&self, numbers: &[u64]) -> Result<()> {
        if numbers.is_empty() {
            return Ok(());
        }
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
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
            for (author, text) in missing_comments(&github, &have) {
                match self.bd.comment_add(id, &author, &text).await {
                    Ok(()) => info!("#{n}: imported a comment by {author}"),
                    Err(e) => warn!("#{n}: could not import a comment by {author}: {e:#}"),
                }
            }
        }
        Ok(())
    }

    async fn github_relations(&self) -> Result<BTreeSet<String>> {
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
        for node in &nodes {
            let Some(n) = node["number"].as_u64() else {
                continue;
            };
            if let (true, Some(p)) = (own(&node["parent"]), node["parent"]["number"].as_u64()) {
                edges.insert(format!("{n} parent-child {p}"));
            }
            for blocker in node["blockedBy"]["nodes"].as_array().into_iter().flatten() {
                if let (true, Some(b)) = (own(blocker), blocker["number"].as_u64()) {
                    edges.insert(format!("{n} blocks {b}"));
                }
            }
        }
        Ok(edges)
    }

    async fn sync_relations(&self) -> Result<bool> {
        let remote = match self.github_relations().await {
            Ok(remote) => remote,
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
        if next_base == base {
            return Ok(false);
        }
        self.state_set("relations", &serde_json::to_string(&next_base)?)
            .await?;
        Ok(true)
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
        let commented = self.pull_issues(since.as_deref()).await?;
        self.import_comments(&commented).await?;
        let relations_changed = self.sync_relations().await?;
        let changed = relations_changed || self.bd.export_raw().await? != before;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_numbers_from_refs() {
        let repo = "Acme/Widgets";
        assert_eq!(
            issue_number("https://github.com/acme/widgets/issues/12", repo),
            Some(12)
        );
        assert_eq!(issue_number("gh-7", repo), Some(7));
        assert_eq!(issue_number("github:8", repo), Some(8));
        assert_eq!(
            issue_number("https://github.com/acme/other/issues/12", repo),
            None
        );
        assert_eq!(
            issue_number("https://github.com/acme/widgets/pull/12", repo),
            None
        );
        assert_eq!(issue_number("jira-1", repo), None);
    }

    #[test]
    fn issue_info() {
        let issue = json!({"number": 3, "comments": 2, "labels": [{"name": "type::bug"}, "priority::high"]});
        assert_eq!(
            IssueInfo::from_json(&issue),
            Some(IssueInfo {
                number: 3,
                is_pr: false,
                bd_created: true,
                comments: 2
            })
        );
        let pr = json!({"number": 4, "pull_request": {}, "labels": []});
        assert!(IssueInfo::from_json(&pr).unwrap().is_pr);
    }

    #[test]
    fn comments_match_by_author_and_text() {
        let github = vec![
            json!({"user": {"login": "octo"}, "body": "looks good\r\n"}),
            json!({"user": {"login": "octo"}, "body": "+1"}),
            json!({"user": {"login": "octo"}, "body": "+1"}),
            json!({"user": {"login": "me"}, "body": "from beads\n\n<!-- bd-comment:abc -->"}),
        ];
        let bead = vec![
            ("octo".to_string(), "looks good".to_string()),
            ("octo".to_string(), "+1".to_string()),
        ];
        assert_eq!(
            missing_comments(&github, &bead),
            vec![("octo".to_string(), "+1".to_string())]
        );
    }

    fn set(edges: &[&str]) -> BTreeSet<String> {
        edges.iter().map(|e| e.to_string()).collect()
    }

    #[test]
    fn relations_merge_three_ways() {
        let linked: BTreeMap<u64, String> = (1..=5).map(|n| (n, format!("b-{n}"))).collect();
        let remote = set(&["2 parent-child 1", "3 blocks 1", "4 blocks 9"]);
        let base = set(&["3 blocks 1", "5 blocks 1"]);
        let have = set(&["3 blocks 1", "5 blocks 1", "4 blocks 2"]);
        let plan = plan_relations(&remote, &base, &have, &linked);
        assert_eq!(plan.add, ["2 parent-child 1"]);
        assert_eq!(plan.remove, ["5 blocks 1"]);
        assert_eq!(plan.base, set(&["2 parent-child 1", "3 blocks 1"]));
    }
}
