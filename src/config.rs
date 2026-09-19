use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::backup;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub logging: Logging,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    /// `None` means the file did not mention it, which is different from
    /// `Some(true)`: only a setting that was actually written down may override
    /// one from a file read earlier.
    pub enabled: Option<bool>,
    pub path: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut config = Self::default();
        for path in config_paths()? {
            if path.exists() {
                let contents = fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?;
                let override_config: Config = toml::from_str(&contents)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                config.merge(override_config);
            }
        }
        Ok(config)
    }

    /// Later files win, but only for the settings they actually contain.
    fn merge(&mut self, other: Config) {
        if other.logging.enabled.is_some() {
            self.logging.enabled = other.logging.enabled;
        }
        if other.logging.path.is_some() {
            self.logging.path = other.logging.path;
        }
    }
}

/// Which front end ran a command. One log file collects both, so each line
/// says where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Cli,
    Tui,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Tui => "tui",
        }
    }
}

impl Logging {
    /// Logging is on unless a file turned it off.
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    pub fn path(&self) -> Result<PathBuf> {
        let path = self
            .path
            .as_deref()
            .map(expand_home)
            .transpose()?
            .unwrap_or_else(default_log_path);
        Ok(path)
    }

    /// Append one line describing what a command did. Generic over the value a
    /// command returns, so both front ends log through this: the CLI hands over
    /// a `Result<()>`, the TUI a `Result<String>` holding the message it shows.
    pub fn record<T>(&self, source: Source, command: &str, result: &Result<T>) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let path = self.path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let line = log_line(
            &timestamp(),
            source,
            command,
            repository().as_deref(),
            result,
        );
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?
            .write_all(line.as_bytes())
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

/// Files are read in order, each overriding the settings the previous one set:
/// your account's config, then the repository's, then the directory you are
/// standing in. The repository root is included because every other dotgit
/// command works from anywhere inside the repo, so its config should too.
fn config_paths() -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if let Some(config_dir) = dirs::config_dir() {
        paths.push(config_dir.join("dotgit").join("dotgit.toml"));
    }
    if let Some(root) = repository() {
        paths.push(PathBuf::from(root).join("dotgit.toml"));
    }
    let here = env::current_dir()?.join("dotgit.toml");
    if !paths.contains(&here) {
        paths.push(here);
    }
    Ok(paths)
}

/// The working directory of the repository we are in, if we are in one. The log
/// records it because one log file collects commands from every repository.
fn repository() -> Option<String> {
    git2::Repository::discover(".")
        .ok()
        .and_then(|repo| repo.workdir().map(|dir| dir.display().to_string()))
}

/// One log line as `key=value` pairs, with the failure reason kept: a log that
/// only says something failed cannot answer the question it was written for.
fn log_line<T>(
    timestamp: &str,
    source: Source,
    command: &str,
    repository: Option<&str>,
    result: &Result<T>,
) -> String {
    let mut line = format!("{timestamp} via={} command={command}", source.label());
    match result {
        Ok(_) => line.push_str(" outcome=success"),
        Err(err) => {
            line.push_str(" outcome=failure");
            if let Some(repository) = repository {
                line.push_str(&format!(" repo={repository}"));
            }
            // Flatten the message so one event stays on one line.
            let reason = format!("{err:#}").replace(['\n', '\r'], " ");
            line.push_str(&format!(" error=\"{}\"", reason.replace('"', "'")));
            return line + "\n";
        }
    }
    if let Some(repository) = repository {
        line.push_str(&format!(" repo={repository}"));
    }
    line + "\n"
}

fn default_log_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("dotgit")
        .join("dotgit.log")
}

fn expand_home(value: &str) -> Result<PathBuf> {
    if value == "~" || value.starts_with("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("cannot resolve ~ (no $HOME)"))?;
        return Ok(PathBuf::from(home).join(value.strip_prefix("~/").unwrap_or("")));
    }
    Ok(PathBuf::from(value))
}

