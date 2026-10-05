use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    bd::{Bd, Bead, Workdir},
    github::{DEFAULT_API_URL, GitHub, Response},
    sync::{COMMENT_MARKER, is_sync_note, normalize, state_reason_for},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    Native,
    Poll,
}

pub struct Options {
    pub once: bool,
    pub initial_push: bool,
    pub poll: Duration,
    pub debounce: Duration,
    pub backend: Backend,
    pub dolt_sync: Duration,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pushed,
    Nothing,
    Failed,
}

impl Outcome {
    fn merge(self, other: Outcome) -> Outcome {
        match (self, other) {
            (Outcome::Failed, _) | (_, Outcome::Failed) => Outcome::Failed,
            (Outcome::Pushed, _) | (_, Outcome::Pushed) => Outcome::Pushed,
            _ => Outcome::Nothing,
        }
    }

    fn pushed(&mut self) {
        if *self == Outcome::Nothing {
            *self = Outcome::Pushed;
        }
    }
}

const IGNORED_SUFFIXES: &[&str] = &[".jsonl", ".log", ".lock", ".pid", ".port", ".activity"];
const DRAIN_CAP: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_millis(500);

pub fn issue_path(external_ref: &str) -> Option<String> {
    let rest = external_ref
        .strip_prefix("https://")
        .or_else(|| external_ref.strip_prefix("http://"))?;
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [host, owner, name, "issues", n]
            if ![host, owner, name].iter().any(|s| s.is_empty())
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some(format!("repos/{owner}/{name}/issues/{n}"))
        }
        _ => None,
    }
}

fn links(beads: &[Bead]) -> BTreeMap<String, String> {
    beads
        .iter()
        .filter(|b| !b.detached())
        .filter_map(|b| Some((b.id.clone(), issue_path(b.external_ref.as_deref()?)?)))
        .collect()
}

pub fn fingerprint(bead: &Bead) -> String {
    let mut labels = bead.labels.clone().unwrap_or_default();
    labels.sort();
    json!({
        "title": bead.title,
        "description": bead.description.as_deref().unwrap_or(""),
        "status": bead.status,
        "priority": bead.priority,
        "issue_type": bead.issue_type,
        "labels": labels,
    })
    .to_string()
}

fn ignored(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
        || name == "last-touched"
        || path
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with("bd.sock"))
}

fn is_write(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        _ => false,
    }
}

struct Watcher {
    wd: Workdir,
    gh: GitHub,
    state_dir: PathBuf,
    opts: Options,
}

pub async fn run(opts: Options) -> Result<()> {
    let start = std::env::current_dir()?;
    let beads_dir = start
        .ancestors()
        .map(|d| d.join(".beads"))
        .find(|d| d.is_dir())
        .context("no .beads directory found (run bd init first)")?;
    let root = beads_dir
        .parent()
        .expect(".beads has a parent")
        .to_path_buf();

    let mut wd = Workdir::new(&root).env("BD_NO_DEP_TYPE_WARNING", "1");
    let token = match std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        Ok(token) => token,
        Err(_) => {
            let token = gh_auth_token(&wd).await.unwrap_or_default();
            if token.is_empty() {
                warn!(
                    "GITHUB_TOKEN is not set and `gh auth token` gave none; GitHub calls will fail"
                );
            }
            wd = wd.env("GITHUB_TOKEN", &token);
            token
        }
    };
    let api = std::env::var("GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into());

    let state_dir = match wd.output("git", &["rev-parse", "--absolute-git-dir"]).await {
        Ok(out) if out.success => PathBuf::from(out.stdout.trim()).join("bd-gh-sync"),
        _ => beads_dir.join("bd-gh-sync.local"),
    };
    std::fs::create_dir_all(&state_dir)?;
    let lock = File::create(state_dir.join("watch.lock"))?;
    if lock.try_lock().is_err() {
        bail!(
            "another bd-gh-sync watch is already running for {}",
            root.display()
        );
    }

    let watcher = Watcher {
        wd,
        gh: GitHub::new(&api, &token),
        state_dir,
        opts,
    };
    if watcher.opts.once {
        if watcher.sync_pass().await == Outcome::Failed {
            bail!("some changes could not be pushed; see above");
        }
        return Ok(());
    }
    tokio::select! {
        result = watcher.watch(&beads_dir) => result,
        _ = crate::server::shutdown() => Ok(()),
    }
}

