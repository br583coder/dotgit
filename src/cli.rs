//! The `dotgit` command line interface, shared by the `dotgit` and `dg`
//! binaries.

use std::env;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};

use crate::error::DotgitError;
use crate::ops::{self, human_bytes};
use crate::{backup, gh, git, history};

#[derive(Parser)]
#[command(
    name = "dotgit",
    version,
    about = "dotfile manager backed by git + gh/glab auth"
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
    /// Revert a commit and push the resulting commit
    Revert {
        /// Commit or revision to revert (prompts with recent commits if omitted)
        commit: Option<String>,
    },
    /// Create a new repository on GitHub or GitLab and wire it up locally
    New {
        /// Repository name (may include an owner/namespace, e.g. me/dotfiles)
        name: String,
        /// Create it on GitHub without asking
        #[arg(long, conflicts_with = "gitlab")]
        github: bool,
        /// Create it on GitLab without asking
        #[arg(long, conflicts_with = "github")]
        gitlab: bool,
        /// Make the repository public
        #[arg(long, conflicts_with = "private")]
        public: bool,
        /// Make the repository private
        #[arg(long, conflicts_with = "public")]
        private: bool,
        /// Host to create it on (default: github.com / gitlab.com)
        #[arg(long)]
        host: Option<String>,
    },
    /// Step the working tree back one version (repeat to keep going back)
    Restore,
    /// Step the working tree forward one version, towards the newest change
    Rebase,
    /// Destroy the newest commit(s) entirely, locally and on the remote
    #[command(alias = "drop")]
    Pull {
        /// How many of the newest commits to destroy
        #[arg(short = 'n', long, default_value_t = 1)]
        count: usize,
        /// Do not ask for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
        /// Skip the safety backup taken before destroying anything
        #[arg(long)]
        no_backup: bool,
        /// Destroy locally only, leaving the commits on the remote
        #[arg(long)]
        no_push: bool,
    },
    /// Save the full commit history to a local bundle file
    Backup {
        /// Directory or file to write the bundle to
        #[arg(long, value_name = "PATH")]
        to: Option<PathBuf>,
        /// List existing backups instead of making one
        #[arg(long)]
        list: bool,
        /// Delete all but the newest N backups after making one
        #[arg(long, value_name = "N")]
        keep: Option<usize>,
    },
    /// Log into a host via gh or glab so dotgit can push as you
    Login {
        /// Host to log into (default: github.com, or gitlab.com with --gitlab)
        host: Option<String>,
        /// Use the GitHub CLI (gh)
        #[arg(long, conflicts_with = "gitlab")]
        github: bool,
        /// Use the GitLab CLI (glab)
        #[arg(long, conflicts_with = "github")]
        gitlab: bool,
    },
}

