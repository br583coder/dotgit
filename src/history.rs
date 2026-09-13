//! A cursor that walks the repository's commits one version at a time.
//!
//! `dotgit restore` steps towards older commits and `dotgit rebase` steps back
//! towards newer ones. Neither rewrites history: stepping only rewrites the
//! working tree, leaving the branch where it is, so every version remains
//! reachable and a step in the wrong direction costs nothing.
//!
//! The cursor lives in `.git/dotgit-position` - inside the git directory, so
//! it is never uploaded, committed, or pushed.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use git2::{Oid, Repository};

use crate::error::DotgitError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Towards the first commit.
    Older,
    /// Towards the newest commit.
    Newer,
}

/// Where the cursor sits, as an index into the first-parent chain: 0 is the
/// newest commit and `total - 1` the oldest.
#[derive(Clone, Debug)]
pub struct Position {
    pub oid: Oid,
    pub index: usize,
    pub total: usize,
}

impl Position {
    pub fn is_newest(&self) -> bool {
        self.index == 0
    }
}

fn position_file(repo: &Repository) -> PathBuf {
    repo.path().join("dotgit-position")
}

/// Every commit from HEAD back to the first, newest first.
///
/// Only first parents are followed, so a merge counts as the single version it
/// looks like from the branch, rather than fanning out into the side history.
pub fn chain(repo: &Repository) -> Result<Vec<Oid>> {
    let head = repo
        .head()
        .map_err(|_| DotgitError::from("no commits yet - nothing to step through"))?;
    let mut commit = head.peel_to_commit()?;
    let mut chain = vec![commit.id()];
    while let Ok(parent) = commit.parent(0) {
        chain.push(parent.id());
        commit = parent;
    }
    Ok(chain)
}

/// Where the cursor is now, defaulting to the newest commit.
///
/// A stored position that is no longer on the chain - the branch was reset, or
/// the commit was pruned - is treated as no position at all rather than an
/// error, since the newest commit is always a correct place to resume from.
pub fn current(repo: &Repository) -> Result<Position> {
    let chain = chain(repo)?;
    let index = fs::read_to_string(position_file(repo))
        .ok()
        .and_then(|stored| Oid::from_str(stored.trim()).ok())
        .and_then(|oid| chain.iter().position(|c| *c == oid))
        .unwrap_or(0);
    Ok(Position {
        oid: chain[index],
        index,
        total: chain.len(),
    })
}

/// The position one step in `direction`, or `None` at either end.
pub fn step(repo: &Repository, direction: Direction) -> Result<Option<Position>> {
    let chain = chain(repo)?;
    let current = current(repo)?;
    Ok(
        next_index(current.index, chain.len(), direction).map(|index| Position {
            oid: chain[index],
            index,
            total: chain.len(),
        }),
    )
}

/// Index arithmetic for a step, kept separate from the repository so the
/// boundaries can be tested directly. Older is a higher index, newer a lower
/// one, and running off either end yields `None`.
fn next_index(index: usize, total: usize, direction: Direction) -> Option<usize> {
    match direction {
        Direction::Older => {
            let next = index + 1;
            (next < total).then_some(next)
        }
        Direction::Newer => index.checked_sub(1),
    }
}

/// Record where the cursor now sits. Sitting on the newest commit is the
/// absence of a position, so that state is stored by removing the file.
pub fn save(repo: &Repository, position: &Position) -> Result<()> {
    if position.is_newest() {
        return clear(repo);
    }
    let path = position_file(repo);
    fs::write(&path, position.oid.to_string())
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

/// Forget the cursor, which puts it back on the newest commit. Committing does
/// this: the commit just made is now the newest version.
pub fn clear(repo: &Repository) -> Result<()> {
    let path = position_file(repo);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("cannot remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_older_walks_towards_the_first_commit() {
        assert_eq!(next_index(0, 3, Direction::Older), Some(1));
        assert_eq!(next_index(1, 3, Direction::Older), Some(2));
    }

    #[test]
    fn stepping_newer_walks_back_towards_the_newest_commit() {
        assert_eq!(next_index(2, 3, Direction::Newer), Some(1));
        assert_eq!(next_index(1, 3, Direction::Newer), Some(0));
    }

    #[test]
    fn stops_at_the_oldest_commit() {
        assert_eq!(next_index(2, 3, Direction::Older), None);
    }

    #[test]
    fn stops_at_the_newest_commit() {
        assert_eq!(next_index(0, 3, Direction::Newer), None);
    }

    #[test]
    fn a_single_commit_cannot_step_either_way() {
        assert_eq!(next_index(0, 1, Direction::Older), None);
        assert_eq!(next_index(0, 1, Direction::Newer), None);
    }
}
