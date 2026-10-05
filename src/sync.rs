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
    Events {
        issues: Vec<u64>,
        changes: Vec<Change>,
    },
    SinceLast,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommentAction {
    Created,
    Edited,
    Deleted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Comment {
        issue: u64,
        action: CommentAction,
        comment: Value,
    },
    Relation {
        edge: String,
        added: bool,
        at: String,
    },
    Field(FieldChange),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldChange {
    pub action: String,
    pub label: Option<String>,
    pub changed: Vec<String>,
    pub sender: Option<String>,
    pub issue: Value,
}

const TYPES: &[&str] = &[
    "bug",
    "feature",
    "task",
    "epic",
    "chore",
    "decision",
    "spike",
    "story",
    "milestone",
];
const PRIORITIES: &[&str] = &["critical", "high", "medium", "low", "backlog"];
const PRIORITY_LABELS: &[&str] = &["critical", "high", "medium", "low", "backlog", "none"];

pub fn priority_rank(name: &str) -> Option<usize> {
    PRIORITIES
        .iter()
        .position(|p| *p == name)
        .or_else(|| (name == "none").then_some(PRIORITIES.len() - 1))
}

pub fn has_priority_label(rank: usize, labels: &[&str]) -> bool {
    labels
        .iter()
        .filter_map(|l| l.strip_prefix("priority::"))
        .any(|name| priority_rank(name) == Some(rank))
}
const IN_PROGRESS_LABEL: &str = "status::in_progress";

pub fn known_label(label: &str) -> bool {
    match label.split_once("::") {
        Some(("type", value)) => TYPES.contains(&value),
        Some(("priority", value)) => PRIORITY_LABELS.contains(&value),
        Some(("status", _)) => label == IN_PROGRESS_LABEL,
        _ => false,
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct FieldPlan {
    pub update: Vec<String>,
    pub reopen: bool,
    pub close: Option<&'static str>,
}

pub fn plan_fields(bead: &Bead, change: &FieldChange) -> FieldPlan {
    let issue = &change.issue;
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| l["name"].as_str())
        .collect();
    let only = |prefix: &str, values: &[&str]| {
        let mut found = labels
            .iter()
            .filter_map(|l| l.strip_prefix(prefix))
            .filter(|v| values.contains(v));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    };
    let mut plan = FieldPlan::default();
    let mut update = Vec::new();
    let mut set = |flag: &str, value: &str| {
        update.push(flag.to_string());
        update.push(value.to_string());
    };
    let changed = |key: &str| change.changed.iter().any(|c| c == key);
    match change.action.as_str() {
        "edited" => {
            if let Some(title) = issue["title"].as_str().filter(|_| changed("title"))
                && bead.title.as_str() != Some(title)
            {
                set("--title", title);
            }
            if changed("body") {
                let body = issue["body"].as_str().unwrap_or_default();
                if normalize(body) != normalize(bead.description.as_deref().unwrap_or_default()) {
                    set("-d", body);
                }
            }
        }
        "assigned" | "unassigned" => {
            let login = issue["assignee"]["login"].as_str().unwrap_or_default();
            if login != bead.assignee.as_deref().unwrap_or_default() {
                set("-a", login);
            }
        }
        "labeled" | "unlabeled" => match change.label.as_deref().and_then(|l| l.split_once("::")) {
            Some(("type", _)) => {
                if let Some(kind) = only("type::", TYPES)
                    && bead.issue_type.as_str() != Some(kind)
                {
                    set("-t", kind);
                }
            }
            Some(("priority", _)) => {
                if let Some(rank) = only("priority::", PRIORITY_LABELS).and_then(priority_rank)
                    && bead.priority.as_u64() != Some(rank as u64)
                {
                    set("-p", &rank.to_string());
                }
            }
            Some(("status", _)) if issue["state"] == "open" => {
                let wanted = if labels.contains(&IN_PROGRESS_LABEL) {
                    "in_progress"
                } else {
                    "open"
                };
                let current = bead.status.as_str().unwrap_or_default();
                if matches!(current, "open" | "in_progress") && current != wanted {
                    set("-s", wanted);
                }
            }
            _ => {}
        },
        "closed" if !bead.is_closed() => {
            let reason = issue["state_reason"].as_str().unwrap_or("completed");
            plan.close = Some(close_reason_for(reason).unwrap_or("Closed"));
        }
        "reopened" if bead.is_closed() => {
            plan.reopen = true;
            if labels.contains(&IN_PROGRESS_LABEL) {
                set("-s", "in_progress");
            }
        }
        _ => {}
    }
    plan.update = update;
    plan
}

const TOMBSTONE: &str = "~";
const APPLIED_KEY: &str = "applied";

type Marks = BTreeMap<String, String>;
type Texts = BTreeMap<u64, BTreeMap<String, TextState>>;

impl Change {
    fn mark(&self) -> Option<(String, String)> {
        let (key, at) = match self {
            Change::Comment {
                issue,
                action,
                comment,
            } => {
                let at = match action {
                    CommentAction::Deleted => TOMBSTONE,
                    _ => comment["updated_at"].as_str()?,
                };
                (format!("{issue}/{}", comment["id"].as_u64()?), at)
            }
            Change::Field(change) => (
                change.issue["number"].as_u64()?.to_string(),
                change.issue["updated_at"].as_str()?,
            ),
            Change::Relation { edge, at, .. } => (format!("e:{edge}"), at.as_str()),
        };
        (!at.is_empty()).then(|| (key, at.to_string()))
    }

    fn is_stale(&self, marks: &Marks) -> bool {
        let Some((key, at)) = self.mark() else {
            return false;
        };
        at != TOMBSTONE && marks.get(&key).is_some_and(|applied| at < *applied)
    }

    fn issues(&self) -> Vec<u64> {
        match self {
            Change::Comment { issue, .. } => vec![*issue],
            Change::Field(change) => change.issue["number"].as_u64().into_iter().collect(),
            Change::Relation { edge, .. } => edge_numbers(edge).into_iter().collect(),
        }
    }
}

fn edge_numbers(edge: &str) -> [u64; 2] {
    let parts: Vec<&str> = edge.split(' ').collect();
    [parts[0], parts[2]].map(|n| n.parse().unwrap_or_default())
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
const OVERWRITTEN_COMMENT: &str = "Overwritten GitHub edit";
const CLOSE_COMMENT_WINDOW: Duration = Duration::from_secs(10);
const FRESH_TTL: Duration = Duration::from_secs(24 * 60 * 60);

pub fn is_sync_note(text: &str) -> bool {
    [
        LINKED_PR_COMMENT,
        EDITED_COMMENT,
        DELETED_COMMENT,
        OVERWRITTEN_COMMENT,
    ]
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
        state_file: if transport == Transport::Dolt {
            dolt_state_file(wd).await
        } else {
            wd.dir
                .join(opts.jsonl.parent().unwrap_or(Path::new("")))
                .join("github-sync.json")
        },
        marks: Default::default(),
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

async fn dolt_state_file(wd: &Workdir) -> PathBuf {
    let git_dir = match wd.output("git", &["rev-parse", "--absolute-git-dir"]).await {
        Ok(out) if out.success => PathBuf::from(out.stdout.trim()),
        _ => wd.dir.join(".beads"),
    };
    git_dir.join("bd-gh-sync").join("sync-state.json")
}

pub async fn verify(wd: &Workdir, gh: &GitHub, repo: &str) -> Result<usize> {
    let opts = Options {
        repo: repo.to_string(),
        mode: Mode::All,
        publish: false,
        transport: Transport::Jsonl,
        jsonl: PathBuf::from(".beads/issues.jsonl"),
        adopt_bd_created: false,
        commit_message: String::new(),
    };
    let sync = Sync {
        wd,
        bd: Bd::new(wd),
        gh,
        opts: &opts,
        transport: Transport::Jsonl,
        mode: Mode::All,
        next_since: String::new(),
        state_file: PathBuf::new(),
        marks: Default::default(),
    };
    sync.drift().await
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
    truncated: BTreeSet<u64>,
}

#[derive(Default)]
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
    marks: std::sync::Mutex<Marks>,
}

#[derive(Debug, PartialEq)]
pub struct IssueInfo {
    pub number: u64,
    pub is_pr: bool,
    pub bd_created: bool,
    pub comments: u64,
    pub state_reason: Option<String>,
    pub closing: Option<Closing>,
    pub updated_at: Option<String>,
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
            updated_at: issue["updated_at"].as_str().map(str::to_string),
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
    let reason = close_reason.trim().to_lowercase();
    let leads = |keyword: &str| {
        reason.strip_prefix(keyword).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with([':', ',', '.', ';', '(', '!'])
                || [" of ", " as ", " -", " (", " #"]
                    .iter()
                    .any(|sep| rest.starts_with(sep))
        })
    };
    if ["duplicate", "dupe", "dup"].iter().any(|k| leads(k)) {
        "duplicate"
    } else if [
        "won't fix",
        "wont fix",
        "wontfix",
        "won't do",
        "wont do",
        "not planned",
        "not_planned",
        "invalid",
    ]
    .iter()
    .any(|k| leads(k))
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TextField {
    Design,
    Acceptance,
    Notes,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TextState {
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub more: Vec<u64>,
    pub text: String,
}

#[derive(Debug, PartialEq)]
pub enum TextAction {
    Keep,
    PushBead,
    ImportGithub,
    Conflict,
}

impl TextField {
    pub const ALL: [TextField; 3] = [TextField::Design, TextField::Acceptance, TextField::Notes];

    pub fn key(self) -> &'static str {
        match self {
            TextField::Design => "design",
            TextField::Acceptance => "acceptance",
            TextField::Notes => "notes",
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            TextField::Design => "--design",
            TextField::Acceptance => "--acceptance",
            TextField::Notes => "--notes",
        }
    }

    fn title(self) -> &'static str {
        match self {
            TextField::Design => "Design",
            TextField::Acceptance => "Acceptance criteria",
            TextField::Notes => "Notes",
        }
    }

    pub fn marker(self) -> String {
        format!("{COMMENT_MARKER}{} -->", self.key())
    }

    pub fn of(self, bead: &Bead) -> &str {
        match self {
            TextField::Design => &bead.design,
            TextField::Acceptance => &bead.acceptance_criteria,
            TextField::Notes => &bead.notes,
        }
        .as_deref()
        .unwrap_or_default()
    }

    pub fn part_marker(self, part: usize) -> String {
        if part == 1 {
            self.marker()
        } else {
            format!("{COMMENT_MARKER}{}:{part} -->", self.key())
        }
    }

    pub fn part_body(self, text: &str, part: usize, parts: usize) -> String {
        let title = match parts {
            1 => self.title().to_string(),
            _ => format!("{} (part {part}/{parts})", self.title()),
        };
        format!("**{title}**\n\n{text}\n\n{}", self.part_marker(part))
    }

    pub fn overwritten(self, text: &str, part: usize, parts: usize) -> String {
        let suffix = match parts {
            1 => String::new(),
            _ => format!(" (part {part}/{parts})"),
        };
        format!(
            "{OVERWRITTEN_COMMENT} to the {}{suffix}:\n\n{text}",
            self.title()
        )
    }

    pub fn part_of(self, body: &str) -> Option<usize> {
        if body.contains(&self.marker()) {
            return Some(1);
        }
        let start = format!("{COMMENT_MARKER}{}:", self.key());
        let rest = &body[body.find(&start)? + start.len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        rest[digits.len()..]
            .starts_with(" -->")
            .then(|| digits.parse().ok())
            .flatten()
    }

    pub fn marked(body: &str) -> Option<TextField> {
        Self::ALL.into_iter().find(|f| f.part_of(body).is_some())
    }

    pub fn text_of(self, body: &str) -> String {
        let mut text = body.to_string();
        let start = format!("{COMMENT_MARKER}{}", self.key());
        while let Some(at) = text.find(&start) {
            let rest = &text[at + start.len()..];
            let digits = rest.strip_prefix(':').map_or(0, |tail| {
                1 + tail.chars().take_while(char::is_ascii_digit).count()
            });
            let end = if rest[digits..].starts_with(" -->") {
                at + start.len() + digits + " -->".len()
            } else {
                at + start.len()
            };
            text.replace_range(at..end, "");
        }
        let text = normalize(&text);
        let header = format!("**{}", self.title());
        match text.split_once('\n') {
            Some((first, rest)) if first.starts_with(&header) && first.ends_with("**") => {
                normalize(rest)
            }
            _ => text,
        }
    }
}

pub const TEXT_LIMIT: usize = 60_000;

fn cut_point(window: &str) -> Option<usize> {
    let fences: Vec<usize> = window.match_indices("```").map(|(i, _)| i).collect();
    let limit = match fences.last() {
        Some(&open) if fences.len() % 2 == 1 && open > 0 => open,
        _ => window.len(),
    };
    let head = &window[..limit];
    let floor = head.len() / 2;
    let sentence = head
        .char_indices()
        .filter(|(_, c)| matches!(c, '.' | '!' | '?'))
        .map(|(i, c)| i + c.len_utf8())
        .rfind(|end| head[*end..].starts_with(char::is_whitespace));
    let after = |found: Option<usize>| found.filter(|at| *at >= floor && *at > 0);
    after(sentence)
        .or_else(|| after(head.rfind("\n\n")))
        .or_else(|| after(head.rfind('\n')))
        .or_else(|| after(head.rfind(char::is_whitespace)))
        .or((limit < window.len() && limit > 0).then_some(limit))
}

pub fn split_text(text: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > max {
        let end = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let cut = cut_point(&rest[..end]).unwrap_or(end);
        parts.push(normalize(&rest[..cut]));
        rest = rest[cut..].trim_start();
    }
    parts.push(normalize(rest));
    parts
}

pub fn rejoin_parts(parts: &[String], references: &[&str]) -> String {
    references
        .iter()
        .find(|reference| split_text(reference, TEXT_LIMIT) == parts)
        .map_or_else(|| parts.join("\n\n"), |reference| normalize(reference))
}

pub fn reconcile_text(bead: &str, github: &str, synced: &str) -> TextAction {
    let (bead, github, synced) = (normalize(bead), normalize(github), normalize(synced));
    if bead == github {
        TextAction::Keep
    } else if github == synced {
        TextAction::PushBead
    } else if bead == synced {
        TextAction::ImportGithub
    } else {
        TextAction::Conflict
    }
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
    truncated: &BTreeSet<u64>,
) -> RelationPlan {
    let is_linked = |edge: &&String| {
        let parts: Vec<&str> = edge.split(' ').collect();
        parts.len() == 3
            && [parts[0], parts[2]]
                .iter()
                .all(|n| n.parse().is_ok_and(|n: u64| linked.contains_key(&n)))
    };
    let cut = |edge: &String| truncated.contains(&edge_numbers(edge)[0]);
    let mut remote: BTreeSet<String> = remote.iter().filter(is_linked).cloned().collect();
    remote.extend(base.iter().filter(|e| cut(e)).cloned());
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

const NODE_FIELDS: &str = r#"number
        parent { number repository { nameWithOwner } }
        blockedBy(first: 100) {
          pageInfo { hasNextPage }
          nodes { number repository { nameWithOwner } }
        }
        duplicateOf { number repository { nameWithOwner } }
        closedByPullRequestsReferences(first: 10, includeClosedPrs: false) {
          pageInfo { hasNextPage }
          nodes { url author { login } }
        }
        timelineItems(first: 100, itemTypes: [CROSS_REFERENCED_EVENT]) {
          pageInfo { hasNextPage }
          nodes {
            ... on CrossReferencedEvent {
              source { ... on Issue { number repository { nameWithOwner } } }
            }
          }
        }"#;

const SCOPED_CHUNK: usize = 25;

fn relations_query() -> String {
    format!(
        "query($owner: String!, $repo: String!, $endCursor: String) {{
  repository(owner: $owner, name: $repo) {{
    issues(first: 100, after: $endCursor) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{ {NODE_FIELDS} }}
    }}
  }}
}}"
    )
}