/// Parse the command line, run the requested command, and report failures the
/// way a command line tool should: a message on stderr and a non-zero exit.
pub fn run() {
    let cli = Cli::parse();
    let result = match cli.command {
        CliCommand::Upload { paths } => upload(&paths),
        CliCommand::Commit { message, no_push } => commit(message, no_push),
        CliCommand::Revert { commit } => revert(commit),
        CliCommand::New {
            name,
            github,
            gitlab,
            public,
            private,
            host,
        } => new_repo(&name, github, gitlab, public, private, host.as_deref()),
        CliCommand::Restore => step_history(history::Direction::Older),
        CliCommand::Rebase => step_history(history::Direction::Newer),
        CliCommand::Pull {
            count,
            yes,
            no_backup,
            no_push,
        } => pull(count, yes, !no_backup, !no_push),
        CliCommand::Backup { to, list, keep } => backup_cmd(to.as_deref(), list, keep),
        CliCommand::Login {
            host,
            github,
            gitlab,
        } => login(host.as_deref(), github, gitlab),
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
    let report = ops::upload(&repo, paths)?;

    for link in &report.stats.broken_links {
        eprintln!("dotgit: skipped broken symlink {}", link.display());
    }
    for rel in &report.staged {
        println!("uploaded {}", rel.display());
    }
    println!(
        "staged for commit ({} copied, {} unchanged, {})",
        report.stats.copied,
        report.stats.skipped,
        human_bytes(report.stats.bytes)
    );
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
    // The commit just made is the newest version, so any stepped-back cursor
    // no longer applies.
    history::clear(&repo)?;
    if no_push {
        println!("skipped push (--no-push)");
        return Ok(());
    }
    push(&repo)?;
    Ok(())
}

fn revert(commit: Option<String>) -> Result<()> {
    let repo = git::open_repo()?;
    let revision = match commit {
        Some(commit) if !commit.trim().is_empty() => commit,
        Some(_) => return Err(anyhow!("commit revision cannot be empty")),
        None => prompt_revert_commit(&repo)?,
    };
    let message = git::revert_commit(&repo, &revision)?;
    let head = repo.head()?;
    let new_commit = head.peel_to_commit()?;
    let id = new_commit.id().to_string();
    println!(
        "reverted {} in commit {} on {}",
        revision,
        &id[..7.min(id.len())],
        head.shorthand().unwrap_or("HEAD")
    );
    push(&repo)?;
    if !message.is_empty() {
        println!("message: {}", message.lines().next().unwrap_or_default());
    }
    Ok(())
}

fn push(repo: &git2::Repository) -> Result<()> {
    match ops::push(repo, false)? {
        ops::PushReport::Ssh { host } => println!("pushed to {host}"),
        ops::PushReport::Http { host, username } => println!("pushed to {host} as {username}"),
        ops::PushReport::Local { target } => println!("pushed to {target}"),
    }
    Ok(())
}

fn new_repo(
    name: &str,
    github: bool,
    gitlab: bool,
    public: bool,
    private: bool,
    host: Option<&str>,
) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DotgitError::from("repository name cannot be empty").into());
    }
    let forge = match forge_from_flags(github, gitlab) {
        Some(forge) => forge,
        None => prompt_forge()?,
    };
    let public = match (public, private) {
        (true, false) => true,
        (false, true) => false,
        (false, false) => prompt_visibility()?,
        (true, true) => unreachable!("clap prevents both visibility flags"),
    };
    let host = host.unwrap_or_else(|| forge.default_host());

    // Authenticate before creating anything, so a login failure leaves no
    // half-made repository behind.
    gh::ensure_forge_auth(forge, host)?;

    let url = gh::create_repo(forge, host, name, !public)?;
    println!(
        "created {} repository {name} ({})",
        forge.label(),
        if public { "public" } else { "private" }
    );

    let workdir = attach_local_repo(name, &url)?;
    println!("origin -> {url}");
    println!("local repository: {}", workdir.display());
    println!(
        "next: cd {} && dotgit upload ~/.config/... && dotgit commit",
        workdir.display()
    );
    Ok(())
}

/// Use the repository we are standing in when it has no `origin` yet;
/// otherwise start a fresh one in a directory named after the repository.
fn attach_local_repo(name: &str, url: &str) -> Result<PathBuf> {
    if let Ok(repo) = git::open_repo()
        && repo.find_remote("origin").is_err()
    {
        let workdir = git::repo_workdir(&repo)?;
        git::set_origin(&repo, url)?;
        return Ok(workdir);
    }
    let dir = env::current_dir()?.join(local_dir_name(name));
    if dir.exists() {
        return Err(anyhow!(
            "{} already exists; the remote was created, point a repository at {url} yourself",
            dir.display()
        ));
    }
    git::init_with_remote(&dir, url)?;
    Ok(dir)
}

