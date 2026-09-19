//! Operations shared by the `dotgit` CLI and the `dotgit-tui` front end.
//!
//! Nothing in here prints. Each operation returns a description of what it did
//! so the caller can render it as a line of terminal output or as a pane in a
//! TUI, which is what keeps one implementation behind both front ends.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use git2::Repository;

use crate::error::DotgitError;
use crate::{fsops, gh, git, history};

/// What an upload copied and staged.
#[derive(Debug, Default)]
pub struct UploadReport {
    /// Repository-relative paths that were staged.
    pub staged: Vec<PathBuf>,
    pub stats: fsops::CopyStats,
}

/// Copy each path into the repository and stage it.
pub fn upload(repo: &Repository, paths: &[PathBuf]) -> Result<UploadReport> {
    let root = git::repo_workdir(repo)?;
    let mut report = UploadReport::default();

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
        report.stats.copied += stats.copied;
        report.stats.skipped += stats.skipped;
        report.stats.bytes += stats.bytes;
        report.stats.broken_links.extend(stats.broken_links);

        // Stage paths relative to the repository root so the command behaves
        // the same from any directory inside the repo.
        let rel = dest
            .strip_prefix(&root)
            .map_err(|_| DotgitError::from("uploaded path escaped the repository"))?;
        report.staged.push(rel.to_path_buf());
    }

    git::force_add(repo, &report.staged)?;
    Ok(report)
}

/// Where a push went, and as whom.
#[derive(Clone, Debug)]
pub enum PushReport {
    /// Pushed over SSH, where the key agent decides the identity.
    Ssh {
        host: String,
    },
    Http {
        host: String,
        username: String,
    },
    /// A path-like remote, which needs no credentials at all.
    Local {
        target: String,
    },
}

/// Push the current branch to `origin`, choosing credentials from the remote.
///
/// This can hand the terminal to `gh`/`glab` for an interactive login, so a
/// full-screen caller must leave its alternate screen first.
pub fn push(repo: &Repository, force: bool) -> Result<PushReport> {
    let url = git::remote_url(repo)?;
    let branch = git::current_branch(repo)?;

    if git::remote_is_ssh(&url) {
        git::push_ssh(repo, &branch, force)?;
        return Ok(PushReport::Ssh {
            host: git::host_of(&url).unwrap_or(url),
        });
    }

    if git::remote_is_http(&url) {
        let host = git::host_of(&url)
            .ok_or_else(|| DotgitError::from(format!("cannot parse remote URL: {url}")))?;
        let credentials = host_credentials(&host)?;
        let username = credentials.username.clone();
        git::push(repo, &branch, Some(credentials), force)?;
        return Ok(PushReport::Http { host, username });
    }

    git::push(repo, &branch, None, force)?;
    // A local or bare-path remote has no host to name, so echo the URL itself.
    Ok(PushReport::Local {
        target: git::host_of(&url).unwrap_or(url),
    })
}

/// Find a token for `host`, logging in through that forge's CLI if there isn't
/// one yet. An unrecognised host gets whatever `gh` happens to hold for it,
/// since that is where a self-hosted GitHub Enterprise login would land.
pub fn host_credentials(host: &str) -> Result<gh::HostCredentials> {
    if let Some(forge) = gh::forge_for_host(host) {
        // An exported token wins over the CLIs: it is the documented escape
        // hatch for CI, where no interactive login is possible.
        if forge == gh::Forge::GitLab
            && let Ok(token) = env::var("GITLAB_TOKEN").or_else(|_| env::var("GL_TOKEN"))
        {
            return Ok(gh::HostCredentials {
                username: "oauth2".into(),
                token,
            });
        }
        gh::ensure_forge_auth(forge, host)?;
        return match forge {
            gh::Forge::GitHub => gh::read_host_credentials(host),
            gh::Forge::GitLab => gh::read_glab_credentials(host),
        };
    }
    gh::read_host_credentials(host)
}

/// The result of stepping through the history.
#[derive(Clone, Debug)]
pub enum StepReport {
    /// Already at the end being stepped towards; nothing changed.
    Boundary,
    Moved(history::Position),
}

/// Move one version older or newer and rewrite the working tree to match.
pub fn step(repo: &Repository, direction: history::Direction) -> Result<StepReport> {
    let current = history::current(repo)?;

    // Guard against silently discarding edits. The comparison is against the
    // version currently checked out, so a step never blocks the next step.
    if git::has_changes_against(repo, current.oid)? {
        return Err(DotgitError::from(
            "you have uncommitted changes - run `dotgit commit` to keep them, \
             or `dotgit backup` first if you are unsure",
        )
        .into());
    }

    let next = match history::step(repo, direction)? {
        Some(next) => next,
        None => return Ok(StepReport::Boundary),
    };

    git::checkout_tree_at(repo, next.oid)?;
    history::save(repo, &next)?;
    Ok(StepReport::Moved(next))
}

