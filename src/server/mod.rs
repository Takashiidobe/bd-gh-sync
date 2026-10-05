pub mod config;
pub mod project;
pub mod projects;
pub mod webhook;
pub mod worker;

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
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
        let verb = match github::register_webhook(&gh, repo, &url, &secrets.webhook_secret).await? {
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
    let workers = Arc::new(Workers::new(Arc::new(Context { config, secrets })));

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

    let app = Router::new()
        .route("/webhook", post(receive))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        .with_state(workers);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    info!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
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

async fn receive(
    State(workers): State<Arc<Workers>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, String) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let secret = workers.context().secrets.webhook_secret.as_bytes();
    if !webhook::verify(secret, &body, header("x-hub-signature-256")) {
        warn!("rejected a webhook with a bad signature");
        return (StatusCode::UNAUTHORIZED, "bad signature".into());
    }
    let Some(event) = header("x-github-event") else {
        return (StatusCode::BAD_REQUEST, "missing X-GitHub-Event".into());
    };
    let payload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad JSON: {e}")),
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
        Event::Reconcile { repo } => (repo, Job::SinceLast),
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