/// `me/dotfiles` lives in a directory called `dotfiles`, not `me/dotfiles`.
fn local_dir_name(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn prompt_forge() -> Result<gh::Forge> {
    let mut input = String::new();
    loop {
        print!("Where should this repository live? [1] GitHub  [2] GitLab: ");
        io::stdout().flush()?;
        input.clear();
        if io::stdin().read_line(&mut input)? == 0 {
            return Err(DotgitError::from("no choice made; pass --github or --gitlab").into());
        }
        match parse_forge_choice(&input) {
            Some(forge) => return Ok(forge),
            None => eprintln!("please answer 1/github or 2/gitlab"),
        }
    }
}

fn parse_forge_choice(input: &str) -> Option<gh::Forge> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "gh" | "github" => Some(gh::Forge::GitHub),
        "2" | "gl" | "glab" | "gitlab" => Some(gh::Forge::GitLab),
        _ => None,
    }
}

fn prompt_visibility() -> Result<bool> {
    let mut input = String::new();
    loop {
        print!("Repository visibility? [1] Public  [2] Private: ");
        io::stdout().flush()?;
        input.clear();
        if io::stdin().read_line(&mut input)? == 0 {
            return Err(anyhow!("no visibility chosen; pass --public or --private"));
        }
        match parse_visibility_choice(&input) {
            Some(public) => return Ok(public),
            None => eprintln!("please answer 1/public or 2/private"),
        }
    }
}

fn parse_visibility_choice(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "public" | "pub" => Some(true),
        "2" | "private" | "priv" => Some(false),
        _ => None,
    }
}

fn prompt_revert_commit(repo: &git2::Repository) -> Result<String> {
    let commits = git::recent_commits(repo, 10)?;
    if commits.is_empty() {
        return Err(anyhow!("repository has no commits to revert"));
    }
    println!("Choose a commit to revert:");
    for (index, (id, message)) in commits.iter().enumerate() {
        println!(
            "  {}. {} {}",
            index + 1,
            &id.to_string()[..7],
            message.lines().next().unwrap_or_default()
        );
    }
    let mut input = String::new();
    loop {
        print!("Commit number or revision: ");
        io::stdout().flush()?;
        input.clear();
        if io::stdin().read_line(&mut input)? == 0 {
            return Err(anyhow!("no commit chosen"));
        }
        let choice = input.trim();
        if let Ok(number) = choice.parse::<usize>() {
            if let Some((id, _)) = commits.get(number.saturating_sub(1)) {
                return Ok(id.to_string());
            }
        } else if !choice.is_empty() && repo.revparse_single(choice).is_ok() {
            return Ok(choice.to_string());
        }
        eprintln!("choose a listed number or a valid commit revision");
    }
}

/// Move one version older or newer and report where we landed.
fn step_history(direction: history::Direction) -> Result<()> {
    let repo = git::open_repo()?;
    let next = match ops::step(&repo, direction)? {
        ops::StepReport::Moved(next) => next,
        ops::StepReport::Boundary => {
            println!("{}", boundary_message(direction));
            return Ok(());
        }
    };

    let (id, subject) = git::describe(&repo, next.oid)?;
    println!("{} {id} \"{subject}\" ({})", verb(direction), place(&next));
    if next.is_newest() {
        println!("this is the newest change");
    } else {
        println!("`dotgit rebase` steps forward, `dotgit commit` keeps this version");
    }
    Ok(())
}

fn boundary_message(direction: history::Direction) -> &'static str {
    match direction {
        history::Direction::Older => "oldest change reached",
        history::Direction::Newer => "newest change released",
    }
}

fn verb(direction: history::Direction) -> &'static str {
    match direction {
        history::Direction::Older => "restored to",
        history::Direction::Newer => "moved forward to",
    }
}

/// How far back from the newest change this version sits.
fn place(position: &history::Position) -> String {
    let newest = position.total.saturating_sub(1);
    match position.index {
        0 => "newest change".to_string(),
        1 => format!("1 version back of {newest}"),
        back => format!("{back} versions back of {newest}"),
    }
}

