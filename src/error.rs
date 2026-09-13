use thiserror::Error;

#[derive(Debug, Error)]
pub enum DotgitError {
    #[error("not inside a git repository - clone (or init) a dotfiles repo first")]
    NotARepository,
    #[error("no git remote 'origin' configured - add one to push")]
    NoRemote,
    #[error("detached HEAD, check out a branch to commit")]
    DetachedHead,
    #[error("git operation failed: {0}")]
    Git(#[from] git2::Error),
    #[error("{0}")]
    Message(String),
    #[error("{cli} is required: {hint}")]
    CliUnavailable { cli: String, hint: String },
    #[error(
        "{cli} is not authenticated for {host}; run `dotgit login {host}` or `{cli} auth login --hostname {host}`"
    )]
    NotAuthenticated { cli: String, host: String },
}

impl From<&str> for DotgitError {
    fn from(msg: &str) -> Self {
        DotgitError::Message(msg.to_string())
    }
}

impl From<String> for DotgitError {
    fn from(msg: String) -> Self {
        DotgitError::Message(msg)
    }
}
