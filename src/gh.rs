use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};

use crate::error::DotgitError;

#[derive(Clone, Debug)]
pub struct HostCredentials {
    pub username: String,
    pub token: String,
}

/// Read the token `glab auth login` stored for `host`.
pub fn read_glab_credentials(host: &str) -> Result<HostCredentials> {
    let path = glab_config_path()?;
    let yaml = fs::read_to_string(&path).map_err(|_| {
        DotgitError::from(format!(
            "glab is not configured; run `glab auth login --hostname {host}` (or set GITLAB_TOKEN)"
        ))
    })?;
    parse_glab_host_block(&yaml, host)
        .ok_or_else(|| DotgitError::NotAuthenticated {
            cli: "glab".into(),
            host: host.to_string(),
        })
        .map_err(Into::into)
}

/// Pull one host out of glab's `config.yml`, which nests hosts under a
/// top-level `hosts:` key:
///
/// ```yaml
/// hosts:
///     gitlab.com:
///         token: glpat-xxx
///         user: tanuki
/// ```
///
/// glab has spelled the key both `token` and `oauth_token` across versions, so
/// accept either. Indentation decides which block a key belongs to, so a token
/// can never leak from one host to another.
fn parse_glab_host_block(yaml: &str, host: &str) -> Option<HostCredentials> {
    let mut in_hosts = false;
    let mut hosts_indent = 0;
    let mut current_host: Option<(String, usize)> = None;
    let mut user = None;
    let mut token = None;

    for raw in yaml.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        let (key, value) = match trimmed.split_once(':') {
            Some((k, v)) => (k.trim(), v.trim().trim_matches('"')),
            None => continue,
        };

        if !in_hosts {
            if key == "hosts" && value.is_empty() {
                in_hosts = true;
                hosts_indent = indent;
            }
            continue;
        }
        // Dedenting back to (or past) `hosts:` ends the hosts section.
        if indent <= hosts_indent {
            in_hosts = false;
            current_host = None;
            continue;
        }
        match &current_host {
            // A host entry is the first level inside `hosts:`; anything deeper
            // is one of its settings.
            Some((_, host_indent)) if indent > *host_indent => {
                if current_host.as_ref().map(|(h, _)| h.as_str()) == Some(host) {
                    match key {
                        "user" | "username" => user = Some(value.to_string()),
                        "token" | "oauth_token" => token = Some(value.to_string()),
                        _ => {}
                    }
                }
            }
            _ => current_host = Some((key.to_string(), indent)),
        }
    }

    Some(HostCredentials {
        // GitLab accepts any username when the password is a token, but
        // `oauth2` is the documented one.
        username: user.unwrap_or_else(|| "oauth2".to_string()),
        token: token?,
    })
}

fn glab_config_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .ok_or_else(|| anyhow!("cannot determine your config directory"))?;
    Ok(base.join("glab-cli").join("config.yml"))
}

/// Hosting providers `dotgit new` can create a repository on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Forge {
    /// GitHub is the default when nothing else names a forge.
    #[default]
    GitHub,
    GitLab,
}

impl Forge {
    pub fn cli(self) -> &'static str {
        match self {
            Forge::GitHub => "gh",
            Forge::GitLab => "glab",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
        }
    }

    pub fn default_host(self) -> &'static str {
        match self {
            Forge::GitHub => "github.com",
            Forge::GitLab => "gitlab.com",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Forge::GitHub => "install it from https://cli.github.com",
            Forge::GitLab => "install it from https://gitlab.com/gitlab-org/cli",
        }
    }
}

/// Which forge a host belongs to, matched on the host label rather than any
/// substring: `gitlab.example.com` is GitLab but `my-gitlab-mirror.example.com`
/// should not be assumed to be.
pub fn forge_for_host(host: &str) -> Option<Forge> {
    host.split('.').find_map(|label| match label {
        "github" => Some(Forge::GitHub),
        "gitlab" => Some(Forge::GitLab),
        _ => None,
    })
}

