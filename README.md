# dotgit

A dotfile manager backed by `git` + `gh`/`glab` auth. Upload files from your machine
into a GitHub or GitLab repository and commit them without ever typing a password.

`dotgit` mirrors dotfiles from your machine into a repo's working tree (paths are
preserved relative to `$HOME`), stages them, prompts for a commit message, commits,
and pushes to the platform your remote points at — authenticating with whichever CLI
you already logged into.

## Features

- **GitHub and GitLab as equals** — every command (`new`, `login`, pushing) works the
  same on either, driving `gh` or `glab` as appropriate, including self-hosted hosts.
- **Survives the remote being deleted** — `dotgit backup` writes the entire commit
  history to a single local file you can clone from with no network and no forge.
- **Passwordless** — never stores or asks for credentials; reuses your `gh` / `glab` /
  GitLab token logins.
- **Creates the repo for you** — `dotgit new <name>` asks whether you want it on
  GitHub or GitLab, asks whether it should be public or private, drives `gh`/`glab
  auth login` when needed, and wires up `origin`.
- **Revert safely** — `dotgit revert` lets you choose a recent commit (or accepts a
  revision directly), creates a normal revert commit, and pushes through the same
  GitHub/GitLab-aware authentication.
- **Upload anything** — files or whole folders, including hidden/gitignored files
  (uploads are force-added).
- **Incremental** — re-uploading a folder copies only what actually changed, so
  syncing a large config tree after a one-line edit is near-instant.
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

You can let dotgit drive either login:

```
dotgit login                    # = gh auth login --hostname github.com
dotgit login --gitlab           # = glab auth login --hostname gitlab.com
dotgit login gitlab.company.net # host says GitLab, so glab is used
dotgit login github.company.net # host says GitHub, so gh is used
```

You rarely need it: `dotgit new` and `dotgit commit` both start the right login flow
themselves when the host you are talking to has no credentials yet.

## Quick start

```bash
# 1. Create the repository (you'll be asked GitHub/GitLab and public/private)
dotgit new mydotfiles

# 2. Upload a folder from your machine into the repo
cd mydotfiles
dotgit upload ~/.config/hypr

# 3. Commit and push (you'll be prompted for a message)
dotgit commit
```

That's it. Every future edit lives at `~/.config/hypr`; upload + commit to sync.

## Commands

### `dotgit new <name>`

Creates a repository on GitHub or GitLab and sets it up locally. With no `--github`
or `--gitlab` flag it asks which one you want, then asks whether the repository
should be public or private:

```
$ dotgit new mydotfiles
Where should this repository live? [1] GitHub  [2] GitLab: 1
Repository visibility? [1] Public  [2] Private: 2
created GitHub repository mydotfiles (private)
origin -> https://github.com/octocat/mydotfiles
local repository: /home/you/mydotfiles
next: cd /home/you/mydotfiles && dotgit upload ~/.config/... && dotgit commit
```

The prompt accepts `1`/`github`/`gh` or `2`/`gitlab`/`glab`.

Before creating anything it checks that the right CLI is installed and logged in,
and runs `gh auth login` / `glab auth login` for you if it is not — so a failed
login never leaves a half-made repository behind.

```
dotgit new mydotfiles                    # ask which forge
dotgit new mydotfiles --github --private  # skip both prompts
dotgit new mydotfiles --gitlab --public  # public GitLab project
dotgit new me/mydotfiles --gitlab        # create under a namespace/group
dotgit new dots --github --host github.company.com
```

Use `--public` or `--private` to skip the visibility prompt. A name with a namespace
(`me/dots`) creates the local directory as `dots`.

Where the local repository ends up:

- If you run it inside a git repository that has **no `origin`**, it attaches the new
  remote to that repository.
- Otherwise it creates `./<name>` with `git init` (on branch `main`) and points
  `origin` at the new remote.

If `./<name>` already exists, dotgit stops and tells you the remote URL to attach
yourself rather than touching the existing directory.

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