/// Destroy the newest commit(s). This is the one command that loses work on
/// purpose, so it shows exactly what will go, backs the history up first, and
/// asks before doing any of it.
fn pull(count: usize, yes: bool, backup_first: bool, push_after: bool) -> Result<()> {
    let repo = git::open_repo()?;
    let plan = ops::plan_drop(&repo, count)?;

    println!("About to destroy {} commit(s):", plan.doomed.len());
    for commit in &plan.doomed {
        println!("  {}  {}", commit.id, commit.subject);
    }
    println!(
        "HEAD would become {} \"{}\"",
        plan.target_id, plan.target_subject
    );
    if plan.dirty {
        println!("warning: uncommitted changes in your working tree will also be destroyed");
    }
    // Only promise a force-push when there is actually a remote to push to.
    let has_remote = git::remote_url(&repo).is_ok();
    if push_after && has_remote {
        println!("the remote will be force-pushed, so it loses them too");
    }

    if !yes && !confirm("Destroy them?")? {
        println!("cancelled, nothing was destroyed");
        return Ok(());
    }

    let report = ops::drop_commits(&repo, &plan, backup_first, push_after)?;
    if let Some(path) = &report.backup {
        println!("backed up first: {}", path.display());
    }
    println!(
        "destroyed {} commit(s); HEAD is now {} \"{}\"",
        report.removed, report.head_id, report.head_subject
    );
    match (&report.pushed, &report.push_error) {
        (Some(ops::PushReport::Ssh { host }), _)
        | (Some(ops::PushReport::Http { host, .. }), _) => {
            println!("force-pushed to {host} (the remote no longer has them)");
        }
        (Some(ops::PushReport::Local { target }), _) => {
            println!("force-pushed to {target}");
        }
        (None, Some(err)) => {
            eprintln!("dotgit: destroyed locally, but the push failed: {err}");
            eprintln!(
                "dotgit: the remote still has them - fix the cause and run `dotgit pull --push`"
            );
        }
        (None, None) => {}
    }
    if report.backup.is_some() {
        println!("recover with: git clone <the bundle above> <directory>");
    }
    Ok(())
}

/// Ask a yes/no question, defaulting to no: anything but an explicit yes
/// leaves the repository alone.
fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer)? == 0 {
        return Ok(false);
    }
    Ok(is_yes(&answer))
}

/// Only an explicit yes counts. Everything else - an empty line, a stray word,
/// a closed stdin - leaves the repository alone, because the question is only
/// ever asked before destroying something.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn backup_cmd(to: Option<&Path>, list: bool, keep: Option<usize>) -> Result<()> {
    let dir = match to {
        Some(path) => path.to_path_buf(),
        None => backup::default_dir()?,
    };
    if list {
        return list_backups(&dir);
    }

    let repo = git::open_repo()?;
    let saved = backup::create(&repo, to)?;
    println!(
        "saved {} ({})",
        saved.path.display(),
        human_bytes(saved.bytes)
    );

    if let Some(keep) = keep {
        // Pruning is scoped to the directory the bundle went into, so a
        // one-off `--to some/file.bundle` never sweeps a neighbouring folder.
        let pruned_dir = saved.path.parent().unwrap_or(&dir).to_path_buf();
        for removed in backup::prune(&pruned_dir, keep.max(1), saved.repo.as_deref())? {
            println!("removed {}", removed.display());
        }
    }

    println!(
        "restore with: git clone {} <directory>",
        saved.path.display()
    );
    Ok(())
}

fn list_backups(dir: &Path) -> Result<()> {
    let backups = backup::list(dir)?;
    if backups.is_empty() {
        println!("no backups in {}", dir.display());
        return Ok(());
    }
    for entry in &backups {
        println!("{}  {}", entry.path.display(), human_bytes(entry.bytes));
    }
    println!("{} backup(s) in {}", backups.len(), dir.display());
    Ok(())
}

fn login(host: Option<&str>, github: bool, gitlab: bool) -> Result<()> {
    let forge = forge_from_flags(github, gitlab);
    // An explicit host names its own forge (`dotgit login gitlab.company` needs
    // glab), so only fall back to the flag default when it doesn't.
    let (forge, host) = match host {
        Some(host) => (
            forge
                .or_else(|| gh::forge_for_host(host))
                .unwrap_or_default(),
            host.to_string(),
        ),
        None => {
            let forge = forge.unwrap_or_default();
            (forge, forge.default_host().to_string())
        }
    };
    gh::ensure_cli(forge)?;
    gh::forge_login(forge, &host)?;
    println!(
        "logged in to {host} via {}; dotgit will push as this account",
        forge.cli()
    );
    Ok(())
}

