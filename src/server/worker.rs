use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
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
    sync::{Change, Mode},
};

#[derive(Debug, Clone, PartialEq)]
pub enum Job {
    Issues(BTreeSet<u64>),
    Changes(Vec<Change>),
    SinceLast,
}

#[derive(Debug, Default, PartialEq)]
pub struct Batch {
    issues: BTreeSet<u64>,
    changes: Vec<Change>,
    since_last: bool,
}

impl Batch {
    pub fn add(&mut self, job: Job) {
        match job {
            Job::Issues(issues) => self.issues.extend(issues),
            Job::Changes(changes) => self.changes.extend(changes),
            Job::SinceLast => self.since_last = true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.issues.is_empty() && self.changes.is_empty() && !self.since_last
    }

    pub fn mode(&self) -> Mode {
        if self.since_last {
            Mode::SinceLast
        } else if self.changes.is_empty() {
            Mode::Issues(self.issues.iter().copied().collect())
        } else {
            Mode::Events {
                issues: self.issues.iter().copied().collect(),
                changes: self.changes.clone(),
            }
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

const REMEMBERED_DELIVERIES: usize = 4096;

#[derive(Default)]
struct Deliveries {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

pub struct Workers {
    ctx: Arc<Context>,
    queues: Mutex<HashMap<String, UnboundedSender<Job>>>,
    deliveries: Mutex<Deliveries>,
}

impl Workers {
    pub fn new(ctx: Arc<Context>) -> Self {
        Self {
            ctx,
            queues: Mutex::default(),
            deliveries: Mutex::default(),
        }
    }

    pub fn seen_delivery(&self, id: &str) -> bool {
        self.deliveries.lock().unwrap().ids.contains(id)
    }

    pub fn remember_delivery(&self, id: &str) {
        let mut deliveries = self.deliveries.lock().unwrap();
        if !deliveries.ids.insert(id.to_string()) {
            return;
        }
        deliveries.order.push_back(id.to_string());
        if deliveries.order.len() > REMEMBERED_DELIVERIES
            && let Some(oldest) = deliveries.order.pop_front()
        {
            deliveries.ids.remove(&oldest);
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