async fn gh_auth_token(wd: &Workdir) -> Option<String> {
    let out = wd.output("gh", &["auth", "token"]).await.ok()?;
    out.success.then(|| out.stdout.trim().to_string())
}

impl Watcher {
    fn bd(&self) -> Bd<'_> {
        Bd::new(&self.wd)
    }

    fn load<T: DeserializeOwned>(&self, name: &str) -> Option<T> {
        let text = std::fs::read_to_string(self.state_dir.join(name)).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        serde_json::from_str(&text).ok()
    }

    fn save<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        if self.opts.dry_run {
            return Ok(());
        }
        let path = self.state_dir.join(name);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string(value)? + "\n")?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    async fn call(&self, method: Method, path: &str, body: Option<&Value>) -> Response {
        match self.gh.send(method, path, body).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("{e:#}");
                Response {
                    status: 0,
                    body: Value::Null,
                }
            }
        }
    }

    async fn export(&self) -> Option<Vec<Bead>> {
        match self.bd().export().await {
            Ok(beads) => Some(beads),
            Err(e) => {
                warn!("bd export failed; will retry: {e:#}");
                None
            }
        }
    }

    async fn sync_pass(&self) -> Outcome {
        let Some(mut beads) = self.export().await else {
            return Outcome::Failed;
        };
        let fields = self.step("fields", self.push_fields(&beads).await);
        if fields == Outcome::Pushed && !self.opts.dry_run {
            match self.export().await {
                Some(fresh) => beads = fresh,
                None => return Outcome::Failed,
            }
        }
        let assignees = self.step("assignees", self.push_assignees(&beads).await);
        let deleted = self.step("deleted beads", self.push_deleted(&beads).await);
        let close_reasons = self.step("close reasons", self.push_close_reasons(&beads).await);
        let comments = self.step("comments", self.push_comments(&beads).await);
        let relations = self.step("relations", self.push_relations(&beads).await);
        fields
            .merge(assignees)
            .merge(deleted)
            .merge(close_reasons)
            .merge(comments)
            .merge(relations)
    }

    fn step(&self, name: &str, result: Result<Outcome>) -> Outcome {
        result.unwrap_or_else(|e| {
            warn!("pushing {name} failed: {e:#}");
            Outcome::Failed
        })
    }

    async fn run_pass(&self) {
        if self.sync_pass().await == Outcome::Pushed && !self.opts.dry_run {
            self.sync_pass().await;
        }
    }

    async fn push_fields(&self, beads: &[Bead]) -> Result<Outcome> {
        let current: BTreeMap<String, String> = beads
            .iter()
            .filter(|b| !b.detached())
            .map(|b| (b.id.clone(), fingerprint(b)))
            .collect();
        let previous: BTreeMap<String, String> = match self.load("pushed.json") {
            Some(previous) => previous,
            None if self.opts.initial_push => BTreeMap::new(),
            None => {
                info!(
                    "first run: recording {} bead(s) as the baseline (use --initial-push to push them all)",
                    current.len()
                );
                self.save("pushed.json", &current)?;
                return Ok(Outcome::Nothing);
            }
        };
        let mut kept: BTreeMap<String, String> = previous
            .into_iter()
            .filter(|(id, _)| current.contains_key(id))
            .collect();
        let changed: Vec<String> = current
            .iter()
            .filter(|(id, fp)| kept.get(*id) != Some(fp))
            .map(|(id, _)| id.clone())
            .collect();
        if changed.is_empty() {
            self.save("pushed.json", &kept)?;
            return Ok(Outcome::Nothing);
        }
        if self.opts.dry_run {
            info!(
                "would push {} bead(s): {}",
                changed.len(),
                changed.join(" ")
            );
            return Ok(Outcome::Pushed);
        }

        info!("pushing {} bead(s): {}", changed.len(), changed.join(" "));
        let out = self.bd().github_push(&changed).await?;
        if !out.combined().is_empty() {
            eprintln!("{}", out.combined());
        }
        if !out.success {
            warn!("push failed; will retry");
            return Ok(Outcome::Failed);
        }
        let lower = out.combined().to_lowercase();
        if ["failed to create", "failed to update", "failed to push"]
            .iter()
            .any(|s| lower.contains(s))
        {
            warn!("push reported per-issue failures; will retry those beads");
            return Ok(Outcome::Failed);
        }
        for id in changed {
            let fp = current[&id].clone();
            kept.insert(id, fp);
        }
        self.save("pushed.json", &kept)?;

        if let Err(e) = self
            .bd()
            .dolt_commit("bd-gh-sync: link GitHub issues")
            .await
        {
            warn!("could not commit issue links to Dolt: {e:#}");
        }
        if !self.opts.dolt_sync.is_zero() {
            match self.bd().dolt_push().await {
                Ok(()) => info!("pushed beads to the Dolt remote"),
                Err(e) => warn!("bd dolt push failed; will retry after the next change: {e:#}"),
            }
        }
        Ok(Outcome::Pushed)
    }

    async fn push_assignees(&self, beads: &[Bead]) -> Result<Outcome> {
        let file = "assignees.json";
        let mut current: BTreeMap<String, String> = beads
            .iter()
            .map(|b| (b.id.clone(), b.assignee.clone().unwrap_or_default()))
            .collect();
        let previous: BTreeMap<String, Option<String>> = match self.load(file) {
            Some(previous) => previous,
            None if self.opts.initial_push => BTreeMap::new(),
            None => {
                self.save(file, &current)?;
                return Ok(Outcome::Nothing);
            }
        };
        let old = |id: &str| previous.get(id).cloned().flatten();
        let links = links(beads);
        let todo: Vec<(String, String, String)> = current
            .iter()
            .filter(|(id, want)| **want != old(id).unwrap_or_default())
            .filter_map(|(id, want)| Some((id.clone(), links.get(id)?.clone(), want.clone())))
            .collect();

        let mut outcome = Outcome::Nothing;
        for (id, path, want) in todo {
            let revert = |current: &mut BTreeMap<String, String>| match old(&id) {
                Some(v) => current.insert(id.clone(), v),
                None => current.remove(&id),
            };
            if self.opts.dry_run {
                info!("would set the assignee of {id} to '{want}'");
                outcome.pushed();
                continue;
            }
            let issue = self.call(Method::GET, &path, None).await;
            if !issue.ok() {
                warn!(
                    "{id}: could not read {path} (HTTP {}); will retry",
                    issue.status
                );
                revert(&mut current);
                outcome = Outcome::Failed;
                continue;
            }
            if issue.body["assignee"]["login"].as_str().unwrap_or("") == want {
                continue;
            }
            let assignees: Vec<&str> = if want.is_empty() {
                vec![]
            } else {
                vec![want.as_str()]
            };
            let resp = self
                .call(Method::PATCH, &path, Some(&json!({"assignees": assignees})))
                .await;
            if resp.ok() {
                info!("{id}: assignee set to '{want}' on GitHub");
                outcome.pushed();
            } else if resp.permanent_failure() {
                warn!(
                    "{id}: GitHub refused assignee '{want}' (HTTP {}; is it a GitHub login?); not retrying",
                    resp.status
                );
            } else {
                warn!(
                    "{id}: could not set the assignee (HTTP {}); will retry",
                    resp.status
                );
                revert(&mut current);
                outcome = Outcome::Failed;
            }
        }
        self.save(file, &current)?;
        Ok(outcome)
    }

    async fn push_deleted(&self, beads: &[Bead]) -> Result<Outcome> {
        let file = "links.json";
        let current = links(beads);
        let mut previous: BTreeMap<String, String> = match self.load(file) {
            Some(previous) => previous,
            None => {
                self.save(file, &current)?;
                return Ok(Outcome::Nothing);
            }
        };
        if beads.is_empty() && !previous.is_empty() {
            warn!("bd exported no beads; not treating them as deleted");
            return Ok(Outcome::Failed);
        }
        let gone: Vec<(String, String)> = previous
            .iter()
            .filter(|(id, _)| !beads.iter().any(|b| &b.id == *id))
            .map(|(id, path)| (id.clone(), path.clone()))
            .collect();

        let mut outcome = Outcome::Nothing;
        for (id, path) in gone {
            if self.opts.dry_run {
                info!("would close the issue of deleted bead {id}");
                outcome.pushed();
                continue;
            }
            match self.bd().exists(&id).await {
                Ok(false) => {}
                Ok(true) => {
                    previous.remove(&id);
                    continue;
                }
                Err(e) => {
                    warn!("{id}: could not confirm the bead is deleted; will retry: {e:#}");
                    outcome = Outcome::Failed;
                    continue;
                }
            }
            let issue = self.call(Method::GET, &path, None).await;
            if matches!(issue.status, 404 | 410) || (issue.ok() && issue.body["state"] == "closed")
            {
                previous.remove(&id);
                continue;
            }
            if !issue.ok() {
                warn!(
                    "{id}: could not read {path} (HTTP {}); will retry",
                    issue.status
                );
                outcome = Outcome::Failed;
                continue;
            }
            let resp = self
                .call(
                    Method::PATCH,
                    &path,
                    Some(&json!({"state": "closed", "state_reason": "not_planned"})),
                )
                .await;
            if resp.ok() {
                info!("{id} was deleted: closed {path} on GitHub as not planned");
                outcome.pushed();
                previous.remove(&id);
            } else if resp.permanent_failure() {
                warn!(
                    "{id}: GitHub refused to close {path} (HTTP {}); not retrying",
                    resp.status
                );
                previous.remove(&id);
            } else {
                warn!(
                    "{id}: could not close {path} (HTTP {}); will retry",
                    resp.status
                );
                outcome = Outcome::Failed;
            }
        }
        for (id, path) in current {
            previous.insert(id, path);
        }
        self.save(file, &previous)?;
        Ok(outcome)
    }

    async fn push_close_reasons(&self, beads: &[Bead]) -> Result<Outcome> {
        let file = "close-reasons.json";
        let links = links(beads);
        let mut current: BTreeMap<String, String> = beads
            .iter()
            .filter(|b| b.is_closed())
            .map(|b| {
                let superseder = b
                    .dependencies()
                    .iter()
                    .find(|d| d.kind == "supersedes" && d.issue_id == b.id)
                    .filter(|d| links.contains_key(&d.depends_on_id));
                let want = match superseder {
                    Some(d) => format!("duplicate:{}", d.depends_on_id),
                    None => {
                        state_reason_for(b.close_reason.as_deref().unwrap_or_default()).to_string()
                    }
                };
                (b.id.clone(), want)
            })
            .collect();
        let previous: BTreeMap<String, String> = match self.load(file) {
            Some(previous) => previous,
            None if self.opts.initial_push => BTreeMap::new(),
            None => {
                self.save(file, &current)?;
                return Ok(Outcome::Nothing);
            }
        };
        let todo: Vec<(String, String, String)> = current
            .iter()
            .filter(|(id, want)| previous.get(*id) != Some(want))
            .filter_map(|(id, want)| Some((id.clone(), links.get(id)?.clone(), want.clone())))
            .collect();

        let mut outcome = Outcome::Nothing;
        for (id, path, want) in todo {
            let revert = |current: &mut BTreeMap<String, String>| match previous.get(&id) {
                Some(v) => current.insert(id.clone(), v.clone()),
                None => current.remove(&id),
            };
            if self.opts.dry_run {
                info!("would close {id} on GitHub as {want}");
                outcome.pushed();
                continue;
            }
            let issue = self.call(Method::GET, &path, None).await;
            if !issue.ok() {
                warn!(
                    "{id}: could not read {path} (HTTP {}); will retry",
                    issue.status
                );
                revert(&mut current);
                outcome = Outcome::Failed;
                continue;
            }
            if let Some((_, target)) = want.split_once(':') {
                match self
                    .mark_duplicate(&issue.body, &links[target], target, &id)
                    .await
                {
                    Ok(true) => outcome.pushed(),
                    Ok(false) => {}
                    Err(e) => {
                        warn!("{id}: could not mark as a duplicate of {target}: {e:#}; will retry");
                        revert(&mut current);
                        outcome = Outcome::Failed;
                    }
                }
                continue;
            }
            if issue.body["state"] == "closed"
                && issue.body["state_reason"].as_str().unwrap_or("completed") == want
            {
                continue;
            }
            let resp = self
                .call(
                    Method::PATCH,
                    &path,
                    Some(&json!({"state": "closed", "state_reason": want})),
                )
                .await;
            if resp.ok() {
                info!("{id}: closed on GitHub as {want}");
                outcome.pushed();
            } else if resp.permanent_failure() {
                warn!(
                    "{id}: GitHub refused the close reason '{want}' (HTTP {}); not retrying",
                    resp.status
                );
            } else {
                warn!(
                    "{id}: could not set the close reason (HTTP {}); will retry",
                    resp.status
                );
                revert(&mut current);
                outcome = Outcome::Failed;
            }
        }
        self.save(file, &current)?;
        Ok(outcome)
    }

    async fn mark_duplicate(
        &self,
        issue: &Value,
        target_path: &str,
        target: &str,
        id: &str,
    ) -> Result<bool> {
        let canonical = self.gh.get(target_path).await?;
        let (Some(issue_node), Some(canonical_node)) =
            (issue["node_id"].as_str(), canonical["node_id"].as_str())
        else {
            bail!("GitHub returned no node ids");
        };
        let current = self
            .gh
            .graphql(
                "query($id: ID!) { node(id: $id) { ... on Issue { stateReason duplicateOf { id } } } }",
                json!({"id": issue_node}),
            )
            .await?;
        let node = &current["node"];
        if node["stateReason"] == "DUPLICATE" && node["duplicateOf"]["id"] == canonical_node {
            return Ok(false);
        }
        self.gh
            .graphql(
                "mutation($issue: ID!, $canonical: ID!) { closeIssue(input: {issueId: $issue, stateReason: DUPLICATE, duplicateIssueId: $canonical}) { issue { number } } }",
                json!({"issue": issue_node, "canonical": canonical_node}),
            )
            .await?;
        info!("{id}: closed on GitHub as a duplicate of {target}");
        Ok(true)
    }

    async fn push_comments(&self, beads: &[Bead]) -> Result<Outcome> {
        let file = "comments.json";
        let links = links(beads);
        let current: Vec<(&str, &str, &str)> = beads
            .iter()
            .flat_map(|b| {
                b.comments()
                    .iter()
                    .map(move |c| (c.id.as_str(), b.id.as_str(), c.text.as_str()))
            })
            .collect();
        let mut handled: Vec<String> = match self.load(file) {
            Some(handled) => handled,
            None if self.opts.initial_push => Vec::new(),
            None => {
                let ids: Vec<&str> = current.iter().map(|c| c.0).collect();
                self.save(file, &ids)?;
                return Ok(Outcome::Nothing);
            }
        };

        let mut outcome = Outcome::Nothing;
        for &(id, bead, text) in &current {
            let Some(path) = links.get(bead) else {
                continue;
            };
            if handled.iter().any(|h| h == id) {
                continue;
            }
            if self.opts.dry_run {
                info!("would post comment {id} of {bead}");
                outcome.pushed();
                continue;
            }
            let existing = match self
                .gh
                .get_all(&format!("{path}/comments?per_page=100"))
                .await
            {
                Ok(existing) => existing,
                Err(e) => {
                    warn!("{bead}: could not read comments; will retry: {e:#}");
                    outcome = Outcome::Failed;
                    continue;
                }
            };
            let marker = format!("{COMMENT_MARKER}{id} -->");
            let on_github = existing.iter().any(|c| {
                let body = c["body"].as_str().unwrap_or("");
                body.contains(&marker) || normalize(body) == normalize(text)
            }) || is_sync_note(text);
            if !on_github {
                let body = json!({"body": format!("{text}\n\n{marker}")});
                let resp = self
                    .call(Method::POST, &format!("{path}/comments"), Some(&body))
                    .await;
                if resp.ok() {
                    info!("{bead}: posted a comment");
                    outcome.pushed();
                } else if resp.permanent_failure() {
                    warn!(
                        "{bead}: GitHub refused a comment (HTTP {}); not retrying",
                        resp.status
                    );
                } else {
                    warn!(
                        "{bead}: could not post a comment (HTTP {}); will retry",
                        resp.status
                    );
                    outcome = Outcome::Failed;
                    continue;
                }
            }
            handled.push(id.to_string());
        }
        handled.retain(|h| current.iter().any(|c| c.0 == h));
        self.save(file, &handled)?;
        Ok(outcome)
    }

    async fn push_relations(&self, beads: &[Bead]) -> Result<Outcome> {
        let file = "relations.json";
        let links = links(beads);
        let current: BTreeSet<String> = beads
            .iter()
            .flat_map(Bead::relations)
            .map(|d| format!("{} {} {}", d.issue_id, d.kind, d.depends_on_id))
            .collect();
        let mut synced: BTreeSet<String> = match self.load(file) {
            Some(synced) => synced,
            None if self.opts.initial_push => BTreeSet::new(),
            None => {
                self.save(file, &current)?;
                return Ok(Outcome::Nothing);
            }
        };

        let ends = |edge: &str| -> Option<(String, String, String)> {
            let mut parts = edge.split(' ');
            let (from, kind, to) = (parts.next()?, parts.next()?, parts.next()?);
            Some((from.to_string(), kind.to_string(), to.to_string()))
        };
        let paths = |edge: &str| {
            let (from, _, to) = ends(edge)?;
            Some((links.get(&from)?.clone(), links.get(&to)?.clone()))
        };
        let mut plan: Vec<(Op, String)> = current
            .difference(&synced)
            .filter(|e| paths(e).is_some())
            .map(|e| (Op::Add, e.clone()))
            .collect();
        plan.extend(synced.difference(&current).map(|e| {
            (
                if paths(e).is_some() {
                    Op::Remove
                } else {
                    Op::Forget
                },
                e.clone(),
            )
        }));

        let mut outcome = Outcome::Nothing;
        for (op, edge) in plan {
            if op != Op::Forget
                && let (Some((from_path, to_path)), Some((_, kind, _))) =
                    (paths(&edge), ends(&edge))
            {
                if self.opts.dry_run {
                    info!("would {} {edge} on GitHub", op.verb());
                    outcome.pushed();
                    continue;
                }
                match self.relation_op(op, &kind, &from_path, &to_path).await {
                    Ok(()) => {
                        info!("{edge}: {} on GitHub", op.past());
                        outcome.pushed();
                    }
                    Err(resp) if resp.permanent_failure() => {
                        warn!(
                            "{edge}: GitHub refused to {} it (HTTP {}); not retrying",
                            op.verb(),
                            resp.status
                        );
                    }
                    Err(resp) => {
                        warn!(
                            "{edge}: could not {} it on GitHub (HTTP {}); will retry",
                            op.verb(),
                            resp.status
                        );
                        outcome = Outcome::Failed;
                        continue;
                    }
                }
            }
            if op == Op::Add {
                synced.insert(edge);
            } else {
                synced.remove(&edge);
            }
        }
        self.save(file, &synced)?;
        Ok(outcome)
    }

    async fn relation_op(&self, op: Op, kind: &str, from: &str, to: &str) -> Result<(), Response> {
        let (list, item) = match kind {
            "parent-child" => (format!("{to}/sub_issues?per_page=100"), from),
            _ => (format!("{from}/dependencies/blocked_by?per_page=100"), to),
        };
        let issue = self.call(Method::GET, item, None).await;
        if !issue.ok() {
            return Err(issue);
        }
        let id = issue.body["id"].clone();
        let listed = self.call(Method::GET, &list, None).await;
        if !listed.ok() {
            return Err(listed);
        }
        let present = listed
            .body
            .as_array()
            .is_some_and(|items| items.iter().any(|i| i["id"] == id));
        let resp = match (op, kind, present) {
            (Op::Add, _, true) | (Op::Remove, _, false) => return Ok(()),
            (Op::Add, "parent-child", _) => {
                self.call(
                    Method::POST,
                    &format!("{to}/sub_issues"),
                    Some(&json!({"sub_issue_id": id})),
                )
                .await
            }
            (Op::Add, _, _) => {
                let path = format!("{from}/dependencies/blocked_by");
                self.call(Method::POST, &path, Some(&json!({"issue_id": id})))
                    .await
            }
            (_, "parent-child", _) => {
                self.call(
                    Method::DELETE,
                    &format!("{to}/sub_issue"),
                    Some(&json!({"sub_issue_id": id})),
                )
                .await
            }
            _ => {
                self.call(
                    Method::DELETE,
                    &format!("{from}/dependencies/blocked_by/{id}"),
                    None,
                )
                .await
            }
        };
        if resp.ok() { Ok(()) } else { Err(resp) }
    }

    async fn watch(&self, beads_dir: &Path) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        let mut backend = self.opts.backend;
        let _fs_watcher = match backend {
            Backend::Native => match native_watcher(beads_dir, tx.clone()) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!("file watching unavailable ({e:#}); falling back to polling");
                    backend = Backend::Poll;
                    None
                }
            },
            Backend::Poll => None,
        };
        let dolt_sync = self.opts.dolt_sync;
        let mut poll = self.opts.poll;
        if !dolt_sync.is_zero() && dolt_sync < poll {
            poll = dolt_sync;
        }
        info!(
            "watching {} (backend: {backend:?}, poll: {}, dolt sync: {})",
            beads_dir.display(),
            humantime::format_duration(poll),
            humantime::format_duration(dolt_sync),
        );

        let mut last_pull = Instant::now();
        self.run_pass().await;
        drain(&mut rx, SETTLE).await;
        loop {
            if let Ok(Some(())) = tokio::time::timeout(poll, rx.recv()).await {
                drain(&mut rx, self.opts.debounce).await;
            }
            if !dolt_sync.is_zero() && last_pull.elapsed() >= dolt_sync {
                last_pull = Instant::now();
                self.dolt_pull().await;
            }
            self.run_pass().await;
            drain(&mut rx, SETTLE).await;
        }
    }

    async fn dolt_pull(&self) {
        if let Err(e) = self.bd().dolt_commit("bd-gh-sync: local changes").await {
            warn!("{e:#}");
        }
        if let Err(e) = self.bd().dolt_pull().await {
            warn!("bd dolt pull failed: {e:#}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Add,
    Remove,
    Forget,
}

impl Op {
    fn verb(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Remove => "remove",
            Op::Forget => "forget",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Op::Add => "added",
            Op::Remove => "removed",
            Op::Forget => "forgotten",
        }
    }
}

fn native_watcher(dir: &Path, tx: mpsc::UnboundedSender<()>) -> Result<notify::RecommendedWatcher> {
    use notify::Watcher as _;
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            Ok(event) if is_write(&event.kind) && !event.paths.iter().all(|p| ignored(p)) => {
                let _ = tx.send(());
            }
            Ok(_) => {}
            Err(e) => warn!("file watcher: {e}"),
        })?;
    watcher.watch(dir, notify::RecursiveMode::Recursive)?;
    Ok(watcher)
}

async fn drain(rx: &mut mpsc::UnboundedReceiver<()>, quiet: Duration) {
    let deadline = tokio::time::Instant::now() + DRAIN_CAP;
    while let Ok(Some(())) =
        tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + quiet), rx.recv()).await
    {
    }
}
