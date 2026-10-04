pub mod config;
pub mod project;
pub mod webhook;
pub mod worker;

use std::sync::Arc;

use anyhow::{Context as _, Result};
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
    let project = Project::find(&config.data_dir, repo)?
        .with_context(|| format!("{repo} is not a project here; run `server add` first"))?;
    project.sync(&config, &Secrets::from_env()?, mode).await
}

pub async fn add(config: Config, repo: &str, no_webhook: bool) -> Result<()> {
    let secrets = Secrets::from_env()?;
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
    match handle(&workers, event, &payload) {
        Ok(reply) => reply,
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

fn handle(
    workers: &Workers,
    event: &str,
    payload: &serde_json::Value,
) -> Result<(StatusCode, String)> {
    let (repo, issues) = match webhook::parse(event, payload) {
        Event::Ping => return Ok((StatusCode::OK, "pong".into())),
        Event::Ignored(why) => return Ok((StatusCode::ACCEPTED, format!("ignored: {why}"))),
        Event::Sync { repo, issues } => (repo, issues),
    };
    let Some(project) = Project::find(&workers.context().config.data_dir, &repo)? else {
        return Ok((
            StatusCode::ACCEPTED,
            format!("ignored: {repo} is not a project here"),
        ));
    };
    info!(project = %project.repo, "{event}: queued {issues:?}");
    workers.submit(&project, Job::Issues(issues));
    Ok((StatusCode::ACCEPTED, "queued".into()))
}
