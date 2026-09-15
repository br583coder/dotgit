//! Local mirrors of a repository's history, kept as git bundles.
//!
//! A bundle is a single file holding every object and ref in the repository,
//! and `git clone` treats it like a remote. That makes it the smallest thing
//! that can bring a repository back from nothing if the forge-side copy is
//! deleted, and it needs no server, no daemon and no network to restore.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use git2::Repository;

use crate::error::DotgitError;

pub const EXTENSION: &str = "bundle";

#[derive(Clone, Debug)]
pub struct Backup {
    pub path: PathBuf,
    pub bytes: u64,
    /// The repository a backup belongs to, read back from its file name.
    pub repo: Option<String>,
    /// Its `YYYYMMDD-HHMMSS` stamp, absent for a file named by hand.
    stamp: Option<String>,
}

/// Where backups live unless `--to` says otherwise: alongside the rest of the
/// user's application data, not inside the repository, so that deleting or
/// re-cloning the repository never takes the backups with it.
pub fn default_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .ok_or_else(|| anyhow!("cannot determine your data directory"))?;
    Ok(base.join("dotgit").join("backups"))
}

/// Write a bundle of every ref in `repo` and return where it landed.
///
/// `dest` may be a directory to name a file inside, or a full file path.
pub fn create(repo: &Repository, dest: Option<&Path>) -> Result<Backup> {
    let workdir = crate::git::repo_workdir(repo)?;
    // `git bundle` refuses to bundle nothing, so say why in dotgit's terms
    // rather than passing git's message through.
    if repo.head().is_err() {
        return Err(DotgitError::from("nothing to back up yet - make a commit first").into());
    }

    let path = match dest {
        Some(dest) if dest.is_dir() => dest.join(bundle_name(&workdir, now_secs())),
        Some(dest) => dest.to_path_buf(),
        None => default_dir()?.join(bundle_name(&workdir, now_secs())),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }

    // `--all` takes branches, tags and remote-tracking refs, so the bundle
    // also records where the remote stood the last time we saw it.
    run_git(
        &workdir,
        &["bundle", "create"],
        &[path.as_path()],
        &["--all"],
    )?;
    // Verifying costs a fraction of writing and turns a silently corrupt
    // backup into an error now, rather than a surprise on the day it is needed.
    run_git(&workdir, &["bundle", "verify"], &[path.as_path()], &[])?;

    let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (repo, stamp) = split_name(&path).unzip();
    Ok(Backup {
        path,
        bytes,
        repo,
        stamp,
    })
}

fn run_git(workdir: &Path, args: &[&str], paths: &[&Path], trailing: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(workdir).args(args);
    for path in paths {
        command.arg(path);
    }
    let output = command
        .args(trailing)
        .output()
        .map_err(|e| anyhow!("git {} failed: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(())
}

/// Every backup in `dir`, newest first.
///
/// Ordering comes from the stamp inside each name, not the name as a whole:
/// a directory holding several repositories' backups would otherwise sort by
/// repository name and call the wrong file newest. A bundle named by hand has
/// no stamp to read, so it sorts last rather than displacing a real backup.
pub fn list(dir: &Path) -> Result<Vec<Backup>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // No directory yet simply means no backups have been taken.
        Err(_) => return Ok(Vec::new()),
    };
    let mut backups = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let (repo, stamp) = split_name(&path).unzip();
        backups.push(Backup {
            path,
            bytes,
            repo,
            stamp,
        });
    }
    backups.sort_by(|a, b| b.stamp.cmp(&a.stamp).then_with(|| a.path.cmp(&b.path)));
    Ok(backups)
}

/// Split `dots-20240101-000000.bundle` into its repository name and stamp.
/// Parsing from the right keeps working for a repository whose own name
/// contains a dash.
fn split_name(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(&format!(".{EXTENSION}"))?;
    // "-YYYYMMDD-HHMMSS" is a fixed 16 characters.
    let split = stem.len().checked_sub(16)?;
    let (repo, stamp) = stem.split_at(split);
    let stamp = stamp.strip_prefix('-')?;
    let (date, time) = stamp.split_once('-')?;
    if date.len() != 8
        || time.len() != 6
        || !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((repo.to_string(), stamp.to_string()))
}

