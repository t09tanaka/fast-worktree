//! fast-worktree: create a git worktree with its npm dependencies in
//! seconds, and remove it in one.

mod clone;
mod cmd;
mod ecosystem;
mod error;
mod git;
mod hooks;
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
        /// Only these project directories
        projects: Vec<String>,
    },
    /// List projects whose dependencies are missing (always exits 0)
    Check {
        /// Print nothing when nothing is missing
        #[arg(long)]
        quiet: bool,
    },
    /// Remove a clean worktree, deleting its files in the background
    Remove {
        path: PathBuf,
        /// Also delete its branch with `git branch -d`
        #[arg(long)]
        delete_branch: bool,
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
        Cmd::Install { root, projects } => cmd::install::run(cmd::install::Opts { root, projects }),
        Cmd::Check { quiet } => Ok(cmd::check::run(quiet)),
        Cmd::Remove {
            path,
            delete_branch,
        } => cmd::remove::run(cmd::remove::Opts {
            path,
            delete_branch,
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
