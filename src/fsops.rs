use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context, Result};

/// What a single upload actually did to the working tree.
#[derive(Debug, Default)]
pub struct CopyStats {
    pub copied: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub broken_links: Vec<PathBuf>,
}

/// Files claimed per batch by a worker; keeps threads in separate directories.
const CHUNK: usize = 128;

struct Job {
    src: PathBuf,
    dst: PathBuf,
    meta: fs::Metadata,
}

/// Mirror `src` onto `dst`, reusing files that are already up to date.
///
/// Files are copied on worker threads because a dotfiles tree is thousands of
/// tiny files: the cost is per-file syscall latency, not bandwidth.
pub fn copy_tree(src: &Path, dst: &Path) -> Result<CopyStats> {
    // Only a top-level file needs its parent created here; every file found
    // inside the walk already had its directory created by the walk itself.
    if src.is_file()
        && let Some(parent) = dst.parent()
    {
        fs::create_dir_all(parent)?;
    }

    let mut jobs = Vec::new();
    let mut broken = Vec::new();
    collect(src, dst, &mut jobs, &mut broken)?;

    let copied = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let bytes = AtomicU64::new(0);
    let cursor = AtomicUsize::new(0);
    // `failed` is checked per file, so it stays lock-free; the mutex is only
    // touched on the error path, to keep the first failure for reporting.
    let failed = AtomicBool::new(false);
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);

    let workers = worker_count(jobs.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let mut local_copied = 0usize;
                let mut local_skipped = 0usize;
                let mut local_bytes = 0u64;
                loop {
                    // Claim a contiguous run of files. The walk emits files in
                    // directory order, so a run keeps each thread inside one
                    // directory instead of having every thread contend on the
                    // same parent inode.
                    let start = cursor.fetch_add(CHUNK, Ordering::Relaxed);
                    if start >= jobs.len() || failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let end = (start + CHUNK).min(jobs.len());
                    for job in &jobs[start..end] {
                        match copy_file(&job.src, &job.dst, &job.meta) {
                            Ok(Some(n)) => {
                                local_copied += 1;
                                local_bytes += n;
                            }
                            Ok(None) => local_skipped += 1,
                            Err(err) => {
                                failed.store(true, Ordering::Relaxed);
                                *failure.lock().unwrap() = Some(err);
                                break;
                            }
                        }
                    }
                }
                copied.fetch_add(local_copied, Ordering::Relaxed);
                skipped.fetch_add(local_skipped, Ordering::Relaxed);
                bytes.fetch_add(local_bytes, Ordering::Relaxed);
            });
        }
    });

    if let Some(err) = failure.into_inner().unwrap() {
        return Err(err);
    }
    Ok(CopyStats {
        copied: copied.into_inner(),
        skipped: skipped.into_inner(),
        bytes: bytes.into_inner(),
        broken_links: broken,
    })
}

/// Walk `src`, creating destination directories and queueing every file.
fn collect(src: &Path, dst: &Path, jobs: &mut Vec<Job>, broken: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        // metadata() follows symlinks, so a link to a directory is mirrored as
        // a directory and a link to a file is stored as its contents.
        let meta = match fs::metadata(&src) {
            Ok(meta) => meta,
            Err(_) if fs::symlink_metadata(&src).is_ok() => {
                broken.push(src);
                continue;
            }
            Err(err) => return Err(err).context(format!("cannot read {}", src.display())),
        };
        if !meta.is_dir() {
            jobs.push(Job { src, dst, meta });
            continue;
        }
        fs::create_dir_all(&dst)?;
        for entry in fs::read_dir(&src).with_context(|| format!("cannot read {}", src.display()))? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                continue;
            }
            stack.push((entry.path(), dst.join(entry.file_name())));
        }
    }
    Ok(())
}

/// Copy one file, returning the bytes written, or `None` if it was already current.
fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata) -> Result<Option<u64>> {
    if is_current(meta, dst) {
        return Ok(None);
    }
    // fs::copy carries the permission bits across, so no extra chmod is needed.
    let written = fs::copy(src, dst)
        .with_context(|| format!("cannot copy {} to {}", src.display(), dst.display()))?;
    Ok(Some(written))
}