/// Delete all but the `keep` newest backups of `repo` in `dir`, returning what
/// went. Backups of other repositories sharing the directory are left alone,
/// as is any bundle whose name dotgit did not choose.
pub fn prune(dir: &Path, keep: usize, repo: Option<&str>) -> Result<Vec<PathBuf>> {
    let backups = list(dir)?
        .into_iter()
        .filter(|b| b.repo.is_some() && (repo.is_none() || b.repo.as_deref() == repo))
        .collect::<Vec<_>>();
    let mut removed = Vec::new();
    for backup in backups.into_iter().skip(keep) {
        fs::remove_file(&backup.path)
            .map_err(|e| anyhow!("cannot remove {}: {e}", backup.path.display()))?;
        removed.push(backup.path);
    }
    Ok(removed)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `<repository directory>-<UTC timestamp>.bundle`, so backups of several
/// repositories can share one directory and still sort by name into age order.
fn bundle_name(workdir: &Path, secs: u64) -> String {
    let repo = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("repo");
    format!("{repo}-{}.{EXTENSION}", format_timestamp(secs))
}

/// `YYYYMMDD-HHMMSS` in UTC. Formatting this by hand keeps the dependency
/// list as it is for the sake of one filename. Public because the TUI shows
/// commit dates with it rather than growing a second implementation.
pub fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (time / 3600, (time % 3600) / 60, time % 60);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Days since the Unix epoch to a calendar date, by Howard Hinnant's
/// `civil_from_days`: shift the year to start in March so the leap day falls
/// at the end, then count 400-year eras, which have a fixed length in days.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps_in_utc() {
        assert_eq!(format_timestamp(0), "19700101-000000");
        // 2023-11-14 22:13:20 UTC
        assert_eq!(format_timestamp(1_700_000_000), "20231114-221320");
        // A leap day, the case the calendar arithmetic exists for.
        assert_eq!(format_timestamp(1_709_208_000), "20240229-120000");
    }

    #[test]
    fn names_backups_after_the_repository_directory() {
        assert_eq!(
            bundle_name(Path::new("/home/me/dotfiles"), 0),
            "dotfiles-19700101-000000.bundle"
        );
        // A trailing slash must not swallow the directory name.
        assert_eq!(
            bundle_name(Path::new("/home/me/dotfiles/"), 0),
            "dotfiles-19700101-000000.bundle"
        );
    }

    #[test]
    fn reads_the_repository_and_stamp_back_out_of_a_name() {
        assert_eq!(
            split_name(Path::new("/b/dots-20240101-000000.bundle")),
            Some(("dots".into(), "20240101-000000".into()))
        );
        // A repository name containing a dash still parses.
        assert_eq!(
            split_name(Path::new("/b/my-dot-files-20240101-000000.bundle")),
            Some(("my-dot-files".into(), "20240101-000000".into()))
        );
        // Anything not matching the pattern has no stamp to sort or prune by.
        assert_eq!(split_name(Path::new("/b/manual.bundle")), None);
        assert_eq!(split_name(Path::new("/b/dots-2024-01-01.bundle")), None);
        assert_eq!(
            split_name(Path::new("/b/dots-20240101-0000xx.bundle")),
            None
        );
    }

    #[test]
    fn orders_by_stamp_across_repositories_not_by_name() {
        let dir = std::env::temp_dir().join("dotgit-backup-ordering");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Alphabetically "alpha" precedes "zeta", but zeta's backup is newer.
        for name in [
            "zeta-20250101-000000.bundle",
            "alpha-20240101-000000.bundle",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let first = list(&dir).unwrap()[0].path.clone();
        assert!(first.ends_with("zeta-20250101-000000.bundle"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pruning_leaves_other_repositories_alone() {
        let dir = std::env::temp_dir().join("dotgit-backup-scoping");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "dots-20240101-000000.bundle",
            "dots-20250101-000000.bundle",
            "notes-20230101-000000.bundle",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let removed = prune(&dir, 1, Some("dots")).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("dots-20240101-000000.bundle"));
        // The other repository's only backup survives, however old it is.
        assert!(dir.join("notes-20230101-000000.bundle").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sorts_backups_newest_first_and_ignores_other_files() {
        let dir = std::env::temp_dir().join("dotgit-backup-listing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in [
            "dots-20240101-000000.bundle",
            "dots-20250101-000000.bundle",
            "notes.txt",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let names: Vec<String> = list(&dir)
            .unwrap()
            .iter()
            .map(|b| b.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            ["dots-20250101-000000.bundle", "dots-20240101-000000.bundle"]
        );

        let removed = prune(&dir, 1, None).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("dots-20240101-000000.bundle"));
        assert_eq!(list(&dir).unwrap().len(), 1);
        // Pruning must never touch files it does not manage.
        assert!(dir.join("notes.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_a_missing_directory_is_empty_not_an_error() {
        assert!(
            list(Path::new("/nonexistent/dotgit/backups"))
                .unwrap()
                .is_empty()
        );
    }
}
