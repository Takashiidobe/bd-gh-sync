use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tracing::info;

#[derive(Debug, Clone)]
pub struct Workdir {
    pub dir: PathBuf,
    pub vars: Vec<(String, String)>,
}

pub struct Output {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr).trim().to_string()
    }
}

impl Workdir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            vars: Vec::new(),
        }
    }

    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.vars.push((key.to_string(), value.into()));
        self
    }

    pub fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(&self.dir)
            .envs(self.vars.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        cmd
    }

    pub async fn output(&self, program: &str, args: &[&str]) -> Result<Output> {
        let out = self
            .command(program)
            .args(args)
            .output()
            .await
            .with_context(|| format!("running {program}"))?;
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub async fn run(&self, program: &str, args: &[&str]) -> Result<String> {
        let out = self.output(program, args).await?;
        if !out.success {
            bail!("`{program} {}` failed:\n{}", args.join(" "), out.combined());
        }
        Ok(out.stdout)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Bead {
    #[serde(rename = "_type")]
    pub kind: Option<String>,
    pub id: String,
    pub title: Value,
    pub description: Option<String>,
    pub status: Value,
    pub close_reason: Option<String>,
    pub priority: Value,
    pub issue_type: Value,
    pub labels: Option<Vec<String>>,
    pub assignee: Option<String>,
    pub external_ref: Option<String>,
    pub estimate: Option<i64>,
    pub defer_until: Option<String>,
    pub updated_at: Option<String>,
    pub comments: Option<Vec<Comment>>,
    pub dependencies: Option<Vec<Dependency>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Comment {
    pub id: String,
    pub author: Option<String>,
    pub text: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Dependency {
    pub issue_id: String,
    pub depends_on_id: String,
    #[serde(rename = "type")]
    pub kind: String,
}

pub const DELETED_LABEL: &str = "github-deleted";
pub const MOVED_LABEL: &str = "github-transferred";

pub const RELATION_TYPES: &[&str] = &["blocks", "parent-child"];

impl Bead {
    pub fn comments(&self) -> &[Comment] {
        self.comments.as_deref().unwrap_or_default()
    }

    pub fn detached(&self) -> bool {
        self.labels
            .iter()
            .flatten()
            .any(|l| l == DELETED_LABEL || l == MOVED_LABEL)
    }

    pub fn is_closed(&self) -> bool {
        self.status.as_str() == Some("closed")
    }

    pub fn dependencies(&self) -> &[Dependency] {
        self.dependencies.as_deref().unwrap_or_default()
    }

    pub fn relations(&self) -> impl Iterator<Item = &Dependency> {
        self.dependencies
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|d| RELATION_TYPES.contains(&d.kind.as_str()))
    }
}

pub fn parse_export(text: &str) -> Result<Vec<Bead>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Bead>(line).context("parsing bd export"))
        .filter(|bead| {
            bead.as_ref()
                .map_or(true, |b| b.kind.as_deref().is_none_or(|k| k == "issue"))
        })
        .collect()
}

pub struct Bd<'a> {
    wd: &'a Workdir,
    bin: String,
    export_cache: Mutex<Option<String>>,
}

static TEMP_FILES: AtomicU64 = AtomicU64::new(0);

impl<'a> Bd<'a> {
    pub fn new(wd: &'a Workdir) -> Self {
        Self {
            wd,
            bin: std::env::var("BD").unwrap_or_else(|_| "bd".into()),
            export_cache: Mutex::new(None),
        }
    }

    pub async fn run(&self, args: &[&str]) -> Result<String> {
        if !matches!(args.first(), Some(&"export" | &"config" | &"kv")) {
            *self.export_cache.lock().expect("export cache poisoned") = None;
        }
        let started = Instant::now();
        let result = self.wd.run(&self.bin, args).await;
        info!(
            command = args.first().copied().unwrap_or_default(),
            subcommand = args.get(1).copied().unwrap_or_default(),
            elapsed_ms = started.elapsed().as_millis(),
            "bd command completed"
        );
        result
    }

    pub async fn output(&self, args: &[&str]) -> Result<Output> {
        if !matches!(args.first(), Some(&"export" | &"config" | &"kv")) {
            *self.export_cache.lock().expect("export cache poisoned") = None;
        }
        let started = Instant::now();
        let result = self.wd.output(&self.bin, args).await;
        info!(
            command = args.first().copied().unwrap_or_default(),
            subcommand = args.get(1).copied().unwrap_or_default(),
            elapsed_ms = started.elapsed().as_millis(),
            "bd command completed"
        );
        result
    }

    pub async fn export_raw(&self) -> Result<String> {
        if let Some(export) = self
            .export_cache
            .lock()
            .expect("export cache poisoned")
            .clone()
        {
            info!("reused bd export within sync pass");
            return Ok(export);
        }
        let started = Instant::now();
        let export = self.wd.run(&self.bin, &["export"]).await?;
        info!(
            command = "export",
            elapsed_ms = started.elapsed().as_millis(),
            "bd command completed"
        );
        *self.export_cache.lock().expect("export cache poisoned") = Some(export.clone());
        Ok(export)
    }

