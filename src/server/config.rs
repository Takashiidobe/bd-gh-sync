use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::sync::Transport;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    pub public_url: Option<String>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_ms: u64,
    #[serde(default = "default_reconcile_minutes")]
    pub reconcile_minutes: u64,
    #[serde(default = "default_dolt_poll_seconds")]
    pub dolt_poll_seconds: u64,
    #[serde(default = "default_transport")]
    pub transport: Transport,
    #[serde(default = "default_git_base")]
    pub git_base: String,
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(default)]
    pub project_sync: Vec<ProjectSync>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSync {
    pub repo: String,
    pub project_id: String,
    pub status_field: String,
    pub status: BTreeMap<String, String>,
    pub priority_field: String,
    pub priority: BTreeMap<String, String>,
    pub estimate_field: Option<String>,
    pub iteration_field: Option<String>,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:8787".parse().unwrap()
}
fn default_data_dir() -> PathBuf {
    "/var/lib/bd-gh-sync".into()
}
fn default_debounce_ms() -> u64 {
    2000
}
fn default_max_wait_ms() -> u64 {
    10000
}
fn default_reconcile_minutes() -> u64 {
    60
}
fn default_dolt_poll_seconds() -> u64 {
    10
}
fn default_transport() -> Transport {
    Transport::Auto
}
fn default_git_base() -> String {
    "https://github.com".into()
}
fn default_api_url() -> String {
    std::env::var("GITHUB_API_URL").unwrap_or_else(|_| crate::github::DEFAULT_API_URL.into())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(config)
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }

    pub fn max_wait(&self) -> Duration {
        Duration::from_millis(self.max_wait_ms.max(self.debounce_ms))
    }

    pub fn reconcile_every(&self) -> Duration {
        Duration::from_secs(self.reconcile_minutes.max(1) * 60)
    }

    pub fn dolt_poll(&self) -> Option<Duration> {
        (self.dolt_poll_seconds > 0).then(|| Duration::from_secs(self.dolt_poll_seconds))
    }

    pub fn webhook_url(&self) -> Result<String> {
        let base = self
            .public_url
            .as_deref()
            .context("public_url is not set in the config; GitHub needs it to reach this server")?;
        Ok(format!("{}/webhook", base.trim_end_matches('/')))
    }
}

pub struct Secrets {
    pub token: String,
    pub projects_token: Option<String>,
    pub webhook_secret: String,
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .context("GITHUB_TOKEN is not set")?;
        let webhook_secret =
            std::env::var("WEBHOOK_SECRET").context("WEBHOOK_SECRET is not set")?;
        if webhook_secret.len() < 16 {
            bail!("WEBHOOK_SECRET is too short; use at least 16 random characters");
        }
        Ok(Self {
            token,
            projects_token: std::env::var("GITHUB_PROJECTS_TOKEN")
                .ok()
                .filter(|token| !token.is_empty()),
            webhook_secret,
        })
    }
}

fn secret_path(data_dir: &Path, repo: &str) -> PathBuf {
    data_dir
        .join(".secrets")
        .join(repo.to_lowercase().replace('/', "__"))
}

pub fn repo_secret(data_dir: &Path, repo: &str) -> Option<String> {
    crate::server::project::validate_repo(repo).ok()?;
    let text = std::fs::read_to_string(secret_path(data_dir, repo)).ok()?;
    let secret = text.trim();
    (!secret.is_empty()).then(|| secret.to_string())
}

pub fn ensure_repo_secret(data_dir: &Path, repo: &str) -> Result<String> {
    if let Some(secret) = repo_secret(data_dir, repo) {
        return Ok(secret);
    }
    let mut bytes = [0u8; 32];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    let secret = hex::encode(bytes);
    let path = secret_path(data_dir, repo);
    let dir = path.parent().context("secret path has a parent")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    std::io::Write::write_all(&mut file.open(&path)?, secret.as_bytes())?;
    Ok(secret)
}
