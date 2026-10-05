mod bd;
mod github;
mod ids;
mod server;
mod sync;
mod watch;

use std::{path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::{
    bd::Workdir,
    github::GitHub,
    server::config::Config,
    sync::{Mode, Options, Transport},
};

#[derive(Parser)]
#[command(
    version,
    about = "Real-time two-way sync between beads (bd) and GitHub issues"
)]
struct Cli {
    #[arg(
        short = 'C',
        global = true,
        help = "Run as if started in this directory"
    )]
    directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Pull GitHub issue changes into the beads of the repository here")]
    Sync(SyncArgs),
    #[command(about = "Push local bead changes to GitHub issues as they happen")]
    Watch(WatchArgs),
    #[command(about = "Compare every linked bead with its GitHub issue and print the drift")]
    Verify {
        #[arg(
            long,
            env = "GITHUB_REPOSITORY",
            help = "owner/name of the GitHub repository"
        )]
        repo: String,
    },
    #[command(about = "Run the webhook server that syncs many repositories")]
    Server {
        #[arg(
            long,
            default_value = "/etc/bd-gh-sync/config.toml",
            help = "Server config file"
        )]
        config: PathBuf,
        #[command(subcommand)]
        command: ServerCommand,
    },
}

#[derive(Args)]
struct SyncArgs {
    #[arg(help = "GitHub issue numbers to pull")]
    issues: Vec<u64>,
    #[arg(long, conflicts_with_all = ["issues", "all"], help = "Pull every issue updated since the last published sync")]
    since_last: bool,
    #[arg(long, conflicts_with = "issues", help = "Reconcile every issue")]
    all: bool,
    #[arg(
        long,
        help = "Publish the result (Dolt push or JSONL commit), retrying races"
    )]
    publish: bool,
    #[arg(
        long,
        env = "GITHUB_REPOSITORY",
        help = "owner/name of the GitHub repository"
    )]
    repo: String,
    #[arg(long, env = "BEADS_TRANSPORT", value_enum, default_value = "auto")]
    transport: Transport,
    #[arg(
        long,
        env = "BEADS_JSONL",
        default_value = ".beads/issues.jsonl",
        help = "Export path for the jsonl transport"
    )]
    jsonl: PathBuf,
    #[arg(
        long,
        env = "ADOPT_BD_CREATED",
        help = "Also import bd-created issues whose bead is not published"
    )]
    adopt_bd_created: bool,
    #[arg(
        long,
        env = "COMMIT_MESSAGE",
        default_value = "beads: sync from GitHub",
        help = "Commit message for the jsonl transport"
    )]
    commit_message: String,
}

#[derive(Args)]
struct WatchArgs {
    #[arg(long, help = "Run a single sync pass and exit")]
    once: bool,
    #[arg(
        long,
        help = "On the first run (no saved state), push every bead instead of only later changes"
    )]
    initial_push: bool,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 30.0,
        help = "Safety-net / fallback poll interval"
    )]
    poll: f64,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 1.0,
        help = "Quiet period that ends a burst of writes"
    )]
    debounce: f64,
    #[arg(
        long,
        value_enum,
        default_value = "native",
        help = "How to notice bead writes"
    )]
    backend: watch::Backend,
    #[arg(
        long,
        value_name = "SECS",
        default_value_t = 0,
        help = "Also `bd dolt pull` every SECS and `bd dolt push` after each GitHub push (0: off)"
    )]
    dolt_sync: u64,
    #[arg(
        long,
        help = "Show what would be pushed; push nothing and save no state"
    )]
    dry_run: bool,
}

#[derive(Subcommand)]
enum ServerCommand {
    #[command(about = "Receive webhooks and keep every project in sync")]
    Serve,
    #[command(about = "Start syncing a repository: clone it, sync once, register the webhook")]
    Add {
        #[arg(help = "owner/name")]
        repo: String,
        #[arg(long, help = "Don't create or update the repository webhook")]
        no_webhook: bool,
    },
    #[command(about = "Sync one project now (while the server is stopped, or to debug)")]
    Sync {
        #[arg(help = "owner/name")]
        repo: String,
        #[arg(help = "Issue numbers to pull [default: everything updated since the last sync]")]
        issues: Vec<u64>,
        #[arg(long, conflicts_with = "issues", help = "Reconcile every issue")]
        all: bool,
    },
    #[command(about = "List the projects being synced")]
    List,
}

fn mode(issues: Vec<u64>, since_last: bool, all: bool) -> Mode {
    match (all, since_last, issues.is_empty()) {
        (true, _, _) => Mode::All,
        (_, true, _) | (_, _, true) => Mode::SinceLast,
        _ => Mode::Issues(issues),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    let cli = Cli::parse();
    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir)?;
    }
    match cli.command {
        Command::Sync(args) => {
            if args.issues.is_empty() && !args.since_last && !args.all {
                bail!("nothing to sync: pass issue numbers, --since-last or --all");
            }
            let opts = Options {
                repo: args.repo,
                mode: mode(args.issues, args.since_last, args.all),
                publish: args.publish,
                transport: args.transport,
                jsonl: args.jsonl,
                adopt_bd_created: args.adopt_bd_created,
                commit_message: args.commit_message,
            };
            let wd = Workdir::new(std::env::current_dir()?)
                .env("GITHUB_REPOSITORY", &opts.repo)
                .env("BD_NON_INTERACTIVE", "1")
                .env("BD_NO_DEP_TYPE_WARNING", "1");
            sync::run(&wd, &GitHub::from_env()?, &opts).await
        }
        Command::Verify { repo } => {
            let wd = Workdir::new(std::env::current_dir()?)
                .env("BD_NON_INTERACTIVE", "1")
                .env("BD_NO_DEP_TYPE_WARNING", "1");
            let drift = sync::verify(&wd, &GitHub::from_env()?, &repo).await?;
            if drift > 0 {
                bail!("{drift} difference(s) between beads and GitHub");
            }
            println!("beads and GitHub agree");
            Ok(())
        }
        Command::Watch(args) => {
            watch::run(watch::Options {
                once: args.once,
                initial_push: args.initial_push,
                poll: Duration::from_secs_f64(args.poll),
                debounce: Duration::from_secs_f64(args.debounce),
                backend: args.backend,
                dolt_sync: Duration::from_secs(args.dolt_sync),
                dry_run: args.dry_run,
            })
            .await
        }
        Command::Server { config, command } => {
            let config = Config::load(&config)?;
            match command {
                ServerCommand::Serve => server::serve(config).await,
                ServerCommand::Add { repo, no_webhook } => {
                    server::add(config, &repo, no_webhook).await
                }
                ServerCommand::Sync { repo, issues, all } => {
                    server::sync(config, &repo, mode(issues, false, all)).await
                }
                ServerCommand::List => server::list(&config),
            }
        }
    }
}
