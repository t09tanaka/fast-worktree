//! fast-worktree: create a git worktree with its dependencies in seconds,
//! and remove it in one.

mod cache;
mod clone;
mod cmd;
mod ecosystem;
mod error;
mod git;
mod hooks;
mod include;
mod paths;
mod trash;
mod util;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "fast-worktree",
    version,
    about = "Create git worktrees with their dependencies in seconds, remove them in one"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a worktree on a new branch and install its dependencies
    Add {
        /// Where to create the worktree
        path: PathBuf,
        /// The new branch
        #[arg(short = 'b', long = "branch", value_name = "BRANCH")]
        branch: String,
        /// Commit to start from (default: origin's default branch, fetched)
        start_point: Option<String>,
    },
    /// Install dependencies into an existing worktree
    Install {
        /// Worktree to install into (default: the current one)
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Also install again where the lockfile or Node version changed
        /// since the last install
        #[arg(long)]
        changed: bool,
        /// Only these project directories
        projects: Vec<String>,
    },
    /// List projects whose dependencies are missing (always exits 0)
    Check {
        /// Print nothing when nothing is missing
        #[arg(long)]
        quiet: bool,
        /// Run as a post-checkout hook, given the hook's three arguments
        #[arg(
            long,
            num_args = 3,
            value_names = ["PREV", "NEW", "FLAG"],
            allow_hyphen_values = true
        )]
        post_checkout: Option<Vec<String>>,
    },
    /// Remove a clean worktree, deleting its files in the background
    Remove {
        path: PathBuf,
        /// Also delete its branch with `git branch -d`
        #[arg(long)]
        delete_branch: bool,
    },
    /// Inspect or clean the dependency cache
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    /// List cache entries with their repository, directory, key and last use
    Ls,
    /// Remove entries not used for a while (moved to the trash)
    Prune {
        /// Days without use (default: 7)
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Add {
            path,
            branch,
            start_point,
        } => cmd::add::run(cmd::add::Opts {
            path,
            branch,
            start_point,
        }),
        Cmd::Install {
            root,
            changed,
            projects,
        } => cmd::install::run(cmd::install::Opts {
            root,
            projects,
            changed,
        }),
        Cmd::Check {
            quiet,
            post_checkout,
        } => Ok(match post_checkout.as_deref() {
            Some([prev, new, flag]) => cmd::check::post_checkout(prev, new, flag),
            _ => cmd::check::run(quiet),
        }),
        Cmd::Remove {
            path,
            delete_branch,
        } => cmd::remove::run(cmd::remove::Opts {
            path,
            delete_branch,
        }),
        Cmd::Cache { cmd } => Ok(match cmd {
            CacheCmd::Ls => cmd::cache::ls(),
            CacheCmd::Prune { older_than } => cmd::cache::prune(older_than),
        }),
    };
    let code = match result {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fast-worktree: {e}");
            1
        }
    };
    std::process::exit(code);
}