/// Both CLIs answer `--version`, so one check covers either forge.
pub fn ensure_cli(forge: Forge) -> Result<()> {
    match Command::new(forge.cli()).arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(DotgitError::CliUnavailable {
            cli: forge.cli().to_string(),
            hint: forge.install_hint().to_string(),
        }
        .into()),
    }
}

/// `gh auth status` and `glab auth status` take the same `--hostname` flag and
/// both exit non-zero when the host has no usable credentials.
pub fn forge_is_logged_in(forge: Forge, host: &str) -> bool {
    Command::new(forge.cli())
        .args(["auth", "status", "--hostname", host])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Hand the terminal to `gh auth login` / `glab auth login` so the user can go
/// through the browser or token flow interactively.
pub fn forge_login(forge: Forge, host: &str) -> Result<()> {
    let cli = forge.cli();
    let status = Command::new(cli)
        .args(["auth", "login", "--hostname", host])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| anyhow!("{cli} auth login failed: {e}"))?;
    if !status.success() {
        return Err(anyhow!("{cli} auth login failed"));
    }
    Ok(())
}

/// Make sure the forge CLI exists and is authenticated for `host`, prompting
/// through its own login flow when it is not.
pub fn ensure_forge_auth(forge: Forge, host: &str) -> Result<()> {
    ensure_cli(forge)?;
    if !forge_is_logged_in(forge, host) {
        println!(
            "not logged in to {host}; starting `{} auth login`",
            forge.cli()
        );
        forge_login(forge, host)?;
    }
    Ok(())
}

