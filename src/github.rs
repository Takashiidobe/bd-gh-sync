use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, RequestBuilder, header::LINK};
use serde_json::{Value, json};

pub const WEBHOOK_EVENTS: &[&str] = &[
    "issues",
    "issue_comment",
    "sub_issues",
    "issue_dependencies",
    "issue_relates_to",
    "pull_request",
];

pub const DEFAULT_API_URL: &str = "https://api.github.com";

pub struct GitHub {
    client: Client,
    api: String,
    token: String,
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
        Self {
            client: Client::new(),
            api: api.trim_end_matches('/').to_string(),
            token: token.to_string(),
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
        let mut req = self.request(method.clone(), &self.url(path));
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
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
            let resp = self
                .request(Method::GET, &url)
                .send()
                .await
                .with_context(|| format!("GET {url}"))?;
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
        let resp = self
            .request(Method::POST, &self.graphql_url())
            .json(&json!({"query": query, "variables": variables}))
            .send()
            .await
            .context("GraphQL request")?;
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
