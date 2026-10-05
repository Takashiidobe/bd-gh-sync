use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    bd::{Bd, Workdir},
    server::webhook,
    watch::{self, Backend, SETTLE},
};

pub struct Options {
    pub once: bool,
    pub poll: Duration,
    pub debounce: Duration,
    pub backend: Backend,
    pub poke_url: Option<String>,
    pub poke_secret: Option<String>,
    pub repo: Option<String>,
}

struct Relay {
    wd: Workdir,
    opts: Options,
    repo: String,
}

pub async fn run(opts: Options) -> Result<()> {
    let (beads_dir, root) = watch::find_beads()?;
    let wd = Workdir::new(&root)
        .env("BD_NON_INTERACTIVE", "1")
        .env("BD_NO_DEP_TYPE_WARNING", "1");
    let repo = match &opts.repo {
        Some(repo) => repo.clone(),
        None => Bd::new(&wd)
            .config_get("github.repository")
            .await?
            .context("the repository is unknown: pass --repo owner/name or `bd config set github.repository owner/name`")?,
    };
    if opts.poke_url.is_some() != opts.poke_secret.is_some() {
        bail!("--poke-url and --poke-secret go together");
    }
    let relay = Relay { wd, opts, repo };
    relay.push(true).await;
    if relay.opts.once {
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let _fs_watcher = match relay.opts.backend {
        Backend::Native => match watch::native_watcher(&beads_dir, tx) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                warn!("file watching unavailable ({e:#}); polling only");
                None
            }
        },
        Backend::Poll => None,
    };
    info!(
        "relaying {} to the Dolt remote; the server does the rest (poll: {})",
        beads_dir.display(),
        humantime::format_duration(relay.opts.poll),
    );
    tokio::select! {
        () = relay.watch(&mut rx) => Ok(()),
        () = crate::server::shutdown() => Ok(()),
    }
}

impl Relay {
    async fn watch(&self, rx: &mut mpsc::UnboundedReceiver<()>) {
        loop {
            let changed = matches!(
                tokio::time::timeout(self.opts.poll, rx.recv()).await,
                Ok(Some(()))
            );
            if changed {
                watch::drain(rx, self.opts.debounce).await;
            } else {
                self.pull().await;
            }
            self.push(changed).await;
            watch::drain(rx, SETTLE).await;
        }
    }

    async fn pull(&self) {
        if let Err(e) = Bd::new(&self.wd).dolt_pull().await {
            warn!("bd dolt pull failed: {e:#}");
        }
    }

    async fn push(&self, poke: bool) {
        let bd = Bd::new(&self.wd);
        if let Err(e) = bd.dolt_commit("bd-gh-sync: local changes").await {
            warn!("{e:#}");
        }
        for attempt in 1..=3 {
            match bd.dolt_push().await {
                Ok(()) => {
                    info!("pushed local changes to the Dolt remote");
                    if poke {
                        self.poke().await;
                    }
                    return;
                }
                Err(e) => warn!("bd dolt push failed (attempt {attempt}): {e:#}"),
            }
            if let Err(e) = bd.dolt_pull().await {
                warn!("bd dolt pull failed: {e:#}");
                return;
            }
        }
    }

    async fn poke(&self) {
        let (Some(url), Some(secret)) = (&self.opts.poke_url, &self.opts.poke_secret) else {
            return;
        };
        let body = json!({"repo": self.repo}).to_string();
        let sent = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .header(
                "x-hub-signature-256",
                webhook::sign(secret.as_bytes(), body.as_bytes()),
            )
            .body(body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        match sent {
            Ok(_) => info!("poked {url}"),
            Err(e) => warn!("could not poke {url}: {e}; the server will notice within its poll interval"),
        }
    }
}
