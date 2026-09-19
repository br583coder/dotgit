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

/// Commit exactly what is in the index, without staging anything first.
///
/// This is what a front end with a staging area needs: unstaging a file has to
/// mean the file is not committed. [`create_commit`] stages everything first,
/// which is right for `dotgit commit` on the command line and wrong here.
pub fn commit_staged(repo: &Repository, message: &str) -> Result<bool> {
    let mut index = repo.index()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let head_tree = parent_commit.as_ref().and_then(|c| c.tree().ok());
    let changed = match head_tree {
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
    index.write()?;
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
        // Leave nothing half-applied: put the working tree back and clear the
        // in-progress revert, so the repository is exactly as it was and no
        // other git tool reports it as mid-revert.
        let head = repo.head()?.peel_to_commit()?;
        reset_hard(repo, head.id())?;
        repo.cleanup_state()?;
        return Err(anyhow!(
            "reverting {revision} conflicts with later changes; nothing was changed - revert it with git if you want to resolve the conflict by hand"
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
    // `revert` marks the repository as reverting; without this every later
    // `git status` claims a revert is still in progress.
    repo.cleanup_state()?;
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

/// Push `branch` to `origin`. A forced push leads the refspec with `+`, which
/// is what lets the remote lose commits that the local branch no longer has.
pub fn push(
    repo: &Repository,
    branch: &str,
    credentials: Option<crate::gh::HostCredentials>,
    force: bool,
) -> Result<()> {
    let lead = if force { "+" } else { "" };
    let refspec = format!("{lead}refs/heads/{branch}:refs/heads/{branch}");
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

pub fn push_ssh(repo: &Repository, branch: &str, force: bool) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo.path().parent().unwrap_or(Path::new(".")));
    if force {
        // `--force-with-lease` still refuses if the remote moved in a way we
        // have not seen, which a bare `--force` would happily overwrite.
        command.args(["push", "--force-with-lease", "origin", branch]);
    } else {
        command.args(["push", "origin", branch]);
    }
    let status = command
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

/// Move the branch to `oid` and make the working tree match it, discarding
/// everything after it. The commits themselves stay in the object database
/// until git garbage-collects them, which is what `git reflog` recovers from.
pub fn reset_hard(repo: &Repository, oid: git2::Oid) -> Result<()> {
    let object = repo.find_object(oid, None)?;
    repo.reset(&object, git2::ResetType::Hard, None)?;
    Ok(())
}

/// One entry of the working tree's status, as a front end needs to show it.
#[derive(Clone, Debug)]
pub struct FileStatus {
    pub path: String,
    /// Two characters in git's own style: index state, then worktree state.
    pub label: String,
    pub staged: bool,
    pub unstaged: bool,
    pub untracked: bool,
}

/// Every changed path in the repository, staged or not, including untracked
/// files - a dotfiles repo tracks things `.gitignore` would normally hide, so
/// they have to be visible to be stageable.
pub fn status_entries(repo: &Repository) -> Result<Vec<FileStatus>> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let statuses = repo.statuses(Some(&mut options))?;

    let mut entries = Vec::with_capacity(statuses.len());
    for entry in statuses.iter() {
        let Some(path) = entry.path() else { continue };
        let status = entry.status();
        let index = if status.is_index_new() {
            'A'
        } else if status.is_index_modified() {
            'M'
        } else if status.is_index_deleted() {
            'D'
        } else if status.is_index_renamed() {
            'R'
        } else {
            ' '
        };
        let worktree = if status.is_wt_new() {
            '?'
        } else if status.is_wt_modified() {
            'M'
        } else if status.is_wt_deleted() {
            'D'
        } else {
            ' '
        };
        entries.push(FileStatus {
            path: path.to_string(),
            label: format!("{index}{worktree}"),
            staged: index != ' ',
            unstaged: worktree != ' ',
            untracked: status.is_wt_new() && !status.is_index_new(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

/// Every path the repository tracks, from the index.
///
/// The status list only holds what has changed, so after committing it is
/// empty. A front end that lets you open a file needs the committed ones too,
/// or the moment you commit a file you can no longer reach it.
pub fn tracked_files(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.index()?;
    let mut paths: Vec<String> = index
        .iter()
        .filter_map(|entry| String::from_utf8(entry.path).ok())
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Stage one path, coping with a deletion, which has nothing left to add.
pub fn stage_path(repo: &Repository, path: &str) -> Result<()> {
    let mut index = repo.index()?;
    let full = repo_workdir(repo)?.join(path);
    if full.exists() {
        // FORCE, like `dotgit upload`: a dotfiles repo holds what it is told to.
        index.add_all([path], IndexAddOption::FORCE, None)?;
    } else {
        index.remove_path(Path::new(path))?;
    }
    index.write()?;
    Ok(())
}

/// Unstage one path by putting HEAD's version of it back in the index.
pub fn unstage_path(repo: &Repository, path: &str) -> Result<()> {
    let head = repo.head()?.peel_to_commit()?;
    repo.reset_default(Some(head.as_object()), [path])?;
    Ok(())
}

/// Throw away the working-tree changes to one path. An untracked file has no
/// committed version to return to, so discarding it means deleting it.
pub fn discard_path(repo: &Repository, path: &str, untracked: bool) -> Result<()> {
    if untracked {
        let full = repo_workdir(repo)?.join(path);
        std::fs::remove_file(&full)
            .map_err(|e| anyhow!("cannot remove {}: {e}", full.display()))?;
        return Ok(());
    }
    let mut options = git2::build::CheckoutBuilder::new();
    options.force().path(path);
    repo.checkout_head(Some(&mut options))?;
    Ok(())
}

/// The patch for one path, comparing the working tree against HEAD so both
/// staged and unstaged changes to it are visible in one view.
pub fn diff_for_path(repo: &Repository, path: &str) -> Result<Vec<String>> {
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let mut options = git2::DiffOptions::new();
    options
        .pathspec(path)
        .include_untracked(true)
        .context_lines(3);
    let diff = repo.diff_tree_to_workdir_with_index(head_tree.as_ref(), Some(&mut options))?;
    patch_lines(&diff)
}

/// The patch a commit introduced, against its first parent.
pub fn diff_for_commit(repo: &Repository, oid: git2::Oid) -> Result<Vec<String>> {
    let commit = repo.find_commit(oid)?;
    let tree = commit.tree()?;
    // The first commit has no parent, so it is compared against nothing and
    // every line reads as an addition.
    let parent = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut options = git2::DiffOptions::new();
    options.context_lines(3);
    let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))?;
    patch_lines(&diff)
}

/// Render a diff as the lines of a unified patch, keeping the leading `+`/`-`
/// so a caller can colour them.
fn patch_lines(diff: &git2::Diff) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let content = String::from_utf8_lossy(line.content());
        let content = content.trim_end_matches(['\n', '\r']);
        lines.push(match line.origin() {
            origin @ ('+' | '-' | ' ') => format!("{origin}{content}"),
            _ => content.to_string(),
        });
        true
    })?;
    Ok(lines)
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