Uploads are incremental: a destination file with the same size and permissions that
is newer than its source is left alone, and the summary line reports what happened.

```
$ dotgit upload ~/.config
uploaded .config
staged for commit (3 copied, 1841 unchanged, 48.2 KiB)
```

Broken symlinks are reported and skipped rather than aborting the upload. Symlinks
that resolve are stored as their contents, and file permissions are preserved.

`dotgit upload` works from any directory inside the repository, not just its root.

### `dotgit commit`

Stages all changes, prompts for a commit message (Ctrl-D finishes multiline input),
creates the commit, and pushes to `origin`.

```
dotgit commit                 # prompt for a message, then commit + push
dotgit commit -m "update"     # non-interactive
dotgit commit --no-push       # commit locally only
```

It stages exactly what `git add -A` would (new, modified and deleted files) and
prints the new commit:

```
committed 3df1c6f on master
pushed to github.com as octocat
```

If nothing changed, it prints `nothing to commit, working tree clean` and simply
pushes. An empty message aborts.

### `dotgit revert [commit]`

Creates a new commit that reverses an earlier commit, then pushes it to the
repository's GitHub or GitLab remote. Without a revision, dotgit lists the ten most
recent commits and lets you choose one:

```
dotgit revert              # choose from recent commits
dotgit revert HEAD~1       # revert a specific revision
```

If the revert conflicts, dotgit leaves the conflict in the working tree and tells
you to resolve and commit it manually.

### `dotgit backup`

Writes every commit, branch and tag to a single `.bundle` file on your machine, so
the history survives the GitHub/GitLab repository being deleted, made private, locked
out, or lost with the account.

```
$ dotgit backup
saved /home/you/.local/share/dotgit/backups/dotfiles-20260912-183000.bundle (36.3 KiB)
restore with: git clone /home/you/.local/share/dotgit/backups/dotfiles-20260912-183000.bundle <directory>
```

```
dotgit backup                      # default location, below
dotgit backup --to /mnt/usb        # a directory: names the file for you
dotgit backup --to ~/dots.bundle   # an exact file path
dotgit backup --list               # what you already have
dotgit backup --keep 5             # back up, then keep only the 5 newest
```

Backups default to `$XDG_DATA_HOME/dotgit/backups` (usually
`~/.local/share/dotgit/backups`) — deliberately **outside** the repository, so
deleting or re-cloning the repo never takes the backups with it.

Files are named `<repo>-<UTC timestamp>.bundle`, and `--keep` only ever prunes
backups of the repository you ran it in; another repository's backups in the same
directory are left alone, as is any bundle you named yourself.

Every bundle is verified with `git bundle verify` immediately after being written, so
a corrupt backup is an error now rather than a surprise on the day you need it.

**Restoring** needs nothing but git and the file:

```bash
git clone ~/.local/share/dotgit/backups/dotfiles-20260912-183000.bundle dotfiles
cd dotfiles
git log            # full history, every commit
```

If the remote is gone for good, create a fresh one and push the recovered history:

```bash
dotgit new dotfiles      # attaches origin to the repo you are standing in
dotgit commit -m "restore from backup"
```

A bundle is a normal git remote, so you can also `git fetch` from one into an
existing repository instead of cloning.

To make it automatic, run it from cron or a systemd timer:

```bash
0 20 * * * cd ~/dotfiles && dotgit backup --keep 14
```

### `dotgit login [host]`

Runs `gh auth login` or `glab auth login` for `<host>` so dotgit can read your token.
The host itself decides which CLI is used — a `gitlab.*` host goes to `glab`, a
`github.*` host to `gh` — and `--github` / `--gitlab` override that.

```
dotgit login                    # gh, github.com
dotgit login --gitlab           # glab, gitlab.com
dotgit login gitlab.company.net # glab, self-hosted GitLab
dotgit login git.example.com    # gh (GitHub Enterprise is the fallback)
```

A host that names neither forge falls back to `gh`, which is where a GitHub
Enterprise login lives.

## How pushing works

`dotgit` reads the remote from `.git/config` (`git remote get-url origin`) and routes:

| Remote URL                               | Credentials used                       |
|------------------------------------------|----------------------------------------|
| `https://github.com/me/dotfiles.git`     | gh token (pushes as your gh user)      |
| `https://gitlab.com/me/dotfiles.git`     | glab token or `GITLAB_TOKEN`           |
| `https://gitlab.company.net/me/dots.git` | glab token for that host               |
| `https://git.gitea.host/...`             | gh token if gh is logged into that host |
| `git@github.com:me/dotfiles.git` (SSH)   | your ssh key / agent                   |
| `/local/path/repo.git` (local)           | none (no credentials needed)           |

If the relevant CLI isn't logged in for the remote's host, `dotgit` starts that CLI's
login flow for you — `gh auth login` for GitHub, `glab auth login` for GitLab — and
then pushes. `GITLAB_TOKEN` / `GL_TOKEN` take priority over the `glab` login, so CI
can push without an interactive session.

Tokens are read per host: a login for `gitlab.com` is never used against
`gitlab.company.net`, and vice versa.

## Workflow ideas

- Take a local snapshot before anything risky (a force-push, a history rewrite, or
  deleting a repo): `dotgit backup` first, and the old history is still on disk.
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
cargo test         # unit tests
cargo clippy       # lints
cargo fmt          # formatting
```

`DOTGIT_JOBS=<n>` overrides the number of copy threads (default: CPU count, capped
at 8). Setting `DOTGIT_JOBS=1` forces serial copying, which is occasionally useful
on network filesystems.

Source layout:

| File          | Contents                                              |
|---------------|-------------------------------------------------------|
| `src/main.rs` | CLI parsing, path mapping, push orchestration        |
| `src/fsops.rs`| incremental, multi-threaded file copying             |
| `src/git.rs`  | git2 operations: staging, commits, remotes, push      |
| `src/gh.rs`   | gh/glab auth: token discovery from CLI config files   |
| `src/error.rs`| typed errors via thiserror                            |

## Troubleshooting

- **`gh is required`** — `gh` isn't installed. `pacman -S github-cli`,
  `brew install gh`, or download from cli.github.com.
- **`glab is required`** — `glab` isn't installed. `pacman -S glab`, `brew install glab`,
  or download from gitlab.com/gitlab-org/cli.
- **`gh is not authenticated for <host>`** / **`glab is not authenticated for <host>`** —
  run `dotgit login <host>`; the message names the exact command.
- **`glab is not configured`** — run `glab auth login --hostname <host>`, or export
  `GITLAB_TOKEN`.
- **`nothing to back up yet - make a commit first`** — a bundle needs at least one
  commit; `git bundle` cannot record an empty repository.
- **`set git user.name and user.email`** — git needs an identity to commit:
  ```bash
  git config --global user.name "You"
  git config --global user.email "you@example.com"
  ```
- **Push to a brand-new repo 404s** — the remote doesn't exist yet. Let dotgit make
  it, from inside the repository (it attaches `origin` when there isn't one):
  ```bash
  dotgit new dotfiles
  ```

## Performance

Uploading and committing a 20,000-file, 79 MB tree on a warm page cache:

| Operation                       | Time   |
|---------------------------------|--------|
| First upload (copy + stage)     | ~3.6 s |
| Re-upload, nothing changed      | ~0.14 s |
| Re-upload after editing 1 file  | ~0.14 s |
| `dotgit commit`                 | ~0.28 s |

The first upload is dominated by git hashing and compressing every file into the
object database, which is the same work `git add` does. Everything after that is
incremental: unchanged files are neither copied nor re-hashed. A typical dotfiles
repo is a few megabytes, where every operation is well under 100 ms.
