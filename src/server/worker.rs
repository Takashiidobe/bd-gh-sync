use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    time::{Instant, timeout_at},
};
use tracing::{error, info};

use crate::{
    server::{
        config::{Config, Secrets},
        project::Project,
    },
    sync::Mode,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Issues(BTreeSet<u64>),
    SinceLast,
}

#[derive(Debug, Default, PartialEq)]
pub struct Batch {
    issues: BTreeSet<u64>,
    since_last: bool,
}

impl Batch {
    pub fn add(&mut self, job: Job) {
        match job {
            Job::Issues(issues) => self.issues.extend(issues),
            Job::SinceLast => self.since_last = true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.issues.is_empty() && !self.since_last
    }

    pub fn mode(&self) -> Mode {
        if self.since_last {
            Mode::SinceLast
        } else {
            Mode::Issues(self.issues.iter().copied().collect())
        }
    }
}

pub async fn next_batch(
    rx: &mut UnboundedReceiver<Job>,
    debounce: Duration,
    max_wait: Duration,
) -> Option<Batch> {
    let mut batch = Batch::default();
    batch.add(rx.recv().await?);
    let deadline = Instant::now() + max_wait;
    loop {
        let quiet = (Instant::now() + debounce).min(deadline);
        match timeout_at(quiet, rx.recv()).await {
            Ok(Some(job)) => batch.add(job),
            Ok(None) | Err(_) => return Some(batch),
        }
    }
}

pub struct Context {
    pub config: Config,
    pub secrets: Secrets,
}

pub struct Workers {
    ctx: Arc<Context>,
    queues: Mutex<HashMap<String, UnboundedSender<Job>>>,
}

impl Workers {
    pub fn new(ctx: Arc<Context>) -> Self {
        Self {
            ctx,
            queues: Mutex::default(),
        }
    }

    pub fn context(&self) -> &Context {
        &self.ctx
    }

    pub fn submit(&self, project: &Project, job: Job) {
        let mut queues = self.queues.lock().unwrap();
        let tx = queues
            .entry(project.repo.to_lowercase())
            .or_insert_with(|| {
                let (tx, rx) = unbounded_channel();
                tokio::spawn(run(self.ctx.clone(), project.clone(), tx.clone(), rx));
                tx
            });
        let _ = tx.send(job);
    }
}

const RETRY_AFTER: Duration = Duration::from_secs(30);

async fn run(
    ctx: Arc<Context>,
    project: Project,
    tx: UnboundedSender<Job>,
    mut rx: UnboundedReceiver<Job>,
) {
    let config = &ctx.config;
    while let Some(batch) = next_batch(&mut rx, config.debounce(), config.max_wait()).await {
        if batch.is_empty() {
            continue;
        }
        match project.sync(config, &ctx.secrets, batch.mode()).await {
            Ok(()) => info!(project = %project.repo, "sync done"),
            Err(e) => {
                error!(project = %project.repo, "sync failed: {e:#}; retrying in {}s", RETRY_AFTER.as_secs());
                let tx = tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(RETRY_AFTER).await;
                    let _ = tx.send(Job::SinceLast);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issues(n: &[u64]) -> Job {
        Job::Issues(n.iter().copied().collect())
    }

    #[test]
    fn batch_mode() {
        let mut batch = Batch::default();
        assert!(batch.is_empty());
        batch.add(issues(&[5, 4]));
        batch.add(issues(&[4, 7]));
        assert_eq!(batch.mode(), Mode::Issues(vec![4, 5, 7]));
        batch.add(Job::SinceLast);
        assert_eq!(batch.mode(), Mode::SinceLast);
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_becomes_one_batch() {
        let (tx, mut rx) = unbounded_channel();
        let debounce = Duration::from_secs(2);
        tokio::spawn({
            let tx = tx.clone();
            async move {
                for n in 1..=3 {
                    tx.send(issues(&[n])).unwrap();
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
                tx.send(issues(&[9])).unwrap();
            }
        });
        let first = next_batch(&mut rx, debounce, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(first.mode(), Mode::Issues(vec![1, 2, 3]));
        let second = next_batch(&mut rx, debounce, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(second.mode(), Mode::Issues(vec![9]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_steady_stream_is_cut_at_max_wait() {
        let (tx, mut rx) = unbounded_channel();
        tokio::spawn(async move {
            for n in 1..=100 {
                if tx.send(issues(&[n])).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let batch = next_batch(&mut rx, Duration::from_secs(2), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(batch.issues.len() <= 11, "{:?}", batch.issues);
    }
}
