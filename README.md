# dotgit

A dotfile manager backed by `git` + `gh`/`glab` auth. Upload files from your machine
into a GitHub or GitLab repository and commit them without ever typing a password.

`dotgit` mirrors dotfiles from your machine into a repo's working tree (paths are
preserved relative to `$HOME`), stages them, prompts for a commit message, commits,
and pushes to the platform your remote points at — authenticating with whichever CLI
you already logged into. It also creates the repository for you, steps the working
tree back and forth through past versions, and keeps an offline copy of the whole
history in case the remote ever disappears.

| Command                  | What it does                                          |
|--------------------------|-------------------------------------------------------|
| `dotgit new <name>`      | create the repo on GitHub or GitLab and wire up `origin` |
| `dotgit upload <paths>`  | copy files from your machine into the repo and stage them |
| `dotgit commit`          | stage everything, commit, and push                    |
| `dotgit restore`         | step the working tree back one version                |
| `dotgit rebase`          | step the working tree forward one version             |
| `dotgit revert [commit]` | reverse an earlier commit and push the result         |
| `dotgit pull`            | destroy the newest commit, locally and on the remote   |
| `dotgit backup`          | save the full history to a local bundle file          |
| `dotgit login [host]`    | log in through `gh` or `glab`                         |

An optional full-screen browser, [`dotgit status`](#dotgit-status-optional), is available
separately. You never need it: it is a different binary, it is not built or installed
by default, and the `dotgit` command never launches it.

## Features

- **GitHub and GitLab as equals** — every command (`new`, `login`, pushing) works the
  same on either, driving `gh` or `glab` as appropriate, including self-hosted hosts.
- **Step through versions** — `dotgit restore` walks the working tree back one change
  at a time and `dotgit rebase` walks it forward again, without rewriting history.
- **Undo a bad commit for good** — `dotgit pull` destroys the newest commit locally
  and on the remote, after backing the history up so it stays recoverable.
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

- [Rust](https://rustup.rs) 1.88 or newer — the crate is edition 2024 and uses
  let-chains
- `git`
- `gh` for GitHub auth (optional if you only use GitLab)
- `glab` or a `GITLAB_TOKEN` for GitLab auth (optional for GitHub)
- `C programming language (cc)`

Clone and install:

```
git clone https://github.com/br583coder/dotgit.git
cd dotgit
cargo build --release
cargo install --path . --force --features tui
```

That installs `dotgit` and its short alias `dg`. The optional TUI is behind a feature
flag and is **not** built by this command — see
[`dotgit status`](#dotgit-status-optional) if you want it.

Verify:

```
dotgit --version
dg --version          # shorthand for dotgit
```

After installation, `dg` and `dotgit` are interchangeable commands.

To update later:

```
cd ~/dotgit
git pull
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

# 4. Optional: keep an offline copy of the history
dotgit backup
```

That's it. Every future edit lives at `~/.config/hypr`; upload + commit to sync.

## Commands

Every command below can be run with either `dotgit` or its shorter alias `dg`.

### Logging with `dotgit.toml`

Dotgit logs what you ran to a local file by default. Configuration is read from these
files in order, each overriding only the settings it actually contains:

```text
~/.config/dotgit/dotgit.toml     # your account
<repository root>/dotgit.toml    # this repository (found from any subdirectory)
./dotgit.toml                    # the directory you are standing in
```

Example:

```toml
[logging]
enabled = true
path = "~/.local/state/dotgit/dotgit.log"
```

Both settings are optional. Logging is on unless a file turns it off, and the default
path is `$XDG_DATA_HOME/dotgit/dotgit.log` (usually
`~/.local/share/dotgit/dotgit.log`). A leading `~` in `path` is expanded.

A file only changes what it mentions, so a repository config that sets `path` does not
switch logging back on if your account config set `enabled = false`. To override a
setting, write it down explicitly.

Each line is one command, as `key=value` pairs:

```text
20260918-101500 via=cli command=commit outcome=success repo=/home/you/dotfiles/
20260918-101733 via=cli command=commit outcome=failure repo=/home/you/dotfiles/ error="no git remote 'origin' configured - add one to push"
20260918-102140 via=tui command=stage outcome=success repo=/home/you/dotfiles/
```

`via` says which front end ran it: `cli` for a `dotgit` command, `tui` for an action
taken inside [`dotgit status`](#dotgit-status-optional). Actions that exist only in the
TUI log under their own names — `stage`, `unstage`, `stage-all`, `discard`, `jump`,
`backup-delete` — while the rest reuse the name of the command that does the same
thing, so `grep command=commit` finds both.

Timestamps are UTC in the same `YYYYMMDD-HHMMSS` form the [backup](#dotgit-backup)
filenames use, so a log line and a bundle from the same moment line up. Failures keep
the reason, flattened onto one line.

The log records the timestamp, front end, command name, repository path, and outcome.
It does **not** record tokens, commit messages, file contents, file paths you upload,
or command arguments.

If the config file cannot be read or parsed, dotgit says so on stderr and carries on
with defaults — a broken config never stops a command from running. Misspelled
settings are rejected rather than ignored, so `[loging]` or a stray `enabled = false`
outside any section tells you `unknown field` instead of silently doing nothing.

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

### `dotgit restore` / `dotgit rebase`

Move the working tree back and forth through the repository's versions, one commit at
a time. `dotgit restore` goes one change **older**; run it again to keep going back.
`dotgit rebase` goes one change **newer**, up to the most recent one.

```
$ dotgit restore
restored to 82c02c8 "second version" (1 version back of 2)
`dotgit rebase` steps forward, `dotgit commit` keeps this version

$ dotgit restore
restored to e20c40a "first version" (2 versions back of 2)

$ dotgit restore
oldest change reached

$ dotgit rebase
moved forward to 82c02c8 "second version" (1 version back of 2)

$ dotgit rebase
moved forward to b2afa86 "third version" (newest change)
this is the newest change

$ dotgit rebase
newest change released
```

**Nothing is ever rewritten or lost.** Stepping changes only the files in the working
tree; the branch keeps pointing at the newest commit, so every version stays
reachable and a step in the wrong direction costs nothing — step the other way and
you are back. `git log` is identical before and after.

The position is remembered in `.git/dotgit-position`, inside the git directory, so it
is never uploaded, committed or pushed.

**To keep a version you stepped back to**, commit it:

```bash
dotgit restore          # go back to the version you want
dotgit commit -m "roll back to yesterday's config"
```

That records the rollback as a new commit on top, which is why nothing is lost. The
position resets to the newest change afterwards, since the commit you just made *is*
now the newest change.

If you have uncommitted edits, stepping refuses rather than overwriting them:

```
dotgit: you have uncommitted changes - run `dotgit commit` to keep them, or
`dotgit backup` first if you are unsure
```

The check is against the version you are currently on, not the newest one, so being
stepped back never blocks the next step — only your own unsaved edits do.

Note these move files **inside the repository**. Copying them back out to `~` is a
separate step; `dotgit upload` only ever copies inward.

### `dotgit pull`

Destroys the newest commit **entirely** — locally and on the remote — for when a
commit is simply wrong and you want it gone rather than reversed.

```
$ dotgit pull
About to destroy 1 commit(s):
  91403b1  broke my waybar config
HEAD would become dc67826 "version 3"
the remote will be force-pushed, so it loses them too
Destroy them? [y/N] y
backed up first: /home/you/.local/share/dotgit/backups/dotfiles-20260915-020758.bundle
destroyed 1 commit(s); HEAD is now dc67826 "version 3"
force-pushed to github.com (the remote no longer has them)
recover with: git clone <the bundle above> <directory>
```

```
dotgit pull                  # destroy the newest commit
dotgit pull -n 3             # destroy the three newest commits
dotgit pull -y               # skip the confirmation (for scripts)
dotgit pull --no-push        # destroy locally, leave the remote alone
dotgit pull --no-backup      # skip the safety bundle
dotgit drop                  # an alias, if `pull` reads oddly to you
```

**This is the one command that deletes work on purpose**, so it is built to be hard
to regret:

- It prints exactly which commits will go, and what `HEAD` will become, **before**
  asking. Anything but an explicit `y` cancels.
- It writes a [backup bundle](#dotgit-backup) first, so every destroyed commit can be
  cloned back afterwards. `git reflog` also still holds them until git collects them.
- It refuses when you are [stepped back](#dotgit-restore--dotgit-rebase) through the
  history, where the result would be hard to predict.
- It refuses to destroy the entire history; the first commit cannot be removed this
  way.
- It warns when uncommitted changes would be destroyed along with the commits.
- The remote is force-pushed with `--force-with-lease` over SSH, which refuses if the
  remote moved in a way you have not seen.

**A word on the name.** `dotgit pull` is not `git pull` — it does not fetch anything
from the remote. It pulls a commit *out* of the history and destroys it. `dotgit drop`
is an alias for the same thing if that reads better.

If the push fails after the commits are already gone locally, dotgit says so and
leaves the local history destroyed rather than silently re-creating it. Fix the cause
(usually a login), then run `git push --force-with-lease origin <branch>` to bring the
remote into line.

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

## `dotgit status` (optional)

A full-screen browser for the repository, laid out the way lazygit lays one out: a
column of panels on the left, one of them focused, and a main pane on the right that
always shows what the focused selection means.

**It is entirely optional.** It is a separate binary, it is not built or installed
unless you ask for it, and the `dotgit` command never launches it — there is no `tui`
subcommand and nothing in the CLI depends on it. Every action it offers names its
command line equivalent in the help pane (`?`).

Build and install it explicitly:

```
cargo install --path . --force --features tui
dotgit status
```

It takes no arguments and runs in whichever repository you start it from.

```
┌ 1 status ─────────────────────────────┐┌ .zshrc ───────────────────────────────┐
│ dotfiles [main]                       ││ diff --git a/.zshrc b/.zshrc          │
│ github.com  v2 of 3  changes          ││ @@ -1,3 +1,3 @@                       │
└───────────────────────────────────────┘│ -v2                                   │
┌ 2 files ──────────────────────────────┐│ +v3                                   │
│ M  .config/waybar/config              ││  line2                                │
│  ? .vimrc                             ││ -line3                                │
│  M .zshrc                             ││ +CHANGED                              │
└───────────────────────────────────────┘│                                       │
┌ 3 versions ───────────────────────────┐│                                       │
│ > e8a8e1e tweak zshrc                 ││                                       │
│   032e34c first upload                ││                                       │
└───────────────────────────────────────┘│                                       │
┌ 4 backups ────────────────────────────┐│                                       │
│ dotfiles-20260918-023106.bundle  38K  ││                                       │
└───────────────────────────────────────┘└───────────────────────────────────────┘
─────────────────────────────────────────────────────────────────────────────────
 staged .zshrc
 space stage / unstage   a stage everything   d discard changes   c commit   u upload
 tab panel  j/k move  J/K scroll  R refresh  ? keys  q quit
```

The focused panel's border is highlighted, and **the footer lists only the keys that
apply to it** — the rest of the interface stays out of the way. `1`-`4` jump straight
to a panel, `tab` cycles.

### What each panel does

| Panel        | Main pane shows                | Keys                                                      |
|--------------|--------------------------------|-----------------------------------------------------------|
| `1` status   | a summary of the repository    | `e` edit `dotgit.toml`, `E` in `$EDITOR`, `p` push, `b` back up, `L` log in |
| `2` files    | the patch, or a committed file's contents | `e` edit, `E` in `$EDITOR`, `space` stage/unstage, `a` stage all, `d` discard, `u` upload |
| `3` versions | the patch the commit introduced| `enter` move here, `r` back one, `f` forward one, `v` revert, `D` destroy |
| `4` backups  | the bundle's path and size     | `n` new backup, `d` delete backup                          |
| `5` commit   | what is staged for the commit  | type the message, `enter` commit and push, `ctrl-l` commit only |

**The files panel lists every tracked file, not only the changed ones**, with the
changed ones first. That matters because committing a file would otherwise be the last
time you could open it here: once the tree is clean there would be nothing to select.
Committed files are dimmed, the main pane shows their **contents** (syntax highlighted)
rather than an empty diff, and `e` opens any of them. Staging or discarding one says it
has no changes rather than appearing to do something, and `a` stages only what actually
changed.

Staged files show green, partly staged yellow, unstaged red. In the versions panel the
`>` marker is the version your **working tree** holds, which is not necessarily the
newest commit.

### Scrolling

Long histories and long patches both scroll, and each pane grows a scrollbar only when
it holds more than fits:

| Keys                | What scrolls                                              |
|---------------------|-----------------------------------------------------------|
| `j` / `k`           | the focused list, one row                                  |
| `PgDn` / `PgUp`     | the focused list, one pane at a time                       |
| `g` / `G`           | first / last item in the focused list                      |
| `J` / `K`           | the diff pane, one line                                    |
| `ctrl-d` / `ctrl-u` | the diff pane, half a pane                                 |
| `Home` / `End`      | top / bottom of the diff pane                              |
| mouse wheel         | whatever is under the pointer                              |

The wheel scrolls the pane you point at, and focuses a side panel if you scroll over
one. The diff pane's title shows where you are — `lines 35-68 of 242` — and scrolling
stops when the last line reaches the bottom rather than letting the text slide out of
view. Every panel keeps its own position, so switching panels and coming back returns
you to the same place in the history.

`enter` in the versions panel moves the working tree straight to the selected version,
which the CLI can only reach by stepping (`dotgit restore` repeatedly) — the one thing
the TUI does more directly than the command line.

### Committing and pushing

The commit message is a **panel, not a pop-up**. Press `c` from anywhere and the cursor
moves to the message panel along the bottom of the window; type the message in place.
Nothing overlays the screen, so the staged files and the diff stay visible while you
write, and the draft survives leaving the panel — `Esc` goes back to the files, and `c`
brings you back to the message exactly as you left it.

| Key | Action |
|---|---|
| any character | write the message |
| `Enter` | commit what is staged **and push** |
| `ctrl-l` | commit locally, without pushing |
| `Esc` | back to the files, keeping the draft |

While the panel has focus the main pane lists **what is actually going into the
commit**, and the commit takes exactly that: the index, not the working tree. Unstaging
a file with `space` means it is left out, which is the point of having a staging area.
Committing with nothing staged says so rather than sweeping up every change, and an
empty message is refused.

**`Enter` commits and pushes**, the same as `dotgit commit` on the command line. The
push only happens if the commit succeeded, and a push that fails never hides the commit
that worked — you get `committed 3df1c6f, but the push failed: …`, with the commit still
there to push again. A repository with no remote says `committed 3df1c6f; no remote to
push to` rather than reporting an error.

Use `ctrl-l` when you want the commit without the push: an offline machine, or a repo
whose remote you have not made yet.

The whole loop therefore works from a clean checkout without leaving the TUI: `e` to
edit a committed file, `:wq` to save it, `a` to stage it, `c` to write a message, and
`Enter` to commit and push.

### Editing

`e` opens the selected file in dotgit's own modal editor — no external editor, no
`$EDITOR` required. It is built on vim's principles, so if you know vim you already
know it:

```
  1 alpha                                                    
  2 new                                                      
  3 beta                                                     
 NORMAL  notes.conf [+]                                 2,1  
```

**Modes.** You start in `NORMAL`, where letters are commands rather than text. `i`
`a` `I` `A` `o` `O` enter `INSERT`; `Esc` (or `ctrl-c`) returns to `NORMAL`, stepping
the cursor back onto the last character as vim does. `:` opens the command line.

**The cursor tells you which mode you are in**, the way neovim's does: a solid block
sitting on the character in `NORMAL`, and a thin blinking bar between characters in
`INSERT` — the VS Code style caret, so writing mode looks like writing. The status line
turns green at the same time. Your terminal's own cursor shape is restored when you
close the editor, when an action hands the screen to another program, and when you quit,
so nothing is left behind in your shell.

| Keys | What they do |
|---|---|
| `h` `j` `k` `l`, arrows | move a character or line |
| `w` `b` | forward / back a word, across line ends |
| `0` `^` `$` | start of line, first non-blank, end of line |
| `gg` `G` `{n}G` `:{n}` | first line, last line, line `n` |
| `ctrl-d` `ctrl-u` | half a screen down / up |
| `i` `a` `I` `A` | insert before / after the cursor, at the first non-blank / end of line |
| `o` `O` | open a line below / above and insert |
| `x` `D` `dd` `dw` `d$` `cc` | delete a character, to end of line, a line, a word, to end of line, change a line |
| `yy` `p` `P` | yank a line, paste below / above |
| `u` `ctrl-r` | undo, redo |
| `:w` `:q` `:q!` `:wq` `:x` | write, quit, quit discarding, write and quit |

**Syntax highlighting** comes with it, for the kinds of file a dotfiles repo holds:
TOML, INI/conf, JSON, YAML, shell, Lua, vimscript, Rust, Python and Markdown. The
language is taken from the extension, or from the name for the extension-less ones
(`.zshrc`, `.vimrc`, `.gitconfig`, `init.lua`). Comments recede into grey italics,
strings are green, numbers magenta, keywords blue, `[section]` headers bold yellow, and
the left-hand side of `key = value` cyan. Highlighting runs per visible line, so a
large file costs nothing off-screen, and a language it does not know is left plain
rather than guessed at.

**The mouse works**, as it does in neovim with `mouse=a`: click anywhere in the buffer
to put the cursor there — clicking past the end of a line lands on its last character —
and the wheel scrolls the view three lines a notch, carrying the cursor along only when
it would otherwise leave the window.

**Counts work**, as they must: `3j`, `5x`, `2dd`, `10G`. Operators wait for their
second key, so `dd`, `dw`, `yy` and `gg` behave as you would expect, and `Esc`
abandons a half-typed `2d`.

**An entire insert session is one undo step**, not one per keystroke — typing `ihello`
then `u` removes `hello`, not the `o`. And `:q` refuses to throw away unsaved changes
with vim's own `E37: No write since last change (add ! to override)`.

Writing the file re-reads the diff and the staging state behind the editor, and the
final newline is left exactly as the file had it. A file that does not exist yet opens
as an empty `[New]` buffer, so `e` on the status panel is how you write your first
`dotgit.toml`.

If you would rather use your own editor, `E` still hands the screen to `$VISUAL` /
`$EDITOR` (falling back to `vi`) and repaints when it exits.

Both edit the copy **inside the repository**, not the live file in `~`. Copying it
back out is still a separate step.

### Reverting a commit

`v` in the versions panel adds a commit that undoes the selected one, after showing
which commit it is and asking. Unlike `D`, nothing is destroyed: the original commit
stays in the history and the undo sits on top, which is the safe choice for a commit
other people have already pulled.

A revert is applied to the working tree, so it needs a clean tree on the newest
version — the same conditions as moving through the history — and says so if either is
not true. If the commit cannot be undone cleanly, because later commits changed the
same lines, dotgit reports that and leaves the repository **exactly** as it was: no
conflict markers in your files, no half-finished revert for other git tools to trip
over. Resolve that case with `git revert` by hand if you want to work through the
conflict.

Reverting does not push; press `p` when you want to. `dotgit revert [commit]` does the
same thing from the command line, and does push.

### Anything destructive asks first

`d` (discard a file), `d` in the backups panel (delete a bundle) and `D` (destroy a
commit) all open a confirmation box; only `y` proceeds, anything else cancels. `D`
takes a backup bundle before destroying anything, exactly as [`dotgit pull`](#dotgit-pull)
does, and refuses on any commit but the newest.

Committing does not push, so a commit never blocks on a login prompt; press `p` when
you want to push. Pushing and logging in leave the full-screen view first, because
either may hand over to `gh auth login`, and return when it is done.

Both front ends call the same functions in `src/ops.rs` and `src/git.rs`, so the TUI
cannot drift from the CLI: staging, stepping, uploading, committing, destroying and
backing up behave identically either way, including every guard — the TUI refuses to
move to another version with uncommitted changes for the same reason the command does.

## How pushing works## How pushing works

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

- Walk back through configs until something works again, then keep it:
  ```bash
  dotgit restore     # repeat until the config is the one that worked
  dotgit commit -m "back to the working version"
  ```
- Undo a commit you regret, keeping the option to change your mind:
  ```bash
  dotgit pull            # destroys it, after backing the history up
  ```
  Prefer `dotgit revert` when the commit is already shared with other people: it
  reverses the change without rewriting anyone else's history.
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
cargo build                  # debug build (CLI only)
cargo build --features tui   # also enables dotgit status
cargo test                   # unit tests
cargo clippy --all-targets   # lints
cargo fmt                    # formatting
```

`ratatui` is an optional dependency: without `--features tui` it is never compiled, so
the CLI stays dependency-light for anyone who does not want the TUI.

`DOTGIT_JOBS=<n>` overrides the number of copy threads (default: CPU count, capped
at 8). Setting `DOTGIT_JOBS=1` forces serial copying, which is occasionally useful
on network filesystems.

Source layout:

| File             | Contents                                           |
|------------------|-----------------------------------------------------|
| `src/main.rs`    | the `dotgit` binary: a shim over `cli::run`        |
| `src/bin/dg.rs`  | the `dg` alias: the same shim                      |
| `src/bin/dotgit-tui.rs` | the optional lazygit-style TUI for `dotgit status` (feature `tui`) |
| `src/cli.rs`     | CLI parsing, prompts and output                    |
| `src/ops.rs`     | operations shared by both front ends               |
| `src/fsops.rs`   | incremental, multi-threaded file copying           |
| `src/git.rs`     | git2 operations: staging, commits, diffs, remotes, push |
| `src/gh.rs`      | gh/glab auth: token discovery from CLI config files |
| `src/history.rs` | the version cursor behind `restore` and `rebase`   |
| `src/backup.rs`  | git bundles: writing, listing and pruning backups  |
| `src/error.rs`   | typed errors via thiserror                         |

## Troubleshooting

- **`gh is required`** — `gh` isn't installed. `pacman -S github-cli`,
  `brew install gh`, or download from cli.github.com.
- **`glab is required`** — `glab` isn't installed. `pacman -S glab`, `brew install glab`,
  or download from gitlab.com/gitlab-org/cli.
- **`gh is not authenticated for <host>`** / **`glab is not authenticated for <host>`** —
  run `dotgit login <host>`; the message names the exact command.
- **`glab is not configured`** — run `glab auth login --hostname <host>`, or export
  `GITLAB_TOKEN`.
- **`you have uncommitted changes`** on `dotgit restore` / `dotgit rebase` — stepping
  will not overwrite unsaved edits. Keep them with `dotgit commit`, or discard them
  with `git checkout -- .`, then step again.
- **`you are stepped back through the history`** on `dotgit pull` — run `dotgit rebase`
  until you reach the newest change, then destroy the commit.
- **`cannot destroy N commit(s)`** — the history is shorter than `N`, or you asked to
  destroy the first commit, which `dotgit pull` will not do.
- **`no commits yet - nothing to step through`** — `dotgit restore` needs at least one
  commit to step through.
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