/// Move the working tree to the version at `index` in the history, where 0 is
/// the newest. Stepping is the one-at-a-time case of this; a front end showing
/// the whole list can jump straight to the version the user picked.
pub fn jump_to(repo: &Repository, index: usize) -> Result<history::Position> {
    let chain = history::chain(repo)?;
    let oid = *chain
        .get(index)
        .ok_or_else(|| DotgitError::from("no such version in this history"))?;

    let current = history::current(repo)?;
    if git::has_changes_against(repo, current.oid)? {
        return Err(DotgitError::from(
            "you have uncommitted changes - commit or discard them before moving to another version",
        )
        .into());
    }

    let position = history::Position {
        oid,
        index,
        total: chain.len(),
    };
    git::checkout_tree_at(repo, oid)?;
    history::save(repo, &position)?;
    Ok(position)
}

/// A commit that `plan_drop` found, ready to be shown before anything happens.
#[derive(Clone, Debug)]
pub struct DoomedCommit {
    pub id: String,
    pub subject: String,
}

/// What destroying the newest commits would do, worked out before any of it
/// happens so the caller can show it and ask for confirmation first.
#[derive(Clone, Debug)]
pub struct DropPlan {
    /// The commits that would stop existing, newest first.
    pub doomed: Vec<DoomedCommit>,
    /// The commit the branch would end up on.
    pub target: git2::Oid,
    pub target_id: String,
    pub target_subject: String,
    /// Whether uncommitted work would be destroyed along with the commits.
    pub dirty: bool,
}

/// Work out which commits `count` would destroy, refusing the cases where
/// destroying them is either meaningless or dangerous.
pub fn plan_drop(repo: &Repository, count: usize) -> Result<DropPlan> {
    if count == 0 {
        return Err(DotgitError::from("nothing to destroy: a count of 0").into());
    }

    // Destroying commits while the working tree is parked on an older version
    // would be very hard to reason about, so insist on a normal starting point.
    let position = history::current(repo)?;
    if !position.is_newest() {
        return Err(DotgitError::from(
            "you are stepped back through the history - run `dotgit rebase` until you reach the newest change first",
        )
        .into());
    }

    let chain = history::chain(repo)?;
    if count >= chain.len() {
        return Err(DotgitError::from(format!(
            "cannot destroy {count} commit(s): the history is only {} commit(s) long, and the first commit cannot be destroyed this way",
            chain.len()
        ))
        .into());
    }

    let doomed = chain[..count]
        .iter()
        .map(|oid| git::describe(repo, *oid).map(|(id, subject)| DoomedCommit { id, subject }))
        .collect::<Result<Vec<_>>>()?;
    let target = chain[count];
    let (target_id, target_subject) = git::describe(repo, target)?;

    Ok(DropPlan {
        doomed,
        target,
        target_id,
        target_subject,
        dirty: git::has_changes_against(repo, chain[0])?,
    })
}

/// What destroying the commits actually did.
#[derive(Debug, Default)]
pub struct DropReport {
    /// Where the safety backup went, unless one was not asked for.
    pub backup: Option<PathBuf>,
    pub removed: usize,
    pub head_id: String,
    pub head_subject: String,
    pub pushed: Option<PushReport>,
    /// A push that failed after the commits were already destroyed locally.
    /// The destruction is not undone for it, so the caller reports it as a
    /// warning and the user can push again once the cause is fixed.
    pub push_error: Option<String>,
}

/// Destroy the planned commits: back up first, then move the branch back, then
/// force-push so the remote loses them too.
pub fn drop_commits(
    repo: &Repository,
    plan: &DropPlan,
    backup_first: bool,
    push_after: bool,
) -> Result<DropReport> {
    let mut report = DropReport {
        removed: plan.doomed.len(),
        ..Default::default()
    };

    // The bundle is written before anything is destroyed, so the commits
    // remain recoverable even after git eventually collects them.
    if backup_first {
        report.backup = Some(crate::backup::create(repo, None)?.path);
    }

    git::reset_hard(repo, plan.target)?;
    // Whatever version cursor existed described commits that may no longer be
    // there, so it cannot be trusted afterwards.
    history::clear(repo)?;

    report.head_id = plan.target_id.clone();
    report.head_subject = plan.target_subject.clone();

    if push_after && git::remote_url(repo).is_ok() {
        match push(repo, true) {
            Ok(pushed) => report.pushed = Some(pushed),
            Err(err) => report.push_error = Some(format!("{err:#}")),
        }
    }
    Ok(report)
}

/// Map a path on this machine to its place inside the repository: anything
/// under `$HOME` keeps its position relative to home, anything else keeps its
/// layout from the filesystem root.
pub fn destination(root: &Path, abs: &Path) -> Result<PathBuf> {
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

/// Expand a leading `~` and resolve relative paths against the current
/// directory, so every later step works with an absolute path.
pub fn absolutize(path: &Path) -> Result<PathBuf> {
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

pub fn human_bytes(bytes: u64) -> String {
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
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
