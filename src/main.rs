mod error;
mod fsops;
mod gh;
mod git;

use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use error::DotgitError;

#[derive(Parser)]
#[command(
    name = "dotgit",
    version,
    about = "dotfile manager backed by git + gh auth"
)]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Copy files from your machine into the current dotfiles repository
    Upload {
        /// Local paths to upload (e.g. ~/.config/hypr)
        paths: Vec<PathBuf>,
    },
    /// Stage, commit and push changes (prompts for a commit message)
    Commit {
        /// Commit message (skips the interactive prompt)
        #[arg(short = 'm')]
        message: Option<String>,
        /// Commit locally without pushing
        #[arg(long)]
        no_push: bool,
    },
    /// Log into a host via gh and configure git to use it as credential helper
    Login {
        /// Host to log into (default: github.com)
        host: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        CliCommand::Upload { paths } => upload(&paths),
        CliCommand::Commit { message, no_push } => commit(message, no_push),
        CliCommand::Login { host } => login(host.as_deref()),
    };
    if let Err(err) = result {
        eprintln!("dotgit: {err:#}");
        std::process::exit(1);
    }
}

fn upload(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Err(DotgitError::from("usage: dotgit upload <path> [path ...]").into());
    }
    let repo = git::open_repo()?;
    let root = git::repo_workdir(&repo)?;
    let mut staged = Vec::with_capacity(paths.len());
    let mut totals = fsops::CopyStats::default();

    for path in paths {
        let abs = absolutize(path)?;
        if !abs.exists() {
            return Err(anyhow!("{}: no such file or directory", path.display()));
        }
        let dest = destination(&root, &abs)?;
        if dest == root {
            return Err(anyhow!("{}: this is the repository itself", path.display()));
        }
        if dest.exists() && dest.is_dir() && abs.is_file() {
            return Err(anyhow!(
                "{}: destination {} already exists as a directory",
                path.display(),
                dest.display()
            ));
        }
        let stats = fsops::copy_tree(&abs, &dest)?;
        totals.copied += stats.copied;
        totals.skipped += stats.skipped;
        totals.bytes += stats.bytes;
        for link in &stats.broken_links {
            eprintln!("dotgit: skipped broken symlink {}", link.display());
        }

        // Stage paths relative to the repository root so the command behaves
        // the same from any directory inside the repo.
        let rel = dest
            .strip_prefix(&root)
            .map_err(|_| DotgitError::from("uploaded path escaped the repository"))?;
        staged.push(rel.to_path_buf());
        println!("uploaded {}", rel.display());
    }

    git::force_add(&repo, &staged)?;
    println!(
        "staged for commit ({} copied, {} unchanged, {})",
        totals.copied,
        totals.skipped,
        human_bytes(totals.bytes)
    );
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn commit(message: Option<String>, no_push: bool) -> Result<()> {
    let repo = git::open_repo()?;
    let message = match message {
        Some(m) => m,
        None => prompt_commit_message()?,
    };
    if message.trim().is_empty() {
        return Err(anyhow!("commit message cannot be empty"));
    }
    match git::create_commit(&repo, &message)? {
        true => {
            let head = repo.head()?;
            let commit = head.peel_to_commit()?;
            let id = commit.id().to_string();
            println!(
                "committed {} on {}",
                &id[..7.min(id.len())],
                head.shorthand().unwrap_or("HEAD")
            );
        }
        false => println!("nothing to commit, working tree clean"),
    }
    if no_push {
        println!("skipped push (--no-push)");
        return Ok(());
    }
    push(&repo)?;
    Ok(())
}

fn push(repo: &git2::Repository) -> Result<()> {
    let url = git::remote_url(repo)?;
    let branch = git::current_branch(repo)?;

    if git::remote_is_ssh(&url) {
        git::push_ssh(repo, &branch)?;
        println!(
            "pushed to {}",
            git::host_of(&url).unwrap_or_else(|| url.clone())
        );
        return Ok(());
    }

    if git::remote_is_http(&url) {
        let host = git::host_of(&url)
            .ok_or_else(|| DotgitError::from(format!("cannot parse remote URL: {url}")))?;
        let credentials = if is_gitlab(&host) {
            let token = match env::var("GITLAB_TOKEN").or_else(|_| env::var("GL_TOKEN")) {
                Ok(token) => token,
                Err(_) => gh::read_glab_credentials()?.token,
            };
            gh::HostCredentials {
                username: "oauth2".into(),
                token,
            }
        } else {
            if is_github(&host) {
                gh::ensure_gh()?;
                if !gh::is_logged_in(&host) {
                    gh::login(&host)?;
                }
            }
            gh::read_host_credentials(&host)?
        };
        let username = credentials.username.clone();
        git::push(repo, &branch, Some(credentials))?;
        println!("pushed to {host} as {username}");
        return Ok(());
    }

    git::push(repo, &branch, None)?;
    // A local or bare-path remote has no host to name, so echo the URL itself.
    println!("pushed to {}", git::host_of(&url).unwrap_or(url));
    Ok(())
}