fn forge_from_flags(github: bool, gitlab: bool) -> Option<gh::Forge> {
    match (github, gitlab) {
        (true, _) => Some(gh::Forge::GitHub),
        (_, true) => Some(gh::Forge::GitLab),
        _ => None,
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
    fn recognises_hosts_by_label_not_substring() {
        assert_eq!(gh::forge_for_host("github.com"), Some(gh::Forge::GitHub));
        assert_eq!(gh::forge_for_host("gitlab.com"), Some(gh::Forge::GitLab));
        assert_eq!(
            gh::forge_for_host("gitlab.example.com"),
            Some(gh::Forge::GitLab)
        );
        // A host that merely contains the name must not be misrouted.
        assert_eq!(gh::forge_for_host("my-gitlab-mirror.example.com"), None);
        assert_eq!(
            gh::forge_for_host("github-enterprise-proxy.example.com"),
            None
        );
    }

    #[test]
    fn flags_pick_the_forge_and_neither_flag_leaves_it_open() {
        assert_eq!(forge_from_flags(true, false), Some(gh::Forge::GitHub));
        assert_eq!(forge_from_flags(false, true), Some(gh::Forge::GitLab));
        assert_eq!(forge_from_flags(false, false), None);
    }

    #[test]
    fn accepts_either_number_or_name_for_the_forge_prompt() {
        assert_eq!(parse_forge_choice("1\n"), Some(gh::Forge::GitHub));
        assert_eq!(parse_forge_choice(" GitHub \n"), Some(gh::Forge::GitHub));
        assert_eq!(parse_forge_choice("2"), Some(gh::Forge::GitLab));
        assert_eq!(parse_forge_choice("gitlab"), Some(gh::Forge::GitLab));
        assert_eq!(parse_forge_choice(""), None);
        assert_eq!(parse_forge_choice("bitbucket"), None);
    }

    #[test]
    fn parses_visibility_choices() {
        assert_eq!(parse_visibility_choice("1"), Some(true));
        assert_eq!(parse_visibility_choice("public"), Some(true));
        assert_eq!(parse_visibility_choice("2"), Some(false));
        assert_eq!(parse_visibility_choice("private"), Some(false));
        assert_eq!(parse_visibility_choice("maybe"), None);
    }

    #[test]
    fn strips_the_namespace_from_the_local_directory_name() {
        assert_eq!(local_dir_name("dotfiles"), "dotfiles");
        assert_eq!(local_dir_name("me/dotfiles"), "dotfiles");
        assert_eq!(local_dir_name("group/sub/dotfiles"), "dotfiles");
    }

    #[test]
    fn reports_where_a_version_sits_in_the_history() {
        let at = |index, total| {
            place(&history::Position {
                oid: git2::Oid::zero(),
                index,
                total,
            })
        };
        assert_eq!(at(0, 5), "newest change");
        assert_eq!(at(1, 5), "1 version back of 4");
        assert_eq!(at(3, 5), "3 versions back of 4");
    }

    #[test]
    fn only_an_explicit_yes_confirms_a_destructive_action() {
        assert!(is_yes("y"));
        assert!(is_yes("Y\n"));
        assert!(is_yes(" yes "));
        assert!(!is_yes(""));
        assert!(!is_yes("\n"));
        assert!(!is_yes("n"));
        // A near-miss must not be read as consent.
        assert!(!is_yes("yeah"));
        assert!(!is_yes("yep"));
    }

    #[test]
    fn names_each_end_of_the_history() {
        assert_eq!(
            boundary_message(history::Direction::Older),
            "oldest change reached"
        );
        assert_eq!(
            boundary_message(history::Direction::Newer),
            "newest change released"
        );
    }
}
