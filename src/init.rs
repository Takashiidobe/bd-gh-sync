use anyhow::{Context, Result, bail};

use crate::{
    bd::{Bd, Workdir},
    watch,
};

const SERVICE_UNIT: &str = include_str!("../deploy/watch/bd-gh-sync-watch@.service");

pub struct Options {
    pub repo: Option<String>,
    pub install_service: bool,
}

fn repo_from_remote(url: &str) -> Option<String> {
    let rest = url
        .trim_end_matches(".git")
        .rsplit_once("github.com")?
        .1
        .trim_start_matches([':', '/']);
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

pub async fn run(opts: Options) -> Result<()> {
    let (_, root) = watch::find_beads()?;
    let wd = Workdir::new(&root)
        .env("BD_NON_INTERACTIVE", "1")
        .env("BD_NO_DEP_TYPE_WARNING", "1");
    let bd = Bd::new(&wd);

    let repo = match opts.repo {
        Some(repo) => repo,
        None => match bd.config_get("github.repository").await? {
            Some(repo) => repo,
            None => {
                let out = wd.output("git", &["remote", "get-url", "origin"]).await?;
                repo_from_remote(out.stdout.trim()).context(
                    "cannot tell which GitHub repository this is; pass --repo owner/name",
                )?
            }
        },
    };
    crate::server::project::validate_repo(&repo)?;
    bd.run(&["config", "set", "github.repository", &repo])
        .await?;
    println!("ok  github.repository = {repo}");

    let remotes = bd.output(&["dolt", "remote", "list"]).await?;
    let listed: Vec<&str> = remotes
        .stdout
        .lines()
        .filter(|line| line.split_whitespace().count() == 2)
        .collect();
    if !remotes.success || listed.is_empty() {
        bail!(
            "no Dolt remote is configured, so beads cannot reach the server; add one with \
             `bd dolt remote add origin git+https://github.com/{repo}`"
        );
    }
    println!("ok  Dolt remote: {}", listed.join("; "));

    bd.dolt_pull()
        .await
        .context("pulling from the Dolt remote")?;
    bd.dolt_push().await.context("pushing to the Dolt remote")?;
    println!("ok  local beads and the Dolt remote are in sync");

    if opts.install_service {
        install_service(&wd, &root).await?;
    }
    println!(
        "\nThe server picks up pushes from refs/dolt/data. Run `bd-gh-sync watch` here to push \
         automatically{}; to skip the poll wait, set BD_GH_SYNC_POKE_URL and BD_GH_SYNC_POKE_SECRET.",
        if opts.install_service {
            " (installed as a user service)"
        } else {
            ""
        }
    );
    Ok(())
}

async fn install_service(wd: &Workdir, root: &std::path::Path) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("--install-service only supports systemd; see the README for launchd");
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    let dir = std::path::Path::new(&home).join(".config/systemd/user");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("bd-gh-sync-watch@.service"), SERVICE_UNIT)?;
    let path = root.to_string_lossy();
    let escaped = wd
        .run("systemd-escape", &["--path", &path])
        .await
        .context("running systemd-escape")?;
    let unit = format!("bd-gh-sync-watch@{}.service", escaped.trim());
    wd.run("systemctl", &["--user", "daemon-reload"]).await?;
    wd.run("systemctl", &["--user", "enable", "--now", &unit])
        .await?;
    println!("ok  started {unit}");
    Ok(())
}