/// Create `name` on the forge and return the clone URL it reports.
pub fn create_repo(forge: Forge, host: &str, name: &str, private: bool) -> Result<String> {
    let visibility = if private { "--private" } else { "--public" };
    let mut command = Command::new(forge.cli());
    command.args(["repo", "create", name, visibility]);
    if forge == Forge::GitHub {
        // glab reads its host from the auth config; gh needs it in the env.
        command.env("GH_HOST", host);
    }
    let output = command
        .output()
        .map_err(|e| anyhow!("{} repo create failed: {e}", forge.cli()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(anyhow!(
            "{} repo create failed{}",
            forge.cli(),
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    // gh prints the URL alone; glab wraps it in a success sentence, so scan
    // both streams for the first URL rather than trusting the layout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    extract_url(&stdout)
        .or_else(|| extract_url(&stderr))
        .ok_or_else(|| {
            anyhow!(
                "{} repo create did not report a repository URL",
                forge.cli()
            )
        })
}

/// Pull the first http(s) URL out of CLI chatter, dropping the punctuation
/// that a sentence may leave hanging off the end.
fn extract_url(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|word| word.starts_with("https://") || word.starts_with("http://"))
        .map(|word| {
            word.trim_end_matches(['.', ',', ')', '"', '\''])
                .to_string()
        })
}

/// Read the token `gh auth login` stored for `host`.
pub fn read_host_credentials(host: &str) -> Result<HostCredentials> {
    let not_authenticated = || DotgitError::NotAuthenticated {
        cli: "gh".into(),
        host: host.to_string(),
    };
    let path = hosts_file_path()?;
    let yaml = fs::read_to_string(&path).map_err(|_| not_authenticated())?;
    parse_host_block(&yaml, host).ok_or_else(|| not_authenticated().into())
}

fn hosts_file_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .ok_or_else(|| anyhow!("cannot determine your config directory"))?;
    Ok(base.join("gh").join("hosts.yml"))
}

fn parse_host_block(yaml: &str, host: &str) -> Option<HostCredentials> {
    let mut current: Option<String> = None;
    let mut user: Option<String> = None;
    let mut token: Option<String> = None;
    for raw in yaml.lines() {
        if !raw.starts_with(' ') && raw.trim_end().ends_with(':') {
            current = Some(raw.trim_end_matches(':').trim().to_string());
            continue;
        }
        if current.as_deref() != Some(host) {
            continue;
        }
        let (key, value) = match raw.trim().split_once(':') {
            Some((k, v)) => (k, v.trim().trim_matches('"')),
            None => continue,
        };
        match key {
            "user" => user = Some(value.to_string()),
            "oauth_token" => token = Some(value.to_string()),
            _ => {}
        }
    }
    Some(HostCredentials {
        username: user.unwrap_or_else(|| "git".to_string()),
        token: token?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &str = "\
github.com:
    user: octocat
    oauth_token: gho_github_token
    git_protocol: https
gitlab.example.com:
    user: tanuki
    oauth_token: glpat_other_token
";

    #[test]
    fn reads_the_requested_host_block() {
        let creds = parse_host_block(HOSTS, "github.com").unwrap();
        assert_eq!(creds.username, "octocat");
        assert_eq!(creds.token, "gho_github_token");
    }

    #[test]
    fn does_not_leak_tokens_between_host_blocks() {
        let creds = parse_host_block(HOSTS, "gitlab.example.com").unwrap();
        assert_eq!(creds.username, "tanuki");
        assert_eq!(creds.token, "glpat_other_token");
    }

    const GLAB_CONFIG: &str = "\
git_protocol: ssh
editor: nvim
hosts:
    gitlab.com:
        api_host: gitlab.com
        token: glpat_dot_com
        user: tanuki
    gitlab.company.net:
        token: glpat_company
        username: worker
";

    #[test]
    fn reads_the_requested_glab_host() {
        let creds = parse_glab_host_block(GLAB_CONFIG, "gitlab.com").unwrap();
        assert_eq!(creds.username, "tanuki");
        assert_eq!(creds.token, "glpat_dot_com");
    }

    #[test]
    fn does_not_leak_tokens_between_glab_hosts() {
        let creds = parse_glab_host_block(GLAB_CONFIG, "gitlab.company.net").unwrap();
        assert_eq!(creds.username, "worker");
        assert_eq!(creds.token, "glpat_company");
        assert!(parse_glab_host_block(GLAB_CONFIG, "gitlab.absent.net").is_none());
    }

    #[test]
    fn accepts_the_older_oauth_token_spelling() {
        let yaml = "hosts:\n  gitlab.com:\n    oauth_token: legacy\n";
        let creds = parse_glab_host_block(yaml, "gitlab.com").unwrap();
        assert_eq!(creds.token, "legacy");
        // With no user recorded, GitLab's token username is used.
        assert_eq!(creds.username, "oauth2");
    }

    #[test]
    fn ignores_top_level_keys_that_share_a_name_with_host_settings() {
        // A stray top-level `token:` outside `hosts:` must not be picked up.
        let yaml = "token: not_a_host_token\nhosts:\n  gitlab.com:\n    token: real\n";
        assert_eq!(
            parse_glab_host_block(yaml, "gitlab.com").unwrap().token,
            "real"
        );
    }

    #[test]
    fn maps_hosts_to_forges_by_label() {
        assert_eq!(forge_for_host("gitlab.com"), Some(Forge::GitLab));
        assert_eq!(forge_for_host("github.com"), Some(Forge::GitHub));
        assert_eq!(forge_for_host("codeberg.org"), None);
    }

    #[test]
    fn pulls_the_repository_url_out_of_cli_output() {
        // gh prints just the URL.
        assert_eq!(
            extract_url("https://github.com/octocat/dots\n").as_deref(),
            Some("https://github.com/octocat/dots")
        );
        // glab wraps it in a sentence that ends in a full stop.
        assert_eq!(
            extract_url(
                "✓ Created repository tanuki/dots on GitLab: https://gitlab.com/tanuki/dots."
            )
            .as_deref(),
            Some("https://gitlab.com/tanuki/dots")
        );
        assert!(extract_url("nothing to see here").is_none());
    }

    #[test]
    fn returns_nothing_for_an_unknown_host() {
        assert!(parse_host_block(HOSTS, "codeberg.org").is_none());
    }
}
