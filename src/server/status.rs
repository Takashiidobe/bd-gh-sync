use std::{
    collections::HashMap,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Attempt {
    pub at: u64,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Seen {
    pub what: String,
    pub at: u64,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct ProjectStatus {
    #[serde(default)]
    pub repo: String,
    pub queued: u64,
    pub last_event: Option<Seen>,
    pub last_push: Option<Attempt>,
    pub last_sync: Option<Attempt>,
    pub remote_head: Option<String>,
    #[serde(default)]
    pub processed_head: Option<String>,
    #[serde(default)]
    pub rate_limited_secs: Option<u64>,
}

#[derive(Default)]
pub struct Status(Mutex<HashMap<String, ProjectStatus>>);

impl Status {
    pub fn update(&self, repo: &str, change: impl FnOnce(&mut ProjectStatus)) {
        change(
            self.0
                .lock()
                .unwrap()
                .entry(repo.to_lowercase())
                .or_default(),
        );
    }

    pub fn get(&self, repo: &str) -> ProjectStatus {
        self.0
            .lock()
            .unwrap()
            .get(&repo.to_lowercase())
            .cloned()
            .unwrap_or_default()
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn attempt(result: &anyhow::Result<()>) -> Attempt {
    Attempt {
        at: now(),
        ok: result.is_ok(),
        error: result.as_ref().err().map(|e| format!("{e:#}")),
    }
}

fn ago(at: u64) -> String {
    let secs = now().saturating_sub(at);
    format!(
        "{} ago",
        humantime::format_duration(std::time::Duration::from_secs(secs))
    )
}

fn describe(label: &str, attempt: &Option<Attempt>) -> String {
    match attempt {
        None => format!("{label}: never"),
        Some(a) if a.ok => format!("{label}: ok {}", ago(a.at)),
        Some(a) => format!(
            "{label}: FAILED {} ({})",
            ago(a.at),
            a.error.as_deref().unwrap_or("unknown error")
        ),
    }
}

pub async fn show(url: &str, secret: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let url = format!("{}/status", url.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(secret)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let code = resp.status();
    if !code.is_success() {
        anyhow::bail!("GET {url}: HTTP {code}: {}", resp.text().await?.trim());
    }
    let projects: Vec<ProjectStatus> = resp.json().await?;
    for p in projects {
        println!("{}", p.repo);
        let head = |h: &Option<String>| {
            h.as_deref()
                .map_or("none", |h| &h[..h.len().min(8)])
                .to_string()
        };
        let dolt = if p.remote_head == p.processed_head {
            "in sync".to_string()
        } else {
            "waiting for the server to process the newest push".to_string()
        };
        println!(
            "  dolt: remote {} / processed {} ({dolt})",
            head(&p.remote_head),
            head(&p.processed_head)
        );
        println!("  queued: {}", p.queued);
        match &p.last_event {
            Some(e) => println!("  last event: {} {}", e.what, ago(e.at)),
            None => println!("  last event: none since the server started"),
        }
        println!("  {}", describe("push", &p.last_push));
        println!("  {}", describe("sync", &p.last_sync));
        if let Some(secs) = p.rate_limited_secs {
            println!("  GitHub rate limit: paused for another {secs}s");
        }
    }
    Ok(())
}
