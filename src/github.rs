use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use reqwest::{
    Client, Method, RequestBuilder,
    header::{HeaderMap, LINK},
};
use serde_json::{Value, json};
use tracing::warn;

pub const WEBHOOK_EVENTS: &[&str] = &[
    "issues",
    "issue_comment",
    "sub_issues",
    "issue_dependencies",
    "issue_relates_to",
    "pull_request",
];

pub const DEFAULT_API_URL: &str = "https://api.github.com";

const WRITE_SPACING: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(300);
const MAX_RETRIES: u32 = 3;
const DEFAULT_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Gate {
    last_write: Option<Instant>,
    blocked_until: Option<Instant>,
}

type SharedGate = Arc<tokio::sync::Mutex<Gate>>;

static GATES: LazyLock<Mutex<HashMap<String, SharedGate>>> = LazyLock::new(Mutex::default);

pub struct GitHub {
    client: Client,
    api: String,
    token: String,
    gate: SharedGate,
}

pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn permanent_failure(&self) -> bool {
        (400..500).contains(&self.status) && self.status != 403 && self.status != 429
    }
}

impl GitHub {
    pub fn new(api: &str, token: &str) -> Self {
        let gate = GATES
            .lock()
            .unwrap()
            .entry(format!("{api} {token}"))
            .or_default()
            .clone();
        Self {
            client: Client::new(),
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
            gate,
        }
    }

    pub fn paused_for(&self) -> Option<Duration> {
        let gate = self.gate.try_lock().ok()?;
        let remaining = gate.blocked_until?.checked_duration_since(Instant::now())?;
        (remaining > MAX_WAIT).then_some(remaining)
    }

    pub async fn pause(&self, wait: Duration) {
        let mut gate = self.gate.lock().await;
        let until = Instant::now() + wait;
        gate.blocked_until = Some(gate.blocked_until.map_or(until, |b| b.max(until)));
    }

    async fn dispatch(
        &self,
        build: impl Fn() -> RequestBuilder,
        write: bool,
    ) -> Result<Sent> {
        let mut attempt = 0;
        loop {
            {
                let mut gate = self.gate.lock().await;
                let now = Instant::now();
                if let Some(remaining) = gate.blocked_until.and_then(|b| b.checked_duration_since(now))
                {
                    if remaining > MAX_WAIT {
                        return Ok(Sent::Paused(remaining));
                    }
                    tokio::time::sleep(remaining).await;
                }
                if write {
                    if let Some(last) = gate.last_write {
                        tokio::time::sleep(WRITE_SPACING.saturating_sub(last.elapsed())).await;
                    }
                    gate.last_write = Some(Instant::now());
                }
            }
            let resp = build().send().await?;
            let Some(wait) = rate_limit_wait(resp.status().as_u16(), resp.headers()) else {
                return Ok(Sent::Done(resp));
            };
            warn!(
                "GitHub rate limit (HTTP {}); waiting {}s before retrying",
                resp.status(),
                wait.as_secs()
            );
            self.pause(wait).await;
            attempt += 1;
            if wait > MAX_WAIT || attempt > MAX_RETRIES {
                return Ok(Sent::Done(resp));
            }
        }
    }

    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .context("GITHUB_TOKEN (or GH_TOKEN) is not set")?;
        let api = std::env::var("GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.into());
        Ok(Self::new(&api, &token))
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/{}", self.api, path.trim_start_matches('/'))
        }
    }

    fn graphql_url(&self) -> String {
        match self.api.strip_suffix("/api/v3") {
            Some(host) => format!("{host}/api/graphql"),
            None => format!("{}/graphql", self.api),
        }
    }

    fn request(&self, method: Method, url: &str) -> RequestBuilder {
        self.client
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "bd-gh-sync")
    }

    pub async fn send(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Response> {
        let url = self.url(path);
        let write = method != Method::GET;
        let sent = self
            .dispatch(
                || {
                    let req = self.request(method.clone(), &url);
                    match body {
                        Some(body) => req.json(body),
                        None => req,
                    }
                },
                write,
            )
            .await
            .with_context(|| format!("{method} {path}"))?;
        let Sent::Done(resp) = sent else {
            return Ok(paused(&sent));
        };
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Response { status, body })
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let resp = self.send(Method::GET, path, None).await?;
        if !resp.ok() {
            bail!("GET {path}: HTTP {}: {}", resp.status, message(&resp.body));
        }
        Ok(resp.body)
    }

    pub async fn get_all(&self, path: &str) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut next = Some(self.url(path));
        while let Some(url) = next.take() {
            let sent = self
                .dispatch(|| self.request(Method::GET, &url), false)
                .await
                .with_context(|| format!("GET {url}"))?;
            let Sent::Done(resp) = sent else {
                bail!("GET {url}: {}", paused(&sent).body["message"]);
            };
            let status = resp.status();
            next = resp
                .headers()
                .get(LINK)
                .and_then(|v| v.to_str().ok())
                .and_then(next_link);
            let body: Value = resp.json().await.with_context(|| format!("GET {url}"))?;
            if !status.is_success() {
                bail!("GET {url}: HTTP {status}: {}", message(&body));
            }
            match body {
                Value::Array(page) => items.extend(page),
                other => bail!("GET {url}: expected a list, got {other}"),
            }
        }
        Ok(items)
    }

    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let url = self.graphql_url();
        let payload = json!({"query": query, "variables": variables});
        let write = query.trim_start().starts_with("mutation");
        let sent = self
            .dispatch(|| self.request(Method::POST, &url).json(&payload), write)
            .await
            .context("GraphQL request")?;
        let Sent::Done(resp) = sent else {
            bail!("GraphQL: {}", paused(&sent).body["message"]);
        };
        let status = resp.status();
        let body: Value = resp.json().await.context("GraphQL response")?;
        if !status.is_success() || body.get("errors").is_some_and(|e| !e.is_null()) {
            bail!(
                "GraphQL: HTTP {status}: {}",
                body.get("errors").unwrap_or(&body)
            );
        }
        Ok(body["data"].clone())
    }

    pub async fn graphql_nodes(
        &self,
        query: &str,
        variables: Value,
        path: &[&str],
    ) -> Result<Vec<Value>> {
        let mut nodes = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let mut vars = variables.clone();
            vars["endCursor"] = cursor;
            let data = self.graphql(query, vars).await?;
            let mut connection = &data;
            for key in path {
                connection = &connection[*key];
            }
            nodes.extend(connection["nodes"].as_array().cloned().unwrap_or_default());
            if connection["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(nodes);
            }
            cursor = connection["pageInfo"]["endCursor"].clone();
        }
    }
}

