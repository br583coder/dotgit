mod error;
mod gh;
mod git;

use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use error::DotgitError;

#[derive(Parser)]
#[command(name = "dotgit", version, about = "dotfile manager backed by git + gh auth")]
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
        copy_into(&abs, &dest)?;
        let rel = dest
            .strip_prefix(&root)
            .map_err(|_| DotgitError::from("uploaded path escaped the repository"))?;
        git::force_add(&repo, rel)?;
        println!("uploaded {}", dest.display());
    }
    println!("staged for commit");
    Ok(())
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
        true => println!("committed as {}", repo.head()?.shorthand().unwrap_or("HEAD")),
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
        println!("pushed to {}", git::host_of(&url).unwrap_or_default());
        return Ok(());
    }

    if git::remote_is_http(&url) {
        let host = git::host_of(&url)
            .ok_or_else(|| DotgitError::from(format!("cannot parse remote URL: {url}")))?;
        let credentials = if host.contains("gitlab") {
            let token = match env::var("GITLAB_TOKEN").or_else(|_| env::var("GL_TOKEN")) {
                Ok(t) => t,
                Err(_) => {
                    let glab = gh::read_glab_credentials()?;
                    glab.token
                }
            };
            gh::HostCredentials {
                username: "oauth2".into(),
                token,
            }
        } else {
            if host.contains("github") {
                gh::ensure_gh()?;
                if !gh::is_logged_in(&host) {
                    gh::login(&host)?;
                }
            }
            gh::read_host_credentials(&host)?
        };
        git::push(repo, &branch, Some(credentials))?;
        println!("pushed to {host} as {}", git_username_for(&url)?);
        return Ok(());
    }

    git::push(repo, &branch, None)?;
    println!("pushed to {}", git::host_of(&url).unwrap_or_default());
    Ok(())
}

fn git_username_for(url: &str) -> Result<String> {
    let host = git::host_of(url).unwrap_or_default();
    Ok(gh::read_host_credentials(&host).map(|c| c.username).unwrap_or_else(|_| "default".into()))
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
        .map_err(|_| DotgitError::from(format!("cannot map {} into the repository", abs.display())).into())
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    let expanded = match path.to_str() {
        Some(s) if s == "~" || s.starts_with("~/") => {
            let home = env::var("HOME").map_err(|_| anyhow!("cannot resolve ~ (no $HOME)"))?;
            let rest = s
                .strip_prefix('~')
                .unwrap_or(s)
                .strip_prefix('/')
                .unwrap_or(s);
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

fn copy_into(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::metadata(src)?;
    if meta.is_dir() {
        if !dst.exists() {
            fs::create_dir_all(dst)?;
        }
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            copy_into(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(src)?.file_type().is_symlink() {
            let real = fs::canonicalize(src)?;
            fs::copy(&real, dst)?;
        } else {
            fs::copy(src, dst)?;
        }
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
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