/// `YYYYMMDD-HHMMSS` in UTC, the same shape the backup filenames use, so a log
/// line and a bundle taken at the same moment are obviously related.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    backup::format_timestamp(seconds)
}

use std::io::Write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_logging_is_enabled() {
        assert!(Logging::default().enabled());
        assert!(Logging::default().path().unwrap().ends_with("dotgit.log"));
    }

    #[test]
    fn a_file_that_omits_a_setting_leaves_it_alone() {
        // The account config turns logging off; a repository config that only
        // names a path must not turn it back on.
        let mut config: Config = toml::from_str("[logging]\nenabled = false\n").unwrap();
        let local: Config = toml::from_str("[logging]\npath = \"/tmp/x.log\"\n").unwrap();
        config.merge(local);
        assert!(!config.logging.enabled());
        assert_eq!(config.logging.path.as_deref(), Some("/tmp/x.log"));
    }

    #[test]
    fn a_file_that_names_a_setting_overrides_it() {
        let mut config: Config = toml::from_str("[logging]\nenabled = false\n").unwrap();
        let local: Config = toml::from_str("[logging]\nenabled = true\n").unwrap();
        config.merge(local);
        assert!(config.logging.enabled());
    }

    #[test]
    fn an_empty_file_changes_nothing() {
        let mut config: Config = toml::from_str("[logging]\nenabled = false\n").unwrap();
        config.merge(toml::from_str("").unwrap());
        assert!(!config.logging.enabled());
    }

    #[test]
    fn a_misspelled_setting_is_reported_rather_than_ignored() {
        // Silently falling back to defaults would leave the user believing a
        // setting had taken effect.
        assert!(toml::from_str::<Config>("[loging]\nenabled = false\n").is_err());
        assert!(toml::from_str::<Config>("[logging]\nenable = false\n").is_err());
        // A top-level key outside any section is the same kind of mistake.
        assert!(toml::from_str::<Config>("enabled = false\n").is_err());
        // The correct spelling still parses.
        assert!(toml::from_str::<Config>("[logging]\nenabled = false\n").is_ok());
    }

    #[test]
    fn a_success_line_names_the_command_and_repository() {
        let line = log_line(
            "20260918-101500",
            Source::Cli,
            "commit",
            Some("/home/me/dots/"),
            &Ok(()),
        );
        assert_eq!(
            line,
            "20260918-101500 via=cli command=commit outcome=success repo=/home/me/dots/\n"
        );
    }

    #[test]
    fn a_line_says_which_front_end_ran_the_command() {
        let line = log_line("20260918-101500", Source::Tui, "stage", None, &Ok(()));
        assert_eq!(
            line,
            "20260918-101500 via=tui command=stage outcome=success\n"
        );
    }

    #[test]
    fn the_value_a_command_returns_does_not_affect_the_line() {
        // The TUI logs `Result<String>`; only success or failure is recorded.
        let result: Result<String> = Ok("staged .zshrc".into());
        let line = log_line("20260918-101500", Source::Tui, "stage", None, &result);
        assert_eq!(
            line,
            "20260918-101500 via=tui command=stage outcome=success\n"
        );
    }

    #[test]
    fn a_failure_line_keeps_the_reason() {
        let result: Result<()> = Err(anyhow!("no git remote 'origin' configured"));
        let line = log_line("20260918-101500", Source::Cli, "commit", None, &result);
        assert_eq!(
            line,
            "20260918-101500 via=cli command=commit outcome=failure error=\"no git remote 'origin' configured\"\n"
        );
    }

    #[test]
    fn a_multi_line_failure_stays_on_one_line() {
        let result: Result<()> = Err(anyhow!("first line\nsecond \"quoted\" line"));
        let line = log_line("20260918-101500", Source::Tui, "pull", None, &result);
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("error=\"first line second 'quoted' line\""));
    }

    #[test]
    fn expands_home_paths() {
        let home = env::var_os("HOME").expect("HOME must be set");
        assert_eq!(
            expand_home("~/dotgit.log").unwrap(),
            PathBuf::from(home).join("dotgit.log")
        );
    }
}
