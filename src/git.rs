use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use git2::{Cred, PushOptions, RemoteCallbacks, Repository, Status, StatusOptions};

use crate::error::DotgitError;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".").map_err(|_| anyhow::Error::new(DotgitError::NotARepository))
}

pub fn force_add(repo: &Repository, rel: &Path) -> Result<()> {
    let mut index = repo.index()?;
    add_path_recursive(&mut index, rel)?;
    index.write()?;
    Ok(())
}

fn add_path_recursive(index: &mut git2::Index, rel: &Path) -> Result<()> {
    if rel.is_dir() {
        for entry in std::fs::read_dir(rel)? {
            add_path_recursive(index, &entry?.path())?;
        }
    } else {
        index.add_path(rel)?;
    }
    Ok(())
}

pub fn stage_all(repo: &Repository) -> Result<()> {
    let mut index = repo.index()?;
    let statuses = repo.statuses(Some(&mut stage_options()))?;
    for entry in statuses.iter() {
        let path = Path::new(entry.path().unwrap_or_default());
        let status = entry.status();
        if status
            .intersects(Status::WT_DELETED | Status::INDEX_DELETED)
            || !path.exists()
        {
            index.remove_path(path)?;
        } else {
            index.add_path(path)?;
        }
    }
    index.write()?;
    Ok(())
}

pub fn create_commit(repo: &Repository, message: &str) -> Result<bool> {
    stage_all(repo)?;
    let mut index = repo.index()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let head_tree = parent_commit
        .as_ref()
        .and_then(|c| c.tree().ok());

    let staged = match (&parent_commit, &head_tree) {
        (Some(_), Some(head_tree)) => {
            let diff = repo.diff_tree_to_tree(Some(head_tree), Some(&tree), None)?;
            diff.deltas().next().is_some()
        }
        _ => true,
    };
    if !staged {
        return Ok(false);
    }

    let signature = repo
        .signature()
        .context("set git user.name and user.email to commit")?;
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    repo.commit(Some("HEAD"), &signature, &signature, message, &tree, &parents)?;
    Ok(true)
}

pub fn remote_url(repo: &Repository) -> Result<String> {
    let url = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url().map(str::to_string))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            repo.config()
                .ok()
                .and_then(|c| c.get_string("remote.origin.url").ok())
        });
    url.ok_or_else(|| DotgitError::NoRemote.into())
}

pub fn current_branch(repo: &Repository) -> Result<String> {
    let head = repo.head()?;
    if head.is_branch() {
        Ok(head.shorthand().unwrap_or("HEAD").to_string())
    } else {
        Err(DotgitError::DetachedHead.into())
    }
}

pub fn remote_is_ssh(url: &str) -> bool {
    url.starts_with("git@") || url.starts_with("ssh://")
}

pub fn remote_is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

pub fn host_of(url: &str) -> Option<String> {
    let rest = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else if let Some(at) = url.find('@') {
        &url[at + 1..]
    } else {
        url
    };
    rest.split('/')
        .next()
        .map(|h| h.split(':').next().unwrap_or_default().to_string())
        .filter(|h| !h.is_empty())
}

pub fn push(repo: &Repository, branch: &str, credentials: Option<crate::gh::HostCredentials>) -> Result<()> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let mut remote = repo.find_remote("origin")?;
    let mut options = PushOptions::new();
    if let Some(creds) = credentials {
        let mut callbacks = RemoteCallbacks::new();
        let username = creds.username;
        let token = creds.token;
        callbacks.credentials(move |_url, user_from_url, _allowed| {
            let user = user_from_url.unwrap_or(&username).to_string();
            Cred::userpass_plaintext(&user, &token)
        });
        options.remote_callbacks(callbacks);
    }
    remote.push(&[&refspec], Some(&mut options))?;
    Ok(())
}

pub fn push_ssh(repo: &Repository, branch: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo.path().parent().unwrap_or(Path::new(".")))
        .args(["push", "origin", branch])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| anyhow!("git push failed: {e}"))?;
    if !status.success() {
        return Err(anyhow!("git push failed"));
    }
    Ok(())
}

fn stage_options() -> StatusOptions {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .update_index(true);
    options
}

pub fn repo_workdir(repo: &Repository) -> Result<PathBuf> {
    repo.workdir()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("repository has no working directory"))
}