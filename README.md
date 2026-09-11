# dotgit

A dotfile manager backed by git + gh auth. Upload files from your machine into a git repo and commit them without ever typing a password.

## Install

Clone and build:

```
git clone https://github.com/br583coder/dotgit.git
cd dotgit
cargo build --release
cargo install --path . --force
```

Verify:

```
dotgit --version
```

### Dependencies

- [Rust](https://rustup.rs) + Cargo
- [git](https://git-scm.com)
- [gh](https://cli.github.com) for GitHub auth (optional if you only use GitLab)

## Auth

dotgit never asks for your password. It uses your existing CLI logins:

- **GitHub**: `gh auth login` (then `dotgit login`, which runs gh and picks up your token)
- **GitLab**: `glab auth login`, or set `GITLAB_TOKEN` (or `GL_TOKEN`)

## Usage

```
dotgit login                          # run gh auth login for github.com
dotgit upload ~/.config/hypr          # copy files from your machine into the repo
dotgit commit                         # prompt for a message, commit, and push
dotgit commit -m "update hypr"        # skip the prompt
dotgit commit --no-push               # commit locally only
```

### upload

Copies any file or folder into the repo (mirroring the path from `$HOME`/root), then stages it. Uploads are force-added, so even gitignored files are tracked.

### commit

Stages all changes, prompts for a commit message, creates the commit, and pushes to `origin`:

- remote URL contains `github` (or gh is logged into that host) -> pushed as your gh user
- remote URL contains `gitlab` -> pushed with your glab/GITLAB_TOKEN
- SSH or local remotes -> pushed directly

## Commands

| Command                | Description                                      |
|------------------------|--------------------------------------------------|
| `dotgit upload <paths>`| Copy files/folders from your machine into the repo and stage them |
| `dotgit commit`        | Stage, commit, and push (prompts for a message)  |
| `dotgit login [host]`  | Run `gh auth login` for a host (default github.com) |