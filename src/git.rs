use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use git2::{Cred, IndexAddOption, Oid, PushOptions, RemoteCallbacks, Repository};

use crate::error::DotgitError;

pub fn open_repo() -> Result<Repository> {
    Repository::discover(".").map_err(|_| anyhow::Error::new(DotgitError::NotARepository))
}

/// Stage uploaded paths, ignoring `.gitignore` - a dotfiles repo should hold
/// whatever you explicitly told it to hold.
///
/// All paths go through a single `add_all` so libgit2 walks the tree once and
/// the index is written once, however many paths were uploaded.
pub fn force_add(repo: &Repository, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut index = repo.index()?;
    index.add_all(paths, IndexAddOption::FORCE, None)?;
    index.write()?;
    Ok(())
}

/// Stage every change in the working tree, the way `git add -A` does:
/// `add_all` picks up new and modified files, `update_all` records deletions.
fn stage_all(index: &mut git2::Index) -> Result<()> {
    index.add_all(["*"], IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"], None)?;
    Ok(())
}

pub fn create_commit(repo: &Repository, message: &str) -> Result<bool> {
    let mut index = repo.index()?;
    stage_all(&mut index)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let head_tree = parent_commit.as_ref().and_then(|c| c.tree().ok());

    let changed = match head_tree {
        // Comparing tree ids is enough: equal trees mean an empty commit.
        Some(head_tree) => head_tree.id() != tree.id(),
        None => true,
    };
    if !changed {
        return Ok(false);
    }

    let signature = repo
        .signature()
        .context("set git user.name and user.email to commit")?;
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
    )?;
    Ok(true)
}

pub fn recent_commits(repo: &Repository, limit: usize) -> Result<Vec<(Oid, String)>> {
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    let mut commits = Vec::new();
    for oid in walk.take(limit) {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        commits.push((oid, commit.summary().unwrap_or("(no message)").to_string()));
    }
    Ok(commits)
}

pub fn revert_commit(repo: &Repository, revision: &str) -> Result<String> {
    let target = repo
        .revparse_single(revision)
        .with_context(|| format!("cannot find commit '{revision}'"))?
        .peel_to_commit()
        .with_context(|| format!("'{revision}' is not a commit"))?;
    let mut options = git2::RevertOptions::new();
    repo.revert(&target, Some(&mut options))?;

    let mut index = repo.index()?;
    if index.has_conflicts() {
        return Err(anyhow!(
            "reverting {revision} produced conflicts; resolve them and commit manually"
        ));
    }
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let head = repo.head()?.peel_to_commit()?;
    let signature = repo
        .signature()
        .context("set git user.name and git user.email to commit")?;
    let subject = target.summary().unwrap_or("commit");
    let message = format!("Revert \"{subject}\"");
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        &message,
        &tree,
        &[&head],
    )?;
    Ok(message)
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
    // Strip the scheme, then the authority, then any `user@` and `:port`, so
    // both `ssh://git@host:22/x` and `git@host:x` yield `host`.
    let rest = match url.find("://") {
        Some(idx) => &url[idx + 3..],
        None => url,
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let authority = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    authority
        .split(':')
        .next()
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

pub fn push(
    repo: &Repository,
    branch: &str,
    credentials: Option<crate::gh::HostCredentials>,
) -> Result<()> {
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

/// Create a repository at `path` (or adopt one already there) and point its
/// `origin` at `url`. New repositories start on `main`, matching the default
/// branch both gh and glab give a freshly created remote.
pub fn init_with_remote(path: &Path, url: &str) -> Result<()> {
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = Repository::init_opts(path, &options)
        .map_err(|e| anyhow!("cannot initialise {}: {e}", path.display()))?;
    set_origin(&repo, url)?;
    Ok(())
}

/// Point `origin` at `url`, replacing any existing remote of that name.
pub fn set_origin(repo: &Repository, url: &str) -> Result<()> {
    if repo.find_remote("origin").is_ok() {
        repo.remote_set_url("origin", url)?;
    } else {
        repo.remote("origin", url)?;
    }
    Ok(())
}

/// Rewrite the working tree (and the index) to match `oid`, leaving the branch
/// pointer alone.
///
/// Checking out the tree rather than the commit is what makes stepping through
/// versions non-destructive: HEAD keeps pointing at the newest commit, so the
/// difference shows up as ordinary changes that `dotgit commit` can record, and
/// no commit ever becomes unreachable.
pub fn checkout_tree_at(repo: &Repository, oid: git2::Oid) -> Result<()> {
    let tree = repo.find_commit(oid)?.tree()?;
    let mut options = git2::build::CheckoutBuilder::new();
    // `force` overwrites tracked files; untracked files are left where they
    // are, since they were never part of any version.
    options.force().update_index(true).remove_untracked(false);
    repo.checkout_tree(tree.as_object(), Some(&mut options))?;
    Ok(())
}

/// Whether the working tree differs from `oid`.
///
/// The comparison is against the version currently checked out, not HEAD:
/// after stepping back, differing from HEAD is the expected state, and only a
/// difference from the stepped-to version means unsaved work.
pub fn has_changes_against(repo: &Repository, oid: git2::Oid) -> Result<bool> {
    let tree = repo.find_commit(oid)?.tree()?;
    let diff = repo.diff_tree_to_workdir_with_index(Some(&tree), None)?;
    Ok(diff.deltas().len() > 0)
}

/// A commit's abbreviated id and subject line, for reporting where we landed.
pub fn describe(repo: &Repository, oid: git2::Oid) -> Result<(String, String)> {
    let commit = repo.find_commit(oid)?;
    let id = commit.id().to_string();
    let subject = commit
        .summary()
        .unwrap_or("(no message)")
        .trim()
        .to_string();
    Ok((id[..7.min(id.len())].to_string(), subject))
}

pub fn repo_workdir(repo: &Repository) -> Result<PathBuf> {
    repo.workdir()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("repository has no working directory"))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_hosts_from_remote_urls() {
        assert_eq!(
            host_of("https://github.com/me/dots.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("git@github.com:me/dots.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("ssh://git@gitlab.com:22/me/dots.git").as_deref(),
            Some("gitlab.com")
        );
        assert_eq!(
            host_of("http://git.example.com:8080/me/dots.git").as_deref(),
            Some("git.example.com")
        );
    }

    #[test]
    fn classifies_remote_transports() {
        assert!(remote_is_ssh("git@github.com:me/dots.git"));
        assert!(remote_is_ssh("ssh://git@github.com/me/dots.git"));
        assert!(!remote_is_ssh("https://github.com/me/dots.git"));

        assert!(remote_is_http("https://github.com/me/dots.git"));
        assert!(remote_is_http("http://example.com/dots.git"));
        assert!(!remote_is_http("/srv/git/dots.git"));
    }
}