    pub async fn export(&self) -> Result<Vec<Bead>> {
        parse_export(&self.export_raw().await?)
    }

    pub async fn export_to(&self, path: &Path) -> Result<()> {
        self.run(&["export", "-o", &path.to_string_lossy()])
            .await
            .map(drop)
    }

    pub async fn import(&self, path: &Path) -> Result<()> {
        self.run(&["import", "--allow-stale", &path.to_string_lossy()])
            .await
            .map(drop)
    }

    pub async fn bootstrap(&self) -> Result<()> {
        let beads = self.wd.dir.join(".beads");
        if beads.join("metadata.json").is_file() && beads.join("embeddeddolt").is_dir() {
            info!("skipping bd bootstrap for initialized embedded Dolt clone");
            return Ok(());
        }
        self.run(&["bootstrap", "--yes"]).await.map(drop)
    }

    pub async fn config_get(&self, key: &str) -> Result<Option<String>> {
        let out = self.output(&["config", "get", key, "--json"]).await?;
        Ok(json_field(&out.stdout, "value"))
    }

    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let out = self.output(&["kv", "get", key, "--json"]).await?;
        let found = serde_json::from_str::<Value>(&out.stdout)
            .is_ok_and(|v| v["found"].as_bool() == Some(true));
        Ok(if found {
            json_field(&out.stdout, "value")
        } else {
            None
        })
    }

    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.run(&["kv", "set", key, value]).await.map(drop)
    }

    pub async fn dolt_commit(&self, message: &str) -> Result<()> {
        let out = self.output(&["dolt", "commit", "-m", message]).await?;
        if !out.success && !out.combined().to_lowercase().contains("nothing to commit") {
            bail!("bd dolt commit failed:\n{}", out.combined());
        }
        Ok(())
    }

    pub async fn dolt_pull(&self) -> Result<()> {
        self.run(&["dolt", "pull"]).await.map(drop)
    }

    pub async fn dolt_push(&self) -> Result<()> {
        self.run(&["dolt", "push"]).await.map(drop)
    }

    pub async fn github_pull(&self, numbers: &[u64]) -> Result<Output> {
        let numbers: Vec<String> = numbers.iter().map(u64::to_string).collect();
        let mut args = vec!["github", "pull"];
        args.extend(numbers.iter().map(String::as_str));
        let out = self.output(&args).await?;
        if !out.success {
            bail!("bd github pull failed:\n{}", out.combined());
        }
        Ok(out)
    }

    pub async fn github_push(&self, ids: &[String]) -> Result<Output> {
        let mut args = vec!["github", "push"];
        args.extend(ids.iter().map(String::as_str));
        self.output(&args).await
    }

    pub async fn comment_add(&self, id: &str, author: &str, text: &str) -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "bd-gh-sync-{}-{}.txt",
            std::process::id(),
            TEMP_FILES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, text)?;
        let result = self
            .run(&[
                "comments",
                "add",
                id,
                "-a",
                author,
                "-f",
                &path.to_string_lossy(),
            ])
            .await;
        let _ = std::fs::remove_file(&path);
        result.map(drop)
    }

    pub async fn set_close_reason(&self, id: &str, reason: &str) -> Result<()> {
        self.run(&["reopen", id]).await?;
        self.run(&["close", id, "--force", "-r", reason])
            .await
            .map(drop)
    }

    pub async fn set_status(&self, id: &str, status: &str) -> Result<()> {
        self.run(&["update", id, "-s", status]).await.map(drop)
    }

    pub async fn update_fields(&self, id: &str, fields: &[String]) -> Result<()> {
        let mut args = vec!["update".to_string(), id.to_string()];
        args.extend(fields.iter().cloned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs).await.map(drop)
    }

    pub async fn label_add(&self, id: &str, label: &str) -> Result<()> {
        self.run(&["label", "add", id, label]).await.map(drop)
    }

    pub async fn set_external_ref(&self, id: &str, external_ref: &str) -> Result<()> {
        self.run(&["update", id, "--external-ref", external_ref])
            .await
            .map(drop)
    }

    pub async fn exists(&self, id: &str) -> Result<bool> {
        let out = self.output(&["show", id, "--json"]).await?;
        if out.success {
            return Ok(true);
        }
        if out.combined().to_lowercase().contains("not found") {
            return Ok(false);
        }
        bail!("bd show {id} failed:\n{}", out.combined())
    }

    pub async fn rename(&self, old: &str, new: &str) -> Result<()> {
        self.run(&["rename", old, new]).await.map(drop)
    }

    pub async fn dep_add(&self, from: &str, to: &str, kind: &str) -> Result<()> {
        self.run(&["dep", "add", from, to, "-t", kind])
            .await
            .map(drop)
    }

    pub async fn dep_remove(&self, from: &str, to: &str) -> Result<()> {
        self.run(&["dep", "remove", from, to]).await.map(drop)
    }
}

fn json_field(text: &str, key: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}
