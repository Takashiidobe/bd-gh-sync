use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{Instrument, info, info_span, warn};

use crate::{
    bd::{Bd, Workdir},
    github::GitHub,
    server::config::{Config, Secrets},
    sync::{self, Mode, Options},
    watch,
};

#[derive(Debug, Clone)]
pub struct Project {
    pub repo: String,
    pub dir: PathBuf,
}

pub fn validate_repo(repo: &str) -> Result<()> {
    let ok = repo.split('/').count() == 2
        && repo.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    if !ok {
        bail!("not a repository name: {repo:?} (expected owner/name)");
    }
    Ok(())
}

pub fn describe(mode: &Mode) -> String {
    match mode {
        Mode::Issues(numbers) => {
            let numbers: Vec<String> = numbers.iter().map(u64::to_string).collect();
            format!("issues {}", numbers.join(" "))
        }
        Mode::Events { issues, changes } => {
            format!("{} event(s) and {} issue(s)", changes.len(), issues.len())
        }
        Mode::SinceLast => "everything since the last sync".into(),
        Mode::All => "every issue".into(),
    }
}

impl Project {
    pub fn new(data_dir: &Path, repo: &str) -> Result<Self> {
        validate_repo(repo)?;
        Ok(Self {
            repo: repo.to_string(),
            dir: data_dir.join(repo),
        })
    }

    pub fn exists(&self) -> bool {
        self.dir.join(".git").exists()
    }

    pub fn discover(data_dir: &Path) -> Result<Vec<Project>> {
        let mut projects = Vec::new();
        let Ok(owners) = std::fs::read_dir(data_dir) else {
            return Ok(projects);
        };
        for owner in owners {
            let owner = owner?;
            let owner_name = owner.file_name().to_string_lossy().into_owned();
            if owner_name.starts_with('.') || !owner.file_type()?.is_dir() {
                continue;
            }
            for repo in std::fs::read_dir(owner.path())? {
                let repo = repo?;
                let project = Project {
                    repo: format!("{owner_name}/{}", repo.file_name().to_string_lossy()),
                    dir: repo.path(),
                };
                if project.exists() {
                    projects.push(project);
                }
            }
        }
        projects.sort_by(|a, b| a.repo.cmp(&b.repo));
        Ok(projects)
    }

    pub fn find(data_dir: &Path, repo: &str) -> Result<Option<Project>> {
        Ok(Self::discover(data_dir)?
            .into_iter()
            .find(|p| p.repo.eq_ignore_ascii_case(repo)))
    }

    fn workdir(&self, dir: &Path, config: &Config, secrets: &Secrets) -> Workdir {
        Workdir::new(dir)
            .env("GITHUB_REPOSITORY", &self.repo)
            .env("GITHUB_API_URL", &config.api_url)
            .env("GITHUB_TOKEN", &secrets.token)
            .env("GH_TOKEN", &secrets.token)
            .env("BD_NON_INTERACTIVE", "1")
            .env("BD_NO_DEP_TYPE_WARNING", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env(
                "GIT_CONFIG_VALUE_0",
                "!f() { echo username=x-access-token; echo \"password=$GITHUB_TOKEN\"; }; f",
            )
            .env("GIT_CONFIG_KEY_1", "user.name")
            .env("GIT_CONFIG_VALUE_1", "bd-gh-sync")
            .env("GIT_CONFIG_KEY_2", "user.email")
            .env("GIT_CONFIG_VALUE_2", "bd-gh-sync@users.noreply.github.com")
    }

    pub async fn clone_from(&self, config: &Config, secrets: &Secrets) -> Result<()> {
        let parent = self.dir.parent().expect("projects live under the data dir");
        tokio::fs::create_dir_all(parent).await?;
        let url = format!(
            "{}/{}.git",
            config.git_base.trim_end_matches('/'),
            self.repo
        );
        self.workdir(parent, config, secrets)
            .run("git", &["clone", "-q", &url, &self.dir.to_string_lossy()])
            .await
            .map(drop)
    }

    fn head_file(&self) -> PathBuf {
        self.dir.join(".git/bd-gh-sync/dolt-head")
    }

    pub fn processed_head(&self) -> Option<String> {
        let head = std::fs::read_to_string(self.head_file()).ok()?;
        Some(head.trim().to_string()).filter(|h| !h.is_empty())
    }

    pub async fn remote_head(&self, config: &Config, secrets: &Secrets) -> Result<Option<String>> {
        let wd = self.workdir(&self.dir, config, secrets);
        let out = wd
            .run("git", &["ls-remote", "origin", "refs/dolt/data"])
            .await?;
        Ok(out.split_whitespace().next().map(str::to_string))
    }

    pub async fn push(&self, config: &Config, secrets: &Secrets) -> Result<()> {
        let span = info_span!("push", project = %self.repo);
        async {
            let head = self.remote_head(config, secrets).await?;
            let wd = self.workdir(&self.dir, config, secrets);
            Bd::new(&wd).dolt_pull().await?;
            watch::push_beads(wd, GitHub::new(&config.api_url, &secrets.token)).await?;
            if let Some(head) = head {
                let file = self.head_file();
                std::fs::create_dir_all(file.parent().expect("head file has a parent"))?;
                std::fs::write(file, head)?;
            }
            Ok(())
        }
        .instrument(span)
        .await
    }

    pub async fn sync(&self, config: &Config, secrets: &Secrets, mode: Mode) -> Result<()> {
        let span = info_span!("sync", project = %self.repo);
        async {
            let wd = self.workdir(&self.dir, config, secrets);
            match wd.run("git", &["fetch", "-q", "origin"]).await {
                Ok(_) => {
                    if let Err(e) = wd.run("git", &["reset", "-q", "--hard", "@{u}"]).await {
                        warn!("could not reset the clone to its upstream: {e:#}");
                    }
                }
                Err(e) => warn!("git fetch failed: {e:#}"),
            }
            restrict_beads_dir(&self.dir);
            info!("syncing {}", describe(&mode));
            let opts = Options {
                repo: self.repo.clone(),
                mode,
                publish: true,
                transport: config.transport,
                jsonl: ".beads/issues.jsonl".into(),
                adopt_bd_created: false,
                commit_message: "beads: sync from GitHub".into(),
            };
            sync::run(&wd, &GitHub::new(&config.api_url, &secrets.token), &opts).await?;
            let mut project_changed = false;
            for project_config in config
                .project_sync
                .iter()
                .filter(|entry| entry.repo.eq_ignore_ascii_case(&self.repo))
            {
                let token = secrets
                    .projects_token
                    .as_deref()
                    .context("GITHUB_PROJECTS_TOKEN is required when project_sync is configured")?;
                project_changed |= crate::server::projects::sync(
                    &wd,
                    &GitHub::new(&config.api_url, token),
                    project_config,
                )
                .await?;
            }
            if project_changed {
                crate::server::projects::publish(&wd, config).await?;
            }
            Ok(())
        }
        .instrument(span)
        .await
    }
}

fn restrict_beads_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let beads = dir.join(".beads");
    if beads.is_dir() {
        let _ = std::fs::set_permissions(&beads, std::fs::Permissions::from_mode(0o700));
    }
}