fn scoped_query(numbers: &[u64]) -> String {
    let issues: String = numbers
        .iter()
        .map(|n| format!("i{n}: issue(number: {n}) {{ {NODE_FIELDS} }}\n"))
        .collect();
    format!(
        "query($owner: String!, $repo: String!) {{ repository(owner: $owner, name: $repo) {{ {issues} }} }}"
    )
}

fn relation_edges(beads: &[Bead], linked: &BTreeMap<u64, String>) -> BTreeSet<String> {
    let number_of: BTreeMap<&str, u64> = linked.iter().map(|(n, id)| (id.as_str(), *n)).collect();
    beads
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
        .collect()
}

fn edge_beads(edge: &str, linked: &BTreeMap<u64, String>) -> Option<(String, String, String)> {
    let parts: Vec<&str> = edge.split(' ').collect();
    let id = |n: &str| linked.get(&n.parse::<u64>().ok()?).cloned();
    Some((id(parts[0])?, parts[1].to_string(), id(parts[2])?))
}

impl Sync<'_> {
    fn repo(&self) -> &str {
        &self.opts.repo
    }

    async fn drift(&self) -> Result<usize> {
        let repo = self.repo();
        let beads = self.bd.export().await?;
        let linked = linked(&beads, repo);
        let issues: BTreeMap<u64, Value> = self
            .gh
            .get_all(&format!("repos/{repo}/issues?state=all&per_page=100"))
            .await?
            .into_iter()
            .filter(|i| i["pull_request"].is_null())
            .filter_map(|i| Some((i["number"].as_u64()?, i)))
            .collect();
        let mut found = 0;
        let mut report = |n: u64, id: &str, what: String| {
            println!("#{n} {id}: {what}");
            found += 1;
        };
        for (n, id) in &linked {
            let Some(bead) = beads.iter().find(|b| &b.id == id) else {
                continue;
            };
            if bead.detached() {
                continue;
            }
            let Some(issue) = issues.get(n) else {
                report(*n, id, "issue not found on GitHub".into());
                continue;
            };
            let text = |value: &Value| normalize(value.as_str().unwrap_or_default());
            if text(&bead.title) != text(&issue["title"]) {
                report(
                    *n,
                    id,
                    format!("title differs: {:?}", text(&issue["title"])),
                );
            }
            if normalize(bead.description.as_deref().unwrap_or_default()) != text(&issue["body"]) {
                report(*n, id, "description differs".into());
            }
            if bead.is_closed() != (issue["state"] == "closed") {
                report(
                    *n,
                    id,
                    format!(
                        "state differs: bead {}, GitHub {}",
                        bead.status, issue["state"]
                    ),
                );
            }
            let labels: Vec<&str> = issue["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["name"].as_str())
                .collect();
            if let Some(kind) = bead.issue_type.as_str()
                && !labels.contains(&format!("type::{kind}").as_str())
            {
                report(*n, id, format!("type label type::{kind} missing"));
            }
            if let Some(rank) = bead.priority.as_u64().map(|p| p as usize)
                && let Some(name) = PRIORITIES.get(rank)
                && !has_priority_label(rank, &labels)
            {
                report(*n, id, format!("priority label priority::{name} missing"));
            }
            if (bead.status.as_str() == Some("in_progress")) != labels.contains(&IN_PROGRESS_LABEL)
                && !bead.is_closed()
            {
                report(*n, id, format!("{IN_PROGRESS_LABEL} label differs"));
            }
            let assignee = issue["assignee"]["login"].as_str().unwrap_or_default();
            if bead.assignee.as_deref().unwrap_or_default() != assignee {
                report(*n, id, format!("assignee differs: GitHub has {assignee:?}"));
            }
            let fields: Vec<(TextField, String)> = TextField::ALL
                .into_iter()
                .map(|f| (f, normalize(f.of(bead))))
                .filter(|(_, text)| !text.is_empty())
                .collect();
            let comments: Vec<(String, String)> = bead
                .comments()
                .iter()
                .filter(|c| !is_sync_note(&c.text))
                .map(|c| (c.id.clone(), normalize(&c.text)))
                .collect();
            if comments.is_empty() && fields.is_empty() {
                continue;
            }
            let github = match self
                .gh
                .get_all(&format!("repos/{repo}/issues/{n}/comments?per_page=100"))
                .await
            {
                Ok(github) => github,
                Err(e) => {
                    report(*n, id, format!("could not read comments: {e:#}"));
                    continue;
                }
            };
            let bodies: Vec<&str> = github.iter().filter_map(|c| c["body"].as_str()).collect();
            for (cid, text) in &comments {
                let marker = format!("{COMMENT_MARKER}{cid} -->");
                if !bodies
                    .iter()
                    .any(|b| b.contains(&marker) || normalize(b) == *text)
                {
                    report(*n, id, format!("comment {cid} is not on GitHub"));
                }
            }
            for (field, want) in &fields {
                match bodies.iter().find(|b| TextField::marked(b) == Some(*field)) {
                    None => report(*n, id, format!("{} comment is missing", field.key())),
                    Some(body) if field.text_of(body) != *want => {
                        report(*n, id, format!("{} comment differs", field.key()))
                    }
                    Some(_) => {}
                }
            }
        }
        let GithubLinks { relations, .. } = self.github_relations(None).await?;
        let have = relation_edges(&beads, &linked);
        let remote: BTreeSet<String> = relations
            .into_iter()
            .filter(|edge| edge_numbers(edge).iter().all(|n| linked.contains_key(n)))
            .collect();
        for edge in have.difference(&remote) {
            let n = edge_numbers(edge)[0];
            report(
                n,
                &linked[&n],
                format!("{}: only in the bead", edge_for_log(edge)),
            );
        }
        for edge in remote.difference(&have) {
            let n = edge_numbers(edge)[0];
            report(
                n,
                &linked[&n],
                format!("{}: only on GitHub", edge_for_log(edge)),
            );
        }
        Ok(found)
    }

    async fn state_get(&self, key: &str) -> Result<Option<String>> {
        if let Ok(text) = std::fs::read_to_string(&self.state_file) {
            let state: Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", self.state_file.display()))?;
            if let Some(value) = state[key].as_str().filter(|s| !s.is_empty()) {
                return Ok(Some(value.to_string()));
            }
        }
        Ok(None)
    }

    async fn state_set(&self, key: &str, value: &str) -> Result<()> {
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

    fn mark(&self, key: String, at: String) {
        let mut marks = self.marks.lock().unwrap();
        match marks.get(&key) {
            Some(known) if *known >= at => {}
            _ => {
                marks.insert(key, at);
            }
        }
    }

    async fn applied_marks(&self) -> Result<Marks> {
        match self.state_get(APPLIED_KEY).await? {
            Some(text) => serde_json::from_str(&text).context("parsing the applied timestamps"),
            None => Ok(Marks::new()),
        }
    }

    async fn flush_marks(&self, linked: &BTreeMap<u64, String>) -> Result<()> {
        let pending = std::mem::take(&mut *self.marks.lock().unwrap());
        if pending.is_empty() {
            return Ok(());
        }
        let before = self.applied_marks().await?;
        let mut all = before.clone();
        for (key, at) in pending {
            match all.get(&key) {
                Some(known) if *known >= at => {}
                _ => {
                    all.insert(key, at);
                }
            }
        }
        all.retain(|key, _| {
            let key = key.strip_prefix("e:").unwrap_or(key);
            key.split(['/', ' '])
                .filter_map(|part| part.parse::<u64>().ok())
                .any(|n| linked.contains_key(&n))
        });
        if all != before {
            self.state_set(APPLIED_KEY, &serde_json::to_string(&all)?)
                .await?;
        }
        Ok(())
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
            if let Some(at) = issue.updated_at {
                self.mark(issue.number.to_string(), at);
            }
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

    async fn texts(&self) -> Result<Texts> {
        match self.state_get("texts").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded texts"),
            None => Ok(Texts::new()),
        }
    }

    async fn import_marked(&self, n: u64, bead: &Bead, comments: &[Value], texts: &mut Texts) {
        for field in TextField::ALL {
            let mut found: BTreeMap<usize, (u64, String)> = BTreeMap::new();
            for comment in comments {
                let body = comment["body"].as_str().unwrap_or_default();
                if let (Some(part), Some(id)) = (field.part_of(body), comment["id"].as_u64()) {
                    found.entry(part).or_insert((id, field.text_of(body)));
                }
            }
            let Some(&(id, _)) = found.values().next() else {
                continue;
            };
            let current = normalize(field.of(bead));
            let synced = texts
                .get(&n)
                .and_then(|fields| fields.get(field.key()))
                .map(|state| state.text.clone());
            let parts: Vec<String> = found.values().map(|(_, text)| text.clone()).collect();
            let mut references = vec![current.as_str()];
            references.extend(synced.as_deref());
            let github = rejoin_parts(&parts, &references);
            let import = match &synced {
                _ if current == github => false,
                None => current.is_empty(),
                Some(synced) => {
                    reconcile_text(&current, &github, synced) == TextAction::ImportGithub
                }
            };
            if import && !github.is_empty() {
                if let Err(e) = self.bd.set_text(&bead.id, field.flag(), &github).await {
                    warn!("#{n}: could not import the {}: {e:#}", field.key());
                    continue;
                }
                info!("#{n}: imported the {} edited on GitHub", field.key());
            } else if current != github {
                continue;
            }
            texts.entry(n).or_default().insert(
                field.key().to_string(),
                TextState {
                    id: Some(id),
                    more: found.values().skip(1).map(|(id, _)| *id).collect(),
                    text: github,
                },
            );
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
        let texts_before = self.texts().await?;
        let mut texts = texts_before.clone();
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
            self.import_marked(*n, bead, &github, &mut texts).await;
            let have: Vec<(String, String)> = bead
                .comments()
                .iter()
                .map(|c| (c.author.clone().unwrap_or_default(), c.text.clone()))
                .collect();
            let previous = before.get(n).cloned().unwrap_or_default();
            let mut plan = plan_comments(&github, &have, &previous);
            for comment in &github {
                if let (Some(cid), Some(at)) =
                    (comment["id"].as_u64(), comment["updated_at"].as_str())
                {
                    self.mark(format!("{n}/{cid}"), at.to_string());
                }
            }
            for (cid, _) in &plan.deleted {
                self.mark(format!("{n}/{cid}"), TOMBSTONE.to_string());
            }
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
            self.apply_comment_plan(*n, id, &plan, &mut tracked, &previous)
                .await;
            if tracked.is_empty() {
                all.remove(n);
            } else {
                all.insert(*n, tracked);
            }
        }
        let texts_changed = texts != texts_before;
        if texts_changed {
            self.state_set("texts", &serde_json::to_string(&texts)?)
                .await?;
        }
        if all == before {
            return Ok((texts_changed, from_comment));
        }
        self.state_set("comments", &serde_json::to_string(&all)?)
            .await?;
        Ok((true, from_comment))
    }

    fn opening(&self, changes: &[Change], linked: &BTreeMap<u64, String>) -> BTreeSet<u64> {
        changes
            .iter()
            .filter_map(|change| match change {
                Change::Field(field) if field.action == "opened" => Some(&field.issue),
                _ => None,
            })
            .filter_map(IssueInfo::from_json)
            .filter(|info| {
                !info.is_pr
                    && !linked.contains_key(&info.number)
                    && (!info.bd_created || self.opts.adopt_bd_created)
            })
            .map(|info| info.number)
            .collect()
    }

    async fn create_opened(&self, issue: &Value) -> Result<String> {
        let number = issue["number"].as_u64().context("issue without a number")?;
        let prefix = self
            .bd
            .config_get("issue_prefix")
            .await?
            .context("no issue_prefix configured")?;
        let beads = self.bd.export().await?;
        let taken: BTreeSet<String> = beads.iter().map(|b| b.id.clone()).collect();
        let config = self.id_config().await;
        let len = ids::adaptive_len(taken.len() + 1, &config);
        let key = format!("{}#{number}", self.repo().to_lowercase());
        let id = ids::short_id(&prefix, &key, len, &config, &taken);
        let labels: Vec<&str> = issue["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str())
            .collect();
        let only = |prefix: &str, values: &[&str]| {
            labels
                .iter()
                .filter_map(|l| l.strip_prefix(prefix))
                .find(|v| values.contains(v))
        };
        let priority = only("priority::", PRIORITY_LABELS)
            .and_then(priority_rank)
            .unwrap_or(2);
        let mut fields = vec![
            "-t".to_string(),
            only("type::", TYPES).unwrap_or("task").to_string(),
            "-p".into(),
            priority.to_string(),
            format!(
                "--external-ref={}",
                issue["html_url"].as_str().unwrap_or_default()
            ),
        ];
        if let Some(body) = issue["body"].as_str().filter(|b| !b.trim().is_empty()) {
            fields.push(format!("--description={body}"));
        }
        if let Some(login) = issue["assignee"]["login"].as_str() {
            fields.extend(["-a".to_string(), login.to_string()]);
        }
        if labels.contains(&IN_PROGRESS_LABEL) {
            fields.extend(["-s".to_string(), "in_progress".into()]);
        }
        let plain: Vec<&str> = labels
            .iter()
            .copied()
            .filter(|l| !l.contains("::"))
            .collect();
        if !plain.is_empty() {
            fields.push(format!("--labels={}", plain.join(",")));
        }
        let title = issue["title"].as_str().unwrap_or("(untitled)");
        self.bd.create(&id, title, &fields).await?;
        let mut fresh = self.fresh_beads().await?;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        fresh.insert(id.clone(), now);
        self.state_set("fresh", &serde_json::to_string(&fresh)?)
            .await?;
        Ok(id)
    }

    async fn apply_field_changes(
        &self,
        changes: &[Change],
        skip: &BTreeSet<u64>,
    ) -> Result<Pulled> {
        let mut applied = Pulled::default();
        let queued: Vec<&FieldChange> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Field(field) => Some(field),
                _ => None,
            })
            .filter(|field| {
                field.issue["number"]
                    .as_u64()
                    .is_some_and(|n| !skip.contains(&n))
            })
            .collect();
        if queued.is_empty() {
            return Ok(applied);
        }
        let beads = self.bd.export().await?;
        let mut linked = linked(&beads, self.repo());
        let opening = self.opening(changes, &linked);
        let tracked = self.tracked_comments().await?;
        for field in queued {
            let Some(n) = field.issue["number"].as_u64() else {
                continue;
            };
            if opening.contains(&n) && !linked.contains_key(&n) {
                match self.create_opened(&field.issue).await {
                    Ok(id) => {
                        info!("#{n}: created {id} from the opened event");
                        linked.insert(n, id);
                    }
                    Err(e) => {
                        warn!("#{n}: could not create its bead directly; importing instead: {e:#}");
                        if let Err(e) = self.bd.github_pull(&[n]).await {
                            warn!("#{n}: could not import it: {e:#}");
                        }
                        linked = self::linked(&self.bd.export().await?, self.repo());
                    }
                }
                continue;
            }
            let Some(id) = linked.get(&n) else { continue };
            let Some(bead) = self.bd.export().await?.into_iter().find(|b| &b.id == id) else {
                continue;
            };
            let plan = plan_fields(&bead, field);
            let mut ok = true;
            if plan.reopen {
                ok &= self.report(n, "reopen", self.bd.run(&["reopen", id]).await);
            }
            if let Some(reason) = plan.close {
                ok &= self.report(
                    n,
                    "close",
                    self.bd.run(&["close", id, "--force", "-r", reason]).await,
                );
            }
            if !plan.update.is_empty() {
                ok &= self.report(n, "update", self.bd.update_fields(id, &plan.update).await);
            }
            if ok && field.action == "closed" {
                let info = IssueInfo::from_json(&field.issue);
                if let Some(mut info) = info {
                    if let Some(closing) = info.closing.as_mut() {
                        closing.by = field.sender.clone().or(closing.by.take());
                    }
                    if let Some(reason) = info.state_reason {
                        applied.closed.insert(n, reason);
                    }
                    if let Some(closing) = info.closing {
                        applied.closing.insert(n, closing);
                    }
                    if info.comments > 0 || tracked.contains_key(&n) {
                        applied.commented.push(n);
                    }
                }
            }
        }
        Ok(applied)
    }

    fn report<T>(&self, n: u64, what: &str, result: Result<T>) -> bool {
        match result {
            Ok(_) => {
                info!("#{n}: applied {what} from the webhook payload");
                true
            }
            Err(e) => {
                warn!("#{n}: could not {what}; will retry next run: {e:#}");
                false
            }
        }
    }

    async fn apply_changes(&self, changes: &[Change], skip: &BTreeSet<u64>) -> Result<bool> {
        let comments = self.apply_comment_changes(changes, skip).await?;
        let relations = self.apply_relation_changes(changes, skip).await?;
        Ok(comments || relations)
    }

    async fn apply_comment_plan(
        &self,
        n: u64,
        id: &str,
        plan: &CommentPlan,
        tracked: &mut Tracked,
        previous: &Tracked,
    ) {
        let revert = |cid: u64, tracked: &mut Tracked| match previous.get(&cid) {
            Some(entry) => tracked.insert(cid, entry.clone()),
            None => tracked.remove(&cid),
        };
        for (cid, author, text) in &plan.add {
            match self.bd.comment_add(id, author, text).await {
                Ok(()) => info!("#{n}: imported a comment by {author}"),
                Err(e) => {
                    warn!("#{n}: could not import a comment by {author}: {e:#}");
                    revert(*cid, tracked);
                }
            }
        }
        for (cid, author, text) in &plan.edited {
            let note = format!("{EDITED_COMMENT} {text}");
            match self.bd.comment_add(id, author, &note).await {
                Ok(()) => info!("#{n}: noted an edit by {author}"),
                Err(e) => {
                    warn!("#{n}: could not note an edit by {author}: {e:#}");
                    revert(*cid, tracked);
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
    }

    async fn apply_comment_changes(
        &self,
        changes: &[Change],
        skip: &BTreeSet<u64>,
    ) -> Result<bool> {
        let queued: Vec<(u64, CommentAction, &Value)> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Comment {
                    issue,
                    action,
                    comment,
                } if !skip.contains(issue) => Some((*issue, *action, comment)),
                _ => None,
            })
            .collect();
        if queued.is_empty() {
            return Ok(false);
        }
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        let before = self.tracked_comments().await?;
        let mut all = before.clone();
        let texts_before = self.texts().await?;
        let mut texts = texts_before.clone();
        for (n, action, comment) in queued {
            let Some(id) = linked.get(&n) else { continue };
            let Some(bead) = beads.iter().find(|b| &b.id == id) else {
                continue;
            };
            let Some(cid) = comment["id"].as_u64() else {
                continue;
            };
            if action != CommentAction::Deleted
                && comment["body"]
                    .as_str()
                    .and_then(TextField::marked)
                    .is_some()
            {
                let all = self
                    .gh
                    .get_all(&format!(
                        "repos/{}/issues/{n}/comments?per_page=100",
                        self.repo()
                    ))
                    .await
                    .unwrap_or_else(|_| vec![comment.clone()]);
                self.import_marked(n, bead, &all, &mut texts).await;
            }
            let previous = all.get(&n).cloned().unwrap_or_default();
            let mut tracked = previous.clone();
            let plan = match action {
                CommentAction::Deleted => {
                    tracked.remove(&cid);
                    CommentPlan {
                        deleted: previous
                            .get(&cid)
                            .map(|(author, _)| (cid, author.clone()))
                            .into_iter()
                            .collect(),
                        ..CommentPlan::default()
                    }
                }
                _ => {
                    let have: Vec<(String, String)> = bead
                        .comments()
                        .iter()
                        .map(|c| (c.author.clone().unwrap_or_default(), c.text.clone()))
                        .collect();
                    let known: Tracked = previous
                        .iter()
                        .filter(|(known, _)| **known == cid)
                        .map(|(k, v)| (*k, v.clone()))
                        .collect();
                    let plan = plan_comments(std::slice::from_ref(comment), &have, &known);
                    tracked.extend(plan.tracked.clone());
                    plan
                }
            };
            self.apply_comment_plan(n, id, &plan, &mut tracked, &previous)
                .await;
            if tracked.is_empty() {
                all.remove(&n);
            } else {
                all.insert(n, tracked);
            }
        }
        let texts_changed = texts != texts_before;
        if texts_changed {
            self.state_set("texts", &serde_json::to_string(&texts)?)
                .await?;
        }
        if all == before {
            return Ok(texts_changed);
        }
        self.state_set("comments", &serde_json::to_string(&all)?)
            .await?;
        Ok(true)
    }

    async fn apply_relation_changes(
        &self,
        changes: &[Change],
        skip: &BTreeSet<u64>,
    ) -> Result<bool> {
        let queued: Vec<(&str, bool)> = changes
            .iter()
            .filter_map(|change| match change {
                Change::Relation { edge, added, .. } if !skip.contains(&edge_numbers(edge)[0]) => {
                    Some((edge.as_str(), *added))
                }
                _ => None,
            })
            .collect();
        if queued.is_empty() {
            return Ok(false);
        }
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        let have = relation_edges(&beads, &linked);
        let recorded = self.recorded_relations().await?;
        let mut next = recorded.clone();
        for (edge, added) in queued {
            let Some((from, kind, to)) = edge_beads(edge, &linked) else {
                continue;
            };
            let shown = edge_for_log(edge);
            if added {
                if !have.contains(edge) {
                    if let Err(e) = self.bd.dep_add(&from, &to, &kind).await {
                        warn!("{shown}: could not add; will retry next run: {e:#}");
                        continue;
                    }
                    info!("{shown}: added");
                }
                next.insert(edge.to_string());
            } else if recorded.contains(edge) {
                if have.contains(edge) {
                    if let Err(e) = self.bd.dep_remove(&from, &to).await {
                        warn!("{shown}: could not remove: {e:#}");
                        continue;
                    }
                    info!("{shown}: removed");
                }
                next.remove(edge);
            }
        }
        if next == recorded {
            return Ok(false);
        }
        self.state_set("relations", &serde_json::to_string(&next)?)
            .await?;
        Ok(true)
    }

    async fn github_relations(&self, scope: Option<&BTreeSet<u64>>) -> Result<GithubLinks> {
        let (owner, name) = self
            .repo()
            .split_once('/')
            .context("repository must be owner/name")?;
        let variables = json!({"owner": owner, "repo": name});
        let nodes = match scope {
            None => {
                self.gh
                    .graphql_nodes(&relations_query(), variables, &["repository", "issues"])
                    .await?
            }
            Some(scope) => {
                let numbers: Vec<u64> = scope.iter().copied().collect();
                let mut nodes = Vec::new();
                for chunk in numbers.chunks(SCOPED_CHUNK) {
                    let data = self
                        .gh
                        .graphql(&scoped_query(chunk), variables.clone())
                        .await?;
                    nodes.extend(
                        chunk
                            .iter()
                            .map(|n| data["repository"][format!("i{n}")].clone())
                            .filter(|node| !node.is_null()),
                    );
                }
                nodes
            }
        };
        let own = |r: &Value| {
            r["repository"]["nameWithOwner"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(self.repo()))
        };
        let mut edges = BTreeSet::new();
        let mut mentions = BTreeSet::new();
        let mut pulls = BTreeMap::new();
        let mut duplicates = BTreeMap::new();
        let mut truncated = BTreeSet::new();
        for node in &nodes {
            let Some(n) = node["number"].as_u64() else {
                continue;
            };
            if [
                "blockedBy",
                "timelineItems",
                "closedByPullRequestsReferences",
            ]
            .iter()
            .any(|key| node[*key]["pageInfo"]["hasNextPage"].as_bool() == Some(true))
            {
                warn!("#{n}: GitHub returned only part of its links; keeping existing ones");
                truncated.insert(n);
            }
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
            truncated,
        })
    }

    fn scope(&self) -> Option<BTreeSet<u64>> {
        match &self.mode {
            Mode::Issues(numbers) => Some(numbers.iter().copied().collect()),
            _ => None,
        }
    }

    async fn recorded_relations(&self) -> Result<BTreeSet<String>> {
        match self.state_get("relations").await? {
            Some(text) => serde_json::from_str(&text).context("parsing the recorded relations"),
            None => Ok(BTreeSet::new()),
        }
    }

    async fn sync_relations(&self) -> Result<bool> {
        let scope = self.scope();
        if scope.as_ref().is_some_and(BTreeSet::is_empty) {
            return Ok(false);
        }
        let GithubLinks {
            relations: remote,
            mentions,
            pulls,
            duplicates,
            truncated,
        } = match self.github_relations(scope.as_ref()).await {
            Ok(found) => found,
            Err(e) => {
                warn!("could not read relations from GitHub; skipping them: {e:#}");
                return Ok(false);
            }
        };
        let beads = self.bd.export().await?;
        let linked = linked(&beads, self.repo());
        let in_scope = |edge: &&String| {
            scope.as_ref().is_none_or(|scope| {
                let [from, _] = edge_numbers(edge);
                scope.contains(&from)
            })
        };
        let have: BTreeSet<String> = relation_edges(&beads, &linked)
            .iter()
            .filter(in_scope)
            .cloned()
            .collect();
        let recorded = self.recorded_relations().await?;
        let base: BTreeSet<String> = recorded.iter().filter(in_scope).cloned().collect();
        let plan = plan_relations(&remote, &base, &have, &linked, &truncated);

        let ids = |edge: &str| edge_beads(edge, &linked).expect("planned edges are linked");
        let mut next_base: BTreeSet<String> = recorded
            .iter()
            .filter(|edge| !in_scope(edge))
            .chain(&plan.base)
            .cloned()
            .collect();
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
        if next_base != recorded {
            self.state_set("relations", &serde_json::to_string(&next_base)?)
                .await?;
            changed = true;
        }
        let scoped = scope.is_some();
        let duplicated = self.import_duplicates(&linked, &duplicates).await?;
        let mentioned = self.import_mentions(&linked, &mentions, scoped).await?;
        let pulled = self.import_pulls(&linked, &pulls, scoped).await?;
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
        scoped: bool,
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
        let mut recorded = if scoped {
            seen.union(&next).cloned().collect()
        } else {
            next.clone()
        };
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
        scoped: bool,
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
        let mut recorded = if scoped {
            imported.union(&next).cloned().collect()
        } else {
            next.clone()
        };
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
            let imported: BTreeSet<&str> = beads
                .iter()
                .filter(|b| {
                    b.external_ref
                        .as_deref()
                        .and_then(|r| issue_number(r, self.repo()))
                        .is_some()
                        && ids::auto_shaped(&b.id)
                })
                .map(|b| b.id.as_str())
                .collect();
            let candidate = |id: &str| fresh.contains_key(id) || imported.contains(id);
            let next = taken.iter().find_map(|id| {
                let parent = parent_of(id).filter(|_| candidate(id))?;
                let waiting = candidate(&parent)
                    && parent_of(&parent).is_some_and(|grand| !nested(&parent, &grand));
                (!nested(id, &parent) && !waiting && !nested(&parent, id))
                    .then(|| (id.clone(), parent))
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
        let mut changes = Vec::new();
        if let Mode::Events {
            issues,
            changes: queued,
        } = self.mode.clone()
        {
            let linked = linked(&self.bd.export().await?, self.repo());
            let applied = self.applied_marks().await?;
            let (stale, fresh): (Vec<Change>, Vec<Change>) =
                queued.into_iter().partition(|c| c.is_stale(&applied));
            if !stale.is_empty() {
                info!(
                    "{} stale event(s) arrived out of order; refreshing those issues from GitHub",
                    stale.len()
                );
            }
            let opening = self.opening(&fresh, &linked);
            let mut numbers: BTreeSet<u64> = issues
                .into_iter()
                .filter(|n| !opening.contains(n))
                .collect();
            numbers.extend(stale.iter().flat_map(Change::issues));
            numbers.extend(
                fresh
                    .iter()
                    .flat_map(Change::issues)
                    .filter(|n| !linked.contains_key(n) && !opening.contains(n)),
            );
            self.mode = Mode::Issues(numbers.into_iter().collect());
            changes = fresh;
        }
        let skip: BTreeSet<u64> = self.scope().unwrap_or_default();
        let idle = matches!(&self.mode, Mode::Issues(numbers) if numbers.is_empty());
        let mut comments_changed = false;
        let mut relations_changed = false;
        let mut pulled = None;
        if !idle {
            pulled = Some(self.pull_issues(since.as_deref()).await?);
        }
        let fields = self.apply_field_changes(&changes, &skip).await?;
        if !fields.closed.is_empty() {
            let pulled = pulled.get_or_insert_with(Pulled::default);
            pulled.commented.extend(fields.commented);
            pulled.closed.extend(fields.closed);
            pulled.closing.extend(fields.closing);
        }
        if let Err(e) = self.shorten_imports().await {
            warn!("could not shorten imported bead ids: {e:#}");
        }
        if let Some(pulled) = pulled {
            let (changed, from_comment) = self
                .import_comments(&pulled.commented, &pulled.closing)
                .await?;
            comments_changed = changed;
            self.import_close_reasons(&pulled.closed, &from_comment)
                .await?;
            relations_changed = self.sync_relations().await?;
        }
        let applied = self.apply_changes(&changes, &skip).await?;
        if let Err(e) = self.nest_fresh().await {
            warn!("could not nest new beads under their parents: {e:#}");
        }
        let skipped = &skip;
        for (key, at) in changes
            .iter()
            .filter(|c| !c.issues().iter().any(|n| skipped.contains(n)))
            .filter_map(Change::mark)
        {
            self.mark(key, at);
        }
        let current = linked(&self.bd.export().await?, self.repo());
        if let Err(e) = self.flush_marks(&current).await {
            warn!("could not record the applied timestamps: {e:#}");
        }
        let changed = relations_changed
            || comments_changed
            || applied
            || self.bd.export_raw().await? != before;
        if (changed || self.transport == Transport::Dolt) && !matches!(self.mode, Mode::Issues(_)) {
            self.state_set("since", &self.next_since).await?;
        }
        Ok(changed)
    }

    async fn adopt_kv_state(&self) -> Result<()> {
        if self.state_file.exists() {
            return Ok(());
        }
        let state: serde_json::Map<String, Value> = self
            .bd
            .kv_with_prefix("bd-gh-sync.")
            .await?
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect();
        if state.is_empty() {
            return Ok(());
        }
        info!("moving {} sync state value(s) out of Dolt", state.len());
        if let Some(dir) = self.state_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            &self.state_file,
            format!("{}\n", serde_json::to_string_pretty(&state)?),
        )?;
        Ok(())
    }

    async fn via_dolt(&mut self) -> Result<()> {
        self.bd.bootstrap().await?;
        self.adopt_kv_state().await?;
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
