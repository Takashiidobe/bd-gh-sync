pub mod config;
pub mod project;
pub mod projects;
pub mod status;
pub mod webhook;
pub mod worker;

use std::{net::SocketAddr, sync::Arc};

use anyhow::{Context as _, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use tracing::{info, warn};

use crate::{
    github::{self, GitHub},
    sync::Mode,
};
use config::{Config, Secrets};
use project::Project;
use webhook::Event;
use worker::{Context, Job, Workers};

pub fn list(config: &Config) -> Result<()> {
    for project in Project::discover(&config.data_dir)? {
        println!("{}\t{}", project.repo, project.dir.display());
    }
    Ok(())
}

pub async fn sync(config: Config, repo: &str, mode: Mode) -> Result<()> {
    let secrets = Secrets::from_env()?;
    require_projects_token(&config, &secrets)?;
    let project = Project::find(&config.data_dir, repo)?
        .with_context(|| format!("{repo} is not a project here; run `server add` first"))?;
    project.sync(&config, &secrets, mode).await
}

pub async fn push(config: Config, repo: &str) -> Result<()> {
    let secrets = Secrets::from_env()?;
    let project = Project::find(&config.data_dir, repo)?
        .with_context(|| format!("{repo} is not a project here; run `server add` first"))?;
    project.push(&config, &secrets).await
}

pub async fn add(config: Config, repo: &str, no_webhook: bool) -> Result<()> {
    let secrets = Secrets::from_env()?;
    require_projects_token(&config, &secrets)?;
    let webhook_url = if no_webhook {
        None
    } else {
        Some(config.webhook_url()?)
    };
    let project = Project::new(&config.data_dir, repo)?;
    if project.exists() {
        info!(project = %repo, "already cloned at {}", project.dir.display());
    } else {
        info!(project = %repo, "cloning into {}", project.dir.display());
        project.clone_from(&config, &secrets).await?;
    }
    project
        .sync(&config, &secrets, Mode::SinceLast)
        .await
        .context("initial sync failed")?;
    if let Some(url) = webhook_url {
        let gh = GitHub::new(&config.api_url, &secrets.token);
        let secret = config::ensure_repo_secret(&config.data_dir, repo)?;
        let verb = match github::register_webhook(&gh, repo, &url, &secret).await? {
            github::Registered::Created => "created",
            github::Registered::Updated => "updated",
        };
        info!(project = %repo, "webhook {verb}: {url}");
    }
    Ok(())
}

pub async fn serve(config: Config) -> Result<()> {
    let secrets = Secrets::from_env()?;
    require_projects_token(&config, &secrets)?;
    let listen = config.listen;
    let workers = Arc::new(Workers::new(Arc::new(Context {
        config,
        secrets,
        status: Default::default(),
    })));

    if Project::discover(&workers.context().config.data_dir)?.is_empty() {
        warn!("no projects yet; add one with `bd-gh-sync server add owner/name`");
    }
    tokio::spawn({
        let workers = workers.clone();
        async move {
            let mut every = tokio::time::interval(workers.context().config.reconcile_every());
            loop {
                every.tick().await;
                match Project::discover(&workers.context().config.data_dir) {
                    Ok(projects) => projects
                        .iter()
                        .for_each(|p| workers.submit(p, Job::SinceLast)),
                    Err(e) => warn!("listing projects: {e:#}"),
                }
            }
        }
    });

    if let Some(every) = workers.context().config.dolt_poll() {
        tokio::spawn(poll_dolt_heads(workers.clone(), every));
    }

    let app = Router::new()
        .route("/webhook", post(receive))
        .route("/poke", post(poke))
        .route("/status", get(status))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(workers);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    info!("listening on {}", listener.local_addr()?);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

async fn poll_dolt_heads(workers: Arc<Workers>, every: std::time::Duration) {
    let ctx = workers.context();
    let mut seen: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let projects = match Project::discover(&ctx.config.data_dir) {
            Ok(projects) => projects,
            Err(e) => {
                warn!("listing projects: {e:#}");
                continue;
            }
        };
        for project in projects {
            let head = match project.remote_head(&ctx.config, &ctx.secrets).await {
                Ok(Some(head)) => head,
                Ok(None) => continue,
                Err(e) => {
                    warn!(project = %project.repo, "reading refs/dolt/data: {e:#}");
                    continue;
                }
            };
            ctx.status
                .update(&project.repo, |s| s.remote_head = Some(head.clone()));
            let last = seen
                .entry(project.repo.clone())
                .or_insert_with(|| project.processed_head());
            if last.as_deref() != Some(head.as_str()) {
                info!(project = %project.repo, "refs/dolt/data moved to {head}");
                *last = Some(head);
                workers.submit(&project, Job::Push);
            }
        }
    }
}

async fn status(
    State(workers): State<Arc<Workers>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> (StatusCode, String) {
    let source = peer.ip().to_string();
    let context = workers.context();
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if workers.blocked(&source) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failures".into());
    }
    if !presented.is_some_and(|p| {
        webhook::secret_matches(context.secrets.webhook_secret.as_bytes(), p.as_bytes())
    }) {
        workers.bad_signature(&source);
        return (StatusCode::UNAUTHORIZED, "bad token".into());
    }
    let projects = match Project::discover(&context.config.data_dir) {
        Ok(projects) => projects,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let paused = GitHub::new(&context.config.api_url, &context.secrets.token).paused_for();
    let all: Vec<status::ProjectStatus> = projects
        .iter()
        .map(|project| {
            let mut status = context.status.get(&project.repo);
            status.repo = project.repo.clone();
            status.processed_head = project.processed_head();
            status.rate_limited_secs = paused.map(|p| p.as_secs());
            status
        })
        .collect();
    (
        StatusCode::OK,
        serde_json::to_string_pretty(&all).unwrap_or_default(),
    )
}

async fn poke(
    State(workers): State<Arc<Workers>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let source = header("x-forwarded-for")
        .and_then(|list| list.split(',').next())
        .map_or_else(|| peer.ip().to_string(), |ip| ip.trim().to_string());
    if workers.blocked(&source) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many failures".into());
    }
    let repo = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["repo"].as_str().map(str::to_string));
    let context = workers.context();
    let Some(repo) = repo else {
        workers.bad_signature(&source);
        return (
            StatusCode::BAD_REQUEST,
            "expected {\"repo\": \"owner/name\"}".into(),
        );
    };
    let secret = config::repo_secret(&context.config.data_dir, &repo)
        .unwrap_or_else(|| context.secrets.webhook_secret.clone());
    if !webhook::verify(secret.as_bytes(), &body, header("x-hub-signature-256")) {
        workers.bad_signature(&source);
        warn!("rejected a poke from {source} with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature".into());
    }
    match Project::find(&context.config.data_dir, &repo) {
        Ok(Some(project)) => {
            info!(project = %project.repo, "poked");
            workers.submit(&project, Job::Push);
            (StatusCode::ACCEPTED, "queued".into())
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("{repo} is not a project here"),
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn require_projects_token(config: &Config, secrets: &Secrets) -> Result<()> {
    if !config.project_sync.is_empty() && secrets.projects_token.is_none() {
        bail!("GITHUB_PROJECTS_TOKEN is required when project_sync is configured");
    }
    Ok(())
}

pub async fn shutdown() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

const MAX_BODY: usize = 1024 * 1024;

async fn receive(
    State(workers): State<Arc<Workers>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let source = header("x-forwarded-for")
        .and_then(|list| list.split(',').next())
        .map_or_else(|| peer.ip().to_string(), |ip| ip.trim().to_string());
    if workers.blocked(&source) {
        warn!("rejected a webhook from {source}: too many bad signatures");
        return (StatusCode::TOO_MANY_REQUESTS, "too many failures".into());
    }
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(e) => {
            workers.bad_signature(&source);
            return (StatusCode::BAD_REQUEST, format!("bad JSON: {e}"));
        }
    };
    let context = workers.context();
    let secret = payload["repository"]["full_name"]
        .as_str()
        .and_then(|repo| config::repo_secret(&context.config.data_dir, repo))
        .unwrap_or_else(|| context.secrets.webhook_secret.clone());
    if !webhook::verify(secret.as_bytes(), &body, header("x-hub-signature-256")) {
        workers.bad_signature(&source);
        warn!("rejected a webhook from {source} with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature".into());
    }
    let Some(event) = header("x-github-event") else {
        return (StatusCode::BAD_REQUEST, "missing X-GitHub-Event".into());
    };
    let delivery = header("x-github-delivery");
    if let Some(id) = delivery.filter(|id| workers.seen_delivery(id)) {
        info!("ignored duplicate delivery {id}");
        return (StatusCode::OK, "duplicate delivery".into());
    }
    match handle(&workers, event, &payload) {
        Ok(reply) => {
            if let Some(id) = delivery {
                workers.remember_delivery(id);
            }
            reply
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn handle(
    workers: &Workers,
    event: &str,
    payload: &serde_json::Value,
) -> Result<(StatusCode, String)> {
    let (repo, job) = match webhook::parse(event, payload) {
        Event::Ping => return Ok((StatusCode::OK, "pong".into())),
        Event::Projects => {
            let mut queued = std::collections::BTreeSet::new();
            for entry in &workers.context().config.project_sync {
                if !queued.insert(entry.repo.to_lowercase()) {
                    continue;
                }
                if let Some(project) =
                    Project::find(&workers.context().config.data_dir, &entry.repo)?
                {
                    workers.submit(&project, Job::SinceLast);
                }
            }
            return Ok((
                StatusCode::ACCEPTED,
                "queued configured project syncs".into(),
            ));
        }
        Event::Ignored(why) => return Ok((StatusCode::ACCEPTED, format!("ignored: {why}"))),
        Event::Sync { repo, issues } => (repo, Job::Issues(issues)),
        Event::Changes { repo, changes } => (repo, Job::Changes(changes)),
        Event::Pulls { repo, numbers } => (repo, Job::Pulls(numbers)),
    };
    let Some(project) = Project::find(&workers.context().config.data_dir, &repo)? else {
        return Ok((
            StatusCode::ACCEPTED,
            format!("ignored: {repo} is not a project here"),
        ));
    };
    match &job {
        Job::Changes(changes) => {
            info!(project = %project.repo, "{event}: queued {} change(s)", changes.len())
        }
        job => info!(project = %project.repo, "{event}: queued {job:?}"),
    }
    workers.submit(&project, job);
    Ok((StatusCode::ACCEPTED, "queued".into()))
}