/// Match the host label, not any substring: `gitlab.example.com` is GitLab but
/// `my-gitlab-mirror.example.com` should not be assumed to be.
fn is_gitlab(host: &str) -> bool {
    host_labels(host).any(|label| label == "gitlab")
}

fn is_github(host: &str) -> bool {
    host_labels(host).any(|label| label == "github")
}

fn host_labels(host: &str) -> impl Iterator<Item = &str> {
    host.split('.')
}

fn login(host: Option<&str>) -> Result<()> {
    let host = host.unwrap_or("github.com");
    gh::ensure_gh()?;
    gh::login(host)?;
    println!("logged in as gh user; dotgit will push as this account");
    Ok(())
}

fn destination(root: &Path, abs: &Path) -> Result<PathBuf> {
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rel) = abs.strip_prefix(&home) {
            return Ok(root.join(rel));
        }
    }
    abs.strip_prefix("/")
        .map(|rel| root.join(rel))
        .map_err(|_| {
            DotgitError::from(format!("cannot map {} into the repository", abs.display())).into()
        })
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    let expanded = match path.to_str() {
        Some(s) if s == "~" || s.starts_with("~/") => {
            let home = env::var("HOME").map_err(|_| anyhow!("cannot resolve ~ (no $HOME)"))?;
            let rest = s.strip_prefix("~/").unwrap_or("");
            Path::new(home.as_str()).join(rest)
        }
        _ => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(env::current_dir()?.join(expanded))
    }
}

fn prompt_commit_message() -> Result<String> {
    println!("Commit message (Ctrl-D to finish):");
    io::stdout().flush()?;
    let mut lines = Vec::new();
    for line in io::stdin().lock().lines() {
        lines.push(line?);
    }
    Ok(lines.join("\n"))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_home_paths_relative_to_the_repository() {
        let root = Path::new("/repo");
        let home = env::var("HOME").expect("HOME must be set for this test");
        let abs = PathBuf::from(&home).join(".config/hypr");
        assert_eq!(
            destination(root, &abs).unwrap(),
            PathBuf::from("/repo/.config/hypr")
        );
    }

    #[test]
    fn maps_non_home_paths_from_the_filesystem_root() {
        let root = Path::new("/repo");
        assert_eq!(
            destination(root, Path::new("/etc/hosts")).unwrap(),
            PathBuf::from("/repo/etc/hosts")
        );
    }

    #[test]
    fn expands_tilde_paths() {
        let home = PathBuf::from(env::var("HOME").unwrap());
        assert_eq!(
            absolutize(Path::new("~/.zshrc")).unwrap(),
            home.join(".zshrc")
        );
        // A bare `~` is the home directory itself, not `$HOME/~`.
        assert_eq!(absolutize(Path::new("~")).unwrap(), home);
    }

    #[test]
    fn leaves_absolute_paths_alone_and_resolves_relative_ones() {
        assert_eq!(
            absolutize(Path::new("/etc/hosts")).unwrap(),
            PathBuf::from("/etc/hosts")
        );
        let cwd = env::current_dir().unwrap();
        assert_eq!(absolutize(Path::new("rel")).unwrap(), cwd.join("rel"));
    }

    #[test]
    fn does_not_expand_a_tilde_in_the_middle_of_a_path() {
        let cwd = env::current_dir().unwrap();
        assert_eq!(absolutize(Path::new("a/~/b")).unwrap(), cwd.join("a/~/b"));
    }

    #[test]
    fn recognises_hosts_by_label_not_substring() {
        assert!(is_github("github.com"));
        assert!(is_gitlab("gitlab.com"));
        assert!(is_gitlab("gitlab.example.com"));
        // A host that merely contains the name must not be misrouted.
        assert!(!is_gitlab("my-gitlab-mirror.example.com"));
        assert!(!is_github("github-enterprise-proxy.example.com"));
    }

    #[test]
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