/// A destination is current when it has the same size and was written after the
/// source was last modified - the same heuristic rsync uses by default.
fn is_current(src: &fs::Metadata, dst: &Path) -> bool {
    let Ok(dst_meta) = fs::metadata(dst) else {
        return false;
    };
    if dst_meta.len() != src.len() || dst_meta.permissions() != src.permissions() {
        return false;
    }
    match (src.modified(), dst_meta.modified()) {
        (Ok(src_time), Ok(dst_time)) => dst_time >= src_time,
        _ => false,
    }
}

fn worker_count(jobs: usize) -> usize {
    if jobs < 2 {
        return 1;
    }
    if let Some(n) = std::env::var("DOTGIT_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return usize::max(1, n);
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    cpus.clamp(1, 8).min(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch(label: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "dotgit-test-{}-{}-{}",
            std::process::id(),
            label,
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn copies_a_tree_and_preserves_layout() {
        let root = scratch("tree");
        let src = root.join("src");
        write(&src.join("a.conf"), "a");
        write(&src.join("nested/b.conf"), "b");

        let dst = root.join("dst");
        let stats = copy_tree(&src, &dst).unwrap();

        assert_eq!(stats.copied, 2);
        assert_eq!(stats.skipped, 0);
        assert_eq!(fs::read_to_string(dst.join("a.conf")).unwrap(), "a");
        assert_eq!(fs::read_to_string(dst.join("nested/b.conf")).unwrap(), "b");
    }

    #[test]
    fn second_copy_skips_unchanged_files() {
        let root = scratch("skip");
        let src = root.join("src");
        write(&src.join("a.conf"), "a");
        let dst = root.join("dst");

        assert_eq!(copy_tree(&src, &dst).unwrap().copied, 1);
        let again = copy_tree(&src, &dst).unwrap();
        assert_eq!(again.copied, 0);
        assert_eq!(again.skipped, 1);
        assert_eq!(again.bytes, 0);
    }

    #[test]
    fn modified_source_is_copied_again() {
        let root = scratch("modified");
        let src = root.join("src");
        write(&src.join("a.conf"), "original");
        let dst = root.join("dst");
        copy_tree(&src, &dst).unwrap();

        // A different length is enough to force a re-copy regardless of clock
        // granularity on the test machine.
        write(&src.join("a.conf"), "a much longer replacement value");
        let again = copy_tree(&src, &dst).unwrap();

        assert_eq!(again.copied, 1);
        assert_eq!(
            fs::read_to_string(dst.join("a.conf")).unwrap(),
            "a much longer replacement value"
        );
    }

    #[test]
    fn copies_a_single_file() {
        let root = scratch("single");
        let src = root.join("src/.zshrc");
        write(&src, "export PATH=/usr/bin");
        let dst = root.join("dst/.zshrc");

        let stats = copy_tree(&src, &dst).unwrap();

        assert_eq!(stats.copied, 1);
        assert_eq!(fs::read_to_string(&dst).unwrap(), "export PATH=/usr/bin");
    }

    #[test]
    fn skips_the_git_directory() {
        let root = scratch("gitdir");
        let src = root.join("src");
        write(&src.join(".git/HEAD"), "ref: refs/heads/master");
        write(&src.join("keep.conf"), "keep");
        let dst = root.join("dst");

        let stats = copy_tree(&src, &dst).unwrap();

        assert_eq!(stats.copied, 1);
        assert!(!dst.join(".git").exists());
        assert!(dst.join("keep.conf").exists());
    }

    #[test]
    fn reports_broken_symlinks_without_failing() {
        let root = scratch("broken");
        let src = root.join("src");
        write(&src.join("good.conf"), "good");
        std::os::unix::fs::symlink(root.join("nowhere"), src.join("dangling")).unwrap();
        let dst = root.join("dst");

        let stats = copy_tree(&src, &dst).unwrap();

        assert_eq!(stats.copied, 1);
        assert_eq!(stats.broken_links.len(), 1);
        assert!(stats.broken_links[0].ends_with("dangling"));
    }

    #[test]
    fn preserves_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("perms");
        let src = root.join("src/run.sh");
        write(&src, "#!/bin/sh\n");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        let dst = root.join("dst/run.sh");

        copy_tree(&src, &dst).unwrap();

        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}