enum Sent {
    Done(reqwest::Response),
    Paused(Duration),
}

fn paused(sent: &Sent) -> Response {
    let wait = match sent {
        Sent::Paused(wait) => wait.as_secs(),
        Sent::Done(_) => 0,
    };
    Response {
        status: 429,
        body: json!({"message": format!("GitHub rate limit; paused for another {wait}s")}),
    }
}

fn rate_limit_wait(status: u16, headers: &HeaderMap) -> Option<Duration> {
    if status != 403 && status != 429 {
        return None;
    }
    let number = |name: &str| {
        headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    };
    if let Some(secs) = number("retry-after") {
        return Some(Duration::from_secs(secs.max(1)));
    }
    if number("x-ratelimit-remaining") == Some(0) {
        let reset = UNIX_EPOCH + Duration::from_secs(number("x-ratelimit-reset")?);
        let wait = reset.duration_since(SystemTime::now()).unwrap_or_default();
        return Some(wait + Duration::from_secs(1));
    }
    (status == 429).then_some(DEFAULT_BACKOFF)
}

fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == "rel=\"next\"")
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

fn message(body: &Value) -> String {
    body["message"]
        .as_str()
        .map_or_else(|| body.to_string(), str::to_string)
}

pub enum Registered {
    Created,
    Updated,
}

pub async fn register_webhook(
    gh: &GitHub,
    repo: &str,
    url: &str,
    secret: &str,
) -> Result<Registered> {
    let hooks = gh
        .get_all(&format!("repos/{repo}/hooks?per_page=100"))
        .await
        .context("listing webhooks (the token needs Webhooks: read and write)")?;
    let body = json!({
        "name": "web",
        "active": true,
        "events": WEBHOOK_EVENTS,
        "config": {"url": url, "content_type": "json", "secret": secret, "insecure_ssl": "0"},
    });
    let existing = hooks
        .iter()
        .find(|h| h["config"]["url"].as_str() == Some(url))
        .and_then(|h| h["id"].as_u64());
    let (method, path, outcome) = match existing {
        Some(id) => (
            Method::PATCH,
            format!("repos/{repo}/hooks/{id}"),
            Registered::Updated,
        ),
        None => (
            Method::POST,
            format!("repos/{repo}/hooks"),
            Registered::Created,
        ),
    };
    let resp = gh.send(method, &path, Some(&body)).await?;
    if !resp.ok() {
        bail!(
            "saving the webhook: HTTP {}: {}",
            resp.status,
            message(&resp.body)
        );
    }
    Ok(outcome)
}
