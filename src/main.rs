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

fn main() {
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
        let credentials = host_credentials(&host)?;
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

/// Find a token for `host`, logging in through that forge's CLI if there isn't
/// one yet. An unrecognised host gets whatever `gh` happens to hold for it,
/// since that is where a self-hosted GitHub Enterprise login would land.
fn host_credentials(host: &str) -> Result<gh::HostCredentials> {
    if let Some(forge) = gh::forge_for_host(host) {
        // An exported token wins over the CLIs: it is the documented escape
        // hatch for CI, where no interactive login is possible.
        if forge == gh::Forge::GitLab {
            if let Ok(token) = env::var("GITLAB_TOKEN").or_else(|_| env::var("GL_TOKEN")) {
                return Ok(gh::HostCredentials {
                    username: "oauth2".into(),
                    token,
                });
            }
        }
        gh::ensure_forge_auth(forge, host)?;
        return match forge {
            gh::Forge::GitHub => gh::read_host_credentials(host),
            gh::Forge::GitLab => gh::read_glab_credentials(host),
        };
    }
    gh::read_host_credentials(host)
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
    if let Ok(repo) = git::open_repo() {
        if repo.find_remote("origin").is_err() {
            let workdir = git::repo_workdir(&repo)?;
            git::set_origin(&repo, url)?;
            return Ok(workdir);
        }
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
    fn formats_byte_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
