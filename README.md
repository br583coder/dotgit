# dotgit

A dotfile manager backed by `git` + `gh` auth. Upload files from your machine into a
git repository and commit them without ever typing a password.

`dotgit` mirrors dotfiles from your machine into a repo's working tree (paths are
preserved relative to `$HOME`), stages them, prompts for a commit message, commits,
and pushes to the platform your remote points at — authenticating with whichever CLI
you already logged into.

## Features

- **Passwordless** — never stores or asks for credentials; reuses your `gh` / `glab` /
  GitLab token logins.
- **Upload anything** — files or whole folders, including hidden/gitignored files
  (uploads are force-added).
- **Host-aware pushes** — inspects `.git/config` remote URL and routes auth correctly
  for GitHub, GitLab, SSH, or local remotes.
- **Interactive or scripted** — `-m` flag for CI/scripts, interactive prompt otherwise.
- Written in Rust with `git2` (native git operations), `clap`, `anyhow`, `thiserror`.

## Install

Requirements:

- [Rust](https://rustup.rs) toolchain (Rust 1.70+)
- `git`
- `gh` for GitHub auth (optional if you only use GitLab)
- `glab` or a `GITLAB_TOKEN` for GitLab auth (optional for GitHub)

Clone and install:

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

To update later:

```
cd ~/dotgit
git pull
cargo install --path . --force
```

## Authentication

`dotgit` never asks for your password. It reads credentials from tools you already
authenticated:

| Platform | Auth method          | Where the token comes from                          |
|----------|----------------------|-----------------------------------------------------|
| GitHub   | `gh auth login`      | `~/.config/gh/hosts.yml`                            |
| GitLab   | `glab auth login`    | `~/.config/glab-cli/config.yml`                     |
| GitLab   | `GITLAB_TOKEN`/`GL_TOKEN` env var | exported in your shell                   |
| SSH      | your ssh key/agent   | `~/.ssh/`                                           |

For GitHub, you can let dotgit drive the login:

```
dotgit login                  # = gh auth login --hostname github.com
dotgit login gitlab.company   # = gh auth login for that host
```

`dotgit login` only wraps the `gh` CLI. For GitLab, log in once with
`glab auth login` and forget about it.

## Quick start

```bash
# 1. Create a repo, e.g. via gh:
gh repo create mydotfiles --private --clone

# 2. Upload a folder from your machine into the repo
cd mydotfiles
dotgit upload ~/.config/hypr

# 3. Commit and push (you'll be prompted for a message)
dotgit commit
```

That's it. Every future edit lives at `~/.config/hypr`; upload + commit to sync.

## Commands

### `dotgit upload <paths>...`

Copies files/folders from your machine into the current repo and stages them.

```
dotgit upload ~/.config/hypr            # a folder
dotgit upload ~/.zshrc                  # a single file
dotgit upload ~/.zshrc ~/.config        # multiple / mixed
```

Path mapping:

- A path under `$HOME` (e.g. `~/.config/hypr`) is stored as `./.config/hypr` in the repo.
- Any other absolute path keeps its layout from the filesystem root.
- Relative paths resolve from your current directory.

Uploads are **force-added**, so `.gitignore`-matched files are still tracked — a
dotfiles repo should hold everything you tell it to.

### `dotgit commit`

Stages all changes, prompts for a commit message (Ctrl-D finishes multiline input),
creates the commit, and pushes to `origin`.

```
dotgit commit                 # prompt for a message, then commit + push
dotgit commit -m "update"     # non-interactive
dotgit commit --no-push       # commit locally only
```

If nothing changed, it prints `nothing to commit, working tree clean` and simply
pushes. An empty message aborts.

### `dotgit login [host]`

Runs `gh auth login --hostname <host>` (default `github.com`) so dotgit can read your
token. After a successful gh login, `dotgit` recognizes you immediately.

```
dotgit login
dotgit login github.com
```

## How pushing works

`dotgit` reads the remote from `.git/config` (`git remote get-url origin`) and routes:

| Remote URL                               | Credentials used                       |
|------------------------------------------|----------------------------------------|
| `https://github.com/me/dotfiles.git`     | gh token (pushes as your gh user)      |
| `https://gitlab.com/me/dotfiles.git`     | glab token or `GITLAB_TOKEN`           |
| `https://git.gitea.host/...`             | gh token if gh is logged into that host |
| `git@github.com:me/dotfiles.git` (SSH)   | your ssh key / agent                   |
| `/local/path/repo.git` (local)           | none (no credentials needed)           |

If gh isn't logged in for a GitHub remote, `dotgit` runs `gh auth login` for you.
If a GitLab remote has no token source, it tells you to run `glab auth login` or set
`GITLAB_TOKEN`.

## Workflow ideas

- Keep a `dotfiles` repo and re-upload a config whenever you change it:
  ```bash
  dotgit upload ~/.config/hypr && dotgit commit -m "tweak colorscheme"
  ```
- Bootstrap a new machine:
  ```bash
  git clone https://github.com/me/dotfiles.git ~/dotfiles
  cd ~/dotfiles
  dotgit commit --no-push      # won't push; clone is already up to date
  ```

## Development

```
cargo build        # debug build
cargo clippy       # lints
```

Source layout:

| File          | Contents                                              |
|---------------|-------------------------------------------------------|
| `src/main.rs` | CLI parsing, upload/copy logic, push orchestration    |
| `src/git.rs`  | git2 operations: staging, commits, remotes, push      |
| `src/gh.rs`   | gh/glab auth: token discovery from CLI config files   |
| `src/error.rs`| typed errors via thiserror                            |

## Troubleshooting

- **`GitHub CLI (gh) is required`** — `gh` isn't installed. `pacman -S github-cli`,
  `brew install gh`, or download from cli.github.com.
- **`gh is not authenticated for github.com`** — run `dotgit login` or `gh auth login`.
- **«glab is not configured»** — run `glab auth login`, or export `GITLAB_TOKEN`.
- **`set git user.name and user.email`** — git needs an identity to commit:
  ```bash
  git config --global user.name "You"
  git config --global user.email "you@example.com"
  ```
- **Push to a brand-new GitHub repo 404s** — the remote doesn't exist yet. Create it:
  ```bash
  git remote add origin https://github.com/you/dotfiles.git
  gh repo create dotfiles --private --source=. --remote=origin --push
  ```