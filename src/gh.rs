use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Result};

use crate::error::DotgitError;

#[derive(Clone, Debug)]
pub struct HostCredentials {
    pub username: String,
    pub token: String,
}

#[derive(Clone, Debug)]
pub struct GlabHost {
    pub token: String,
}

pub fn read_glab_credentials() -> Result<GlabHost> {
    let path = glab_config_path()?;
    let yaml = fs::read_to_string(&path).map_err(|_| {
        DotgitError::from(
            "glab is not configured; run `glab auth login` (or set GITLAB_TOKEN)",
        )
    })?;
    let mut token = None;
    for raw in yaml.lines() {
        let (key, value) = match raw.trim().split_once(':') {
            Some((k, v)) => (k, v.trim().trim_matches('"')),
            None => continue,
        };
        if key == "oauth_token" {
            token = Some(value.to_string());
        }
    }
    Ok(GlabHost {
        token: token.ok_or_else(|| {
            DotgitError::from("no token found in glab config; run `glab auth login`")
        })?,
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

pub fn ensure_gh() -> Result<()> {
    match Command::new("gh").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        _ => Err(DotgitError::GhUnavailable(
            "install it from https://cli.github.com".into(),
        )
        .into()),
    }
}

pub fn is_logged_in(host: &str) -> bool {
    Command::new("gh")
        .args(["auth", "status", "--hostname", host])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn login(host: &str) -> Result<()> {
    let status = Command::new("gh")
        .args(["auth", "login", "--hostname", host])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| anyhow!("gh auth login failed: {e}"))?;
    if !status.success() {
        return Err(anyhow!("gh auth login failed"));
    }
    Ok(())
}

pub fn read_host_credentials(host: &str) -> Result<HostCredentials> {
    let path = hosts_file_path()?;
    let yaml = fs::read_to_string(&path).map_err(|_| {
        anyhow!(DotgitError::GhNotAuthenticated(host.to_string()))
    })?;
    match parse_host_block(&yaml, host) {
        Some(creds) => Ok(creds),
        None => Err(DotgitError::GhNotAuthenticated(host.to_string()).into()),
    }
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