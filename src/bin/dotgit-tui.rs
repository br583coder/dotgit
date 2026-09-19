//! `dotgit-tui`: an optional full-screen browser for a dotfiles repository,
//! arranged the way lazygit arranges one.
//!
//! The left column holds four panels - status, files, versions and backups -
//! and one of them has focus at a time. The right pane always shows what the
//! focused panel's selection means: a file's patch, a version's patch, a
//! bundle's details. Keys act on the focused panel, and the footer lists the
//! ones that apply right now rather than every key that exists.
//!
//! Everything here calls the same functions the `dotgit` command calls, so the
//! two can never disagree. The TUI stays entirely optional: a separate binary
//! behind a non-default feature, never launched by `dotgit`, and every action
//! names its command line equivalent in the help pane.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use git2::Repository;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use dotgit::{backup, git, history, ops};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = reject_arguments(&args) {
        eprintln!("dotgit-tui: {err:#}");
        std::process::exit(2);
    }

    let mut terminal = ratatui::init();
    let result = App::new().and_then(|mut app| app.run(&mut terminal));
    ratatui::restore();
    if let Err(err) = result {
        eprintln!("dotgit-tui: {err:#}");
        std::process::exit(1);
    }
}

/// The four side panels, in the order they appear and in the order the number
/// keys select them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Status,
    Files,
    Versions,
    Backups,
}

impl Panel {
    const ALL: [Panel; 4] = [Panel::Status, Panel::Files, Panel::Versions, Panel::Backups];

    fn title(self) -> &'static str {
        match self {
            Panel::Status => " 1 status ",
            Panel::Files => " 2 files ",
            Panel::Versions => " 3 versions ",
            Panel::Backups => " 4 backups ",
        }
    }

    fn next(self) -> Panel {
        let index = Panel::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Panel::ALL[(index + 1) % Panel::ALL.len()]
    }

    fn previous(self) -> Panel {
        let index = Panel::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Panel::ALL[(index + Panel::ALL.len() - 1) % Panel::ALL.len()]
    }

    /// The keys that do something in this panel, for the footer and the help
    /// pane. Each carries the command that does the same thing, so the TUI
    /// keeps teaching the CLI rather than replacing it.
    fn keys(self) -> &'static [(&'static str, &'static str, &'static str)] {
        match self {
            Panel::Status => &[
                ("p", "push", "dotgit commit"),
                ("b", "back up history", "dotgit backup"),
                ("L", "log in to the remote", "dotgit login"),
            ],
            Panel::Files => &[
                ("space", "stage / unstage", "dotgit upload stages for you"),
                ("a", "stage everything", ""),
                ("d", "discard changes", ""),
                ("c", "commit staged work", "dotgit commit --no-push"),
                ("u", "upload a path", "dotgit upload <path>"),
            ],
            Panel::Versions => &[
                ("enter", "move here", ""),
                ("r", "back one version", "dotgit restore"),
                ("f", "forward one version", "dotgit rebase"),
                ("D", "destroy this commit", "dotgit pull"),
                ("v", "revert this commit", "dotgit revert <sha>"),
            ],
            Panel::Backups => &[
                ("n", "new backup", "dotgit backup"),
                ("d", "delete backup", ""),
            ],
        }
    }
}

struct CommitRow {
    oid: git2::Oid,
    id: String,
    subject: String,
    author: String,
    date: String,
}

struct BackupRow {
    path: PathBuf,
    name: String,
    bytes: u64,
}

/// A question waiting on a yes, and what to do if it gets one. Destructive
/// actions all go through here, the way lazygit confirms before it rewrites
/// anything.
enum Pending {
    DestroyCommit,
    DiscardFile(String, bool),
    DeleteBackup(PathBuf),
}

enum Mode {
    Browse,
    Input { kind: Input, buffer: String },
    Confirm { question: String, action: Pending },
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    CommitMessage,
    UploadPath,
}

impl Input {
    fn title(self) -> &'static str {
        match self {
            Input::CommitMessage => " commit message ",
            Input::UploadPath => " path to upload ",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Input::CommitMessage => "Enter commits (locally; press p to push)   Esc cancels",
            Input::UploadPath => "e.g. ~/.config/hypr    Enter uploads   Esc cancels",
        }
    }
}

struct App {
    repo: Repository,
    root: PathBuf,
    branch: String,
    remote: String,
    focus: Panel,
    files: Vec<git::FileStatus>,
    commits: Vec<CommitRow>,
    backups: Vec<BackupRow>,
    /// Selection per panel, indexed the way [`Panel::ALL`] is ordered.
    selected: [usize; 4],
    position: usize,
    dirty: bool,
    /// The right-hand pane: a title and the lines beneath it.
    main_title: String,
    main_lines: Vec<String>,
    scroll: usize,
    message: String,
    mode: Mode,
    quit: bool,
}

impl App {
    fn new() -> Result<Self> {
        let repo = git::open_repo()?;
        let root = git::repo_workdir(&repo)?;
        let mut app = App {
            repo,
            root,
            branch: String::new(),
            remote: String::new(),
            focus: Panel::Files,
            files: Vec::new(),
            commits: Vec::new(),
            backups: Vec::new(),
            selected: [0; 4],
            position: 0,
            dirty: false,
            main_title: String::new(),
            main_lines: Vec::new(),
            scroll: 0,
            message: "? for keys, tab to change panel, q to quit".into(),
            mode: Mode::Browse,
            quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    fn slot(&self) -> usize {
        Panel::ALL
            .iter()
            .position(|p| *p == self.focus)
            .unwrap_or(0)
    }

    fn selection(&self) -> usize {
        self.selected[self.slot()]
    }

    /// How many rows the focused panel has, so selection can be clamped.
    fn rows(&self) -> usize {
        match self.focus {
            Panel::Status => 0,
            Panel::Files => self.files.len(),
            Panel::Versions => self.commits.len(),
            Panel::Backups => self.backups.len(),
        }
    }

    /// Re-read every panel's contents. Called after each action, so the screen
    /// can never show a stale index, version or backup.
    fn refresh(&mut self) -> Result<()> {
        self.branch = git::current_branch(&self.repo).unwrap_or_else(|_| "(detached)".into());
        self.remote = git::remote_url(&self.repo)
            .ok()
            .and_then(|url| git::host_of(&url))
            .unwrap_or_else(|| "(no remote)".into());

        self.files = git::status_entries(&self.repo).unwrap_or_default();

        self.commits = match history::chain(&self.repo) {
            Ok(chain) => chain
                .iter()
                .map(|oid| self.describe(*oid))
                .collect::<Result<Vec<_>>>()?,
            // An empty repository has no history to list yet; every other pane
            // still works, so this is not an error.
            Err(_) => Vec::new(),
        };

        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        self.backups = backup::default_dir()
            .and_then(|dir| backup::list(&dir))
            .map(|list| {
                list.into_iter()
                    .filter(|b| b.repo.as_deref() == Some(name.as_str()))
                    .map(|b| BackupRow {
                        name: b
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        path: b.path,
                        bytes: b.bytes,
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Ok(position) = history::current(&self.repo) {
            self.position = position.index;
            self.dirty = git::has_changes_against(&self.repo, position.oid).unwrap_or(false);
        }

        // Keep every panel's selection inside its (possibly shrunken) list.
        for (slot, panel) in Panel::ALL.iter().enumerate() {
            let rows = match panel {
                Panel::Status => 0,
                Panel::Files => self.files.len(),
                Panel::Versions => self.commits.len(),
                Panel::Backups => self.backups.len(),
            };
            self.selected[slot] = self.selected[slot].min(rows.saturating_sub(1));
        }
        self.load_main();
        Ok(())
    }

    fn describe(&self, oid: git2::Oid) -> Result<CommitRow> {
        let commit = self.repo.find_commit(oid)?;
        let (id, subject) = git::describe(&self.repo, oid)?;
        Ok(CommitRow {
            oid,
            id,
            subject,
            author: commit.author().name().unwrap_or("unknown").to_string(),
            date: backup::format_timestamp(commit.time().seconds().max(0) as u64),
        })
    }

    /// Fill the right-hand pane from the focused panel's selection. This is the
    /// part that makes the layout feel like lazygit: the main view is always
    /// about whatever the cursor is on.
    fn load_main(&mut self) {
        self.scroll = 0;
        let index = self.selection();
        match self.focus {
            Panel::Status => {
                self.main_title = " repository ".into();
                self.main_lines = self.status_summary();
            }
            Panel::Files => match self.files.get(index) {
                None => {
                    self.main_title = " diff ".into();
                    self.main_lines = vec!["nothing changed".into()];
                }
                Some(file) => {
                    self.main_title = format!(" {} ", file.path);
                    self.main_lines = git::diff_for_path(&self.repo, &file.path)
                        .unwrap_or_else(|err| vec![format!("cannot diff: {err:#}")]);
                    if self.main_lines.is_empty() {
                        self.main_lines = vec!["no textual changes".into()];
                    }
                }
            },
            Panel::Versions => match self.commits.get(index) {
                None => {
                    self.main_title = " version ".into();
                    self.main_lines = vec!["no commits yet".into()];
                }
                Some(commit) => {
                    self.main_title = format!(" {} ", commit.id);
                    let mut lines = vec![
                        commit.subject.clone(),
                        String::new(),
                        format!("commit  {}", commit.oid),
                        format!("author  {}", commit.author),
                        format!("date    {} UTC", commit.date),
                        String::new(),
                    ];
                    lines.extend(
                        git::diff_for_commit(&self.repo, commit.oid)
                            .unwrap_or_else(|err| vec![format!("cannot diff: {err:#}")]),
                    );
                    self.main_lines = lines;
                }
            },
            Panel::Backups => match self.backups.get(index) {
                None => {
                    self.main_title = " backups ".into();
                    self.main_lines = vec![
                        "no backups of this repository yet".into(),
                        String::new(),
                        "press n to write one (same as `dotgit backup`)".into(),
                    ];
                }
                Some(row) => {
                    self.main_title = format!(" {} ", row.name);
                    self.main_lines = vec![
                        format!("path   {}", row.path.display()),
                        format!("size   {}", ops::human_bytes(row.bytes)),
                        String::new(),
                        "A bundle holds every commit, branch and tag.".into(),
                        "Restore it with:".into(),
                        format!("  git clone {} <directory>", row.path.display()),
                    ];
                }
            },
        }
    }

    fn status_summary(&self) -> Vec<String> {
        let version = if self.commits.is_empty() {
            "no commits".to_string()
        } else if self.position == 0 {
            format!("newest of {}", self.commits.len())
        } else {
            format!(
                "version {} of {} ({} back)",
                self.commits.len() - self.position,
                self.commits.len(),
                self.position
            )
        };
        let staged = self.files.iter().filter(|f| f.staged).count();
        let unstaged = self.files.iter().filter(|f| f.unstaged).count();
        vec![
            format!("path      {}", self.root.display()),
            format!("branch    {}", self.branch),
            format!("remote    {}", self.remote),
            format!("version   {version}"),
            format!("staged    {staged} file(s)"),
            format!("unstaged  {unstaged} file(s)"),
            format!("backups   {}", self.backups.len()),
            String::new(),
            if self.dirty {
                "The working tree differs from the version you are on.".into()
            } else {
                "The working tree matches the version you are on.".into()
            },
        ]
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.quit = true;
                continue;
            }
            match &self.mode {
                Mode::Help => self.mode = Mode::Browse,
                Mode::Confirm { .. } => self.confirm_key(key.code)?,
                Mode::Input { kind, buffer } => {
                    let (kind, buffer) = (*kind, buffer.clone());
                    self.input_key(key.code, kind, buffer)?;
                }
                Mode::Browse => self.browse_key(key.code, terminal)?,
            }
        }
        Ok(())
    }

    fn browse_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Tab => self.focus(self.focus.next()),
            KeyCode::BackTab => self.focus(self.focus.previous()),
            KeyCode::Char('1') => self.focus(Panel::Status),
            KeyCode::Char('2') => self.focus(Panel::Files),
            KeyCode::Char('3') => self.focus(Panel::Versions),
            KeyCode::Char('4') => self.focus(Panel::Backups),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => self.set_selection(0),
            KeyCode::Char('G') => self.set_selection(self.rows().saturating_sub(1)),
            // The main pane scrolls with shifted keys, so a long patch can be
            // read without leaving the list.
            KeyCode::Char('J') | KeyCode::PageDown => self.scroll_main(5),
            KeyCode::Char('K') | KeyCode::PageUp => self.scroll_main(-5),
            KeyCode::Char('R') => {
                self.refresh()?;
                self.message = "refreshed".into();
            }
            _ => self.panel_key(code, terminal)?,
        }
        Ok(())
    }

    /// Keys that mean different things depending on which panel has focus.
    fn panel_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        match (self.focus, code) {
            (_, KeyCode::Char('p')) => self.push(terminal)?,
            (_, KeyCode::Char('b')) | (Panel::Backups, KeyCode::Char('n')) => self.backup()?,
            (Panel::Status, KeyCode::Char('L')) => self.login(terminal)?,

            (Panel::Files, KeyCode::Char(' ')) => self.toggle_stage()?,
            (Panel::Files, KeyCode::Char('a')) => self.stage_all()?,
            (Panel::Files, KeyCode::Char('c')) => self.ask(Input::CommitMessage),
            (Panel::Files, KeyCode::Char('u')) => self.ask(Input::UploadPath),
            (Panel::Files, KeyCode::Char('d')) => self.ask_discard(),

            (Panel::Versions, KeyCode::Enter) => self.jump()?,
            (Panel::Versions, KeyCode::Char('r')) => self.step(history::Direction::Older)?,
            (Panel::Versions, KeyCode::Char('f')) => self.step(history::Direction::Newer)?,
            (Panel::Versions, KeyCode::Char('D')) => self.ask_destroy()?,
            (Panel::Versions, KeyCode::Char('v')) => self.revert()?,

            (Panel::Backups, KeyCode::Char('d')) => self.ask_delete_backup(),
            _ => {}
        }
        Ok(())
    }

    fn focus(&mut self, panel: Panel) {
        self.focus = panel;
        self.load_main();
    }

    fn move_selection(&mut self, delta: isize) {
        let rows = self.rows();
        if rows == 0 {
            return;
        }
        let current = self.selection() as isize;
        let next = (current + delta).clamp(0, rows as isize - 1) as usize;
        self.set_selection(next);
    }

    fn set_selection(&mut self, index: usize) {
        let slot = self.slot();
        self.selected[slot] = index;
        self.load_main();
    }

    fn scroll_main(&mut self, delta: isize) {
        let max = self.main_lines.len().saturating_sub(1);
        let next = (self.scroll as isize + delta).clamp(0, max as isize);
        self.scroll = next as usize;
    }

    fn report(&mut self, outcome: Result<String>) {
        self.message = match outcome {
            Ok(message) => message,
            // A failed action is a message, not a crash: the user reads it and
            // tries something else, exactly as after a failed command.
            Err(err) => format!("error: {err:#}"),
        };
    }

    fn ask(&mut self, kind: Input) {
        self.mode = Mode::Input {
            kind,
            buffer: String::new(),
        };
    }

    fn input_key(&mut self, code: KeyCode, kind: Input, mut buffer: String) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.message = "cancelled".into();
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                let value = buffer.trim().to_string();
                if value.is_empty() {
                    self.message = "nothing entered".into();
                    return Ok(());
                }
                let outcome = match kind {
                    Input::CommitMessage => self.commit(&value),
                    Input::UploadPath => self.upload(&value),
                };
                self.report(outcome);
                self.refresh()?;
            }
            KeyCode::Backspace => {
                buffer.pop();
                self.mode = Mode::Input { kind, buffer };
            }
            KeyCode::Char(c) => {
                buffer.push(c);
                self.mode = Mode::Input { kind, buffer };
            }
            _ => {}
        }
        Ok(())
    }

    fn confirm_key(&mut self, code: KeyCode) -> Result<()> {
        let yes = matches!(code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let previous = std::mem::replace(&mut self.mode, Mode::Browse);
        if !yes {
            self.message = "cancelled".into();
            return Ok(());
        }
        let Mode::Confirm { action, .. } = previous else {
            return Ok(());
        };
        let outcome = match action {
            Pending::DestroyCommit => self.destroy_commit(),
            Pending::DiscardFile(path, untracked) => self.discard(&path, untracked),
            Pending::DeleteBackup(path) => self.delete_backup(&path),
        };
        self.report(outcome);
        self.refresh()
    }

    fn toggle_stage(&mut self) -> Result<()> {
        let Some(file) = self.files.get(self.selection()) else {
            return Ok(());
        };
        let (path, staged, unstaged) = (file.path.clone(), file.staged, file.unstaged);
        // A file with both staged and unstaged parts stages the rest, which is
        // the more useful reading of one keypress.
        let outcome = if unstaged || !staged {
            git::stage_path(&self.repo, &path).map(|()| format!("staged {path}"))
        } else {
            git::unstage_path(&self.repo, &path).map(|()| format!("unstaged {path}"))
        };
        self.report(outcome);
        self.refresh()
    }

    fn stage_all(&mut self) -> Result<()> {
        let paths: Vec<String> = self.files.iter().map(|f| f.path.clone()).collect();
        let mut staged = 0;
        for path in &paths {
            git::stage_path(&self.repo, path)?;
            staged += 1;
        }
        self.message = format!("staged {staged} file(s)");
        self.refresh()
    }

    fn ask_discard(&mut self) {
        let Some(file) = self.files.get(self.selection()) else {
            return;
        };
        let verb = if file.untracked {
            "delete"
        } else {
            "discard changes to"
        };
        self.mode = Mode::Confirm {
            question: format!("{verb} {}?", file.path),
            action: Pending::DiscardFile(file.path.clone(), file.untracked),
        };
    }

    fn discard(&mut self, path: &str, untracked: bool) -> Result<String> {
        git::discard_path(&self.repo, path, untracked)?;
        Ok(if untracked {
            format!("deleted {path}")
        } else {
            format!("discarded changes to {path}")
        })
    }

    fn commit(&mut self, message: &str) -> Result<String> {
        if !git::create_commit(&self.repo, message)? {
            return Ok("nothing to commit, working tree clean".into());
        }
        // The commit just made is the newest version, so a stepped-back cursor
        // no longer applies - the rule the CLI follows too.
        history::clear(&self.repo)?;
        let head = self.repo.head()?.peel_to_commit()?;
        let (id, _) = git::describe(&self.repo, head.id())?;
        Ok(format!("committed {id} (press p to push)"))
    }

    fn upload(&mut self, path: &str) -> Result<String> {
        let report = ops::upload(&self.repo, &[PathBuf::from(path)])?;
        Ok(format!(
            "uploaded {} ({} copied, {} unchanged, {})",
            report
                .staged
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            report.stats.copied,
            report.stats.skipped,
            ops::human_bytes(report.stats.bytes)
        ))
    }

    fn jump(&mut self) -> Result<()> {
        let index = self.selection();
        let outcome = ops::jump_to(&self.repo, index).map(|position| {
            if position.is_newest() {
                "moved to the newest version".to_string()
            } else {
                format!(
                    "moved to version {} of {}",
                    position.total - position.index,
                    position.total
                )
            }
        });
        self.report(outcome);
        self.refresh()
    }

    fn step(&mut self, direction: history::Direction) -> Result<()> {
        let outcome = ops::step(&self.repo, direction).map(|report| match report {
            ops::StepReport::Boundary => match direction {
                history::Direction::Older => "oldest change reached".to_string(),
                history::Direction::Newer => "newest change released".to_string(),
            },
            ops::StepReport::Moved(position) => format!(
                "moved to version {} of {}",
                position.total - position.index,
                position.total
            ),
        });
        self.report(outcome);
        self.refresh()?;
        // Follow the cursor, so the list shows where the working tree now is.
        self.selected[2] = self.position;
        self.load_main();
        Ok(())
    }

    fn ask_destroy(&mut self) -> Result<()> {
        // Only the newest commit can be destroyed, the same restriction the
        // command has, so say so rather than appearing to offer more.
        if self.selection() != 0 {
            self.message = "only the newest version can be destroyed - select it first".into();
            return Ok(());
        }
        let Some(commit) = self.commits.first() else {
            return Ok(());
        };
        self.mode = Mode::Confirm {
            question: format!(
                "destroy {} \"{}\" for good? it will be backed up first",
                commit.id, commit.subject
            ),
            action: Pending::DestroyCommit,
        };
        Ok(())
    }

    fn destroy_commit(&mut self) -> Result<String> {
        let plan = ops::plan_drop(&self.repo, 1)?;
        let report = ops::drop_commits(&self.repo, &plan, true, true)?;
        let mut message = format!("destroyed 1 commit; HEAD is now {}", report.head_id);
        if let Some(err) = &report.push_error {
            message.push_str(&format!(" (the push failed: {err})"));
        }
        Ok(message)
    }

    fn revert(&mut self) -> Result<()> {
        let Some(commit) = self.commits.get(self.selection()) else {
            return Ok(());
        };
        let revision = commit.oid.to_string();
        let outcome = git::revert_commit(&self.repo, &revision)
            .map(|_| format!("reverted {} in a new commit", commit.id));
        self.report(outcome);
        self.refresh()
    }

    fn backup(&mut self) -> Result<()> {
        let outcome = backup::create(&self.repo, None).map(|saved| {
            format!(
                "saved {} ({})",
                saved.path.display(),
                ops::human_bytes(saved.bytes)
            )
        });
        self.report(outcome);
        self.refresh()
    }

    fn ask_delete_backup(&mut self) {
        let Some(row) = self.backups.get(self.selection()) else {
            return;
        };
        self.mode = Mode::Confirm {
            question: format!("delete the backup {}?", row.name),
            action: Pending::DeleteBackup(row.path.clone()),
        };
    }

    fn delete_backup(&mut self, path: &PathBuf) -> Result<String> {
        std::fs::remove_file(path).map_err(|e| anyhow!("cannot delete: {e}"))?;
        Ok(format!(
            "deleted {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
    }

    /// Push, giving the terminal back first: a push may hand over to
    /// `gh auth login`, which needs the ordinary screen to talk to the user.
    fn push(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let outcome = self.outside(terminal, |app| {
            ops::push(&app.repo, false).map(|report| match report {
                ops::PushReport::Ssh { host } => format!("pushed to {host}"),
                ops::PushReport::Http { host, username } => {
                    format!("pushed to {host} as {username}")
                }
                ops::PushReport::Local { target } => format!("pushed to {target}"),
            })
        })?;
        self.report(outcome);
        self.refresh()
    }

    fn login(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let host = self.remote.clone();
        let outcome = self.outside(terminal, |_| {
            let forge = dotgit::gh::forge_for_host(&host).unwrap_or_default();
            dotgit::gh::ensure_cli(forge)?;
            dotgit::gh::forge_login(forge, &host)?;
            Ok(format!("logged in to {host}"))
        })?;
        self.report(outcome);
        self.refresh()
    }

    /// Run something that needs the ordinary terminal, then come back.
    fn outside<T>(
        &mut self,
        terminal: &mut DefaultTerminal,
        action: impl FnOnce(&mut Self) -> T,
    ) -> Result<T> {
        ratatui::restore();
        let outcome = action(self);
        *terminal = ratatui::init();
        terminal.clear()?;
        Ok(outcome)
    }

    fn draw(&self, frame: &mut Frame) {
        let rows =
            Layout::vertical([Constraint::Min(6), Constraint::Length(4)]).split(frame.area());
        let columns = Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(rows[0]);
        let side = Layout::vertical([
            Constraint::Length(4),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
            Constraint::Min(3),
        ])
        .split(columns[0]);

        self.draw_status(frame, side[0]);
        self.draw_files(frame, side[1]);
        self.draw_versions(frame, side[2]);
        self.draw_backups(frame, side[3]);
        self.draw_main(frame, columns[1]);
        self.draw_footer(frame, rows[1]);

        match &self.mode {
            Mode::Input { kind, buffer } => self.draw_input(frame, *kind, buffer),
            Mode::Confirm { question, .. } => self.draw_confirm(frame, question),
            Mode::Help => self.draw_help(frame),
            Mode::Browse => {}
        }
    }

    /// A panel's frame, highlighted when it has focus - the cue that says which
    /// keys are live.
    fn block(&self, panel: Panel) -> Block<'static> {
        let focused = self.focus == panel;
        let style = if focused {
            Style::new().fg(Color::Green).bold()
        } else {
            Style::new().fg(Color::DarkGray)
        };
        Block::default()
            .borders(Borders::ALL)
            .border_style(style)
            .title(Span::styled(
                panel.title(),
                if focused {
                    Style::new().fg(Color::Green).bold()
                } else {
                    Style::new()
                },
            ))
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let version = if self.commits.is_empty() {
            "no commits".to_string()
        } else if self.position == 0 {
            format!("newest of {}", self.commits.len())
        } else {
            format!(
                "v{} of {}",
                self.commits.len() - self.position,
                self.commits.len()
            )
        };
        let state = if self.dirty {
            Span::styled("changes", Style::new().fg(Color::Yellow))
        } else {
            Span::styled("clean", Style::new().fg(Color::Green))
        };
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    self.root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| self.root.display().to_string()),
                    Style::new().bold(),
                ),
                Span::raw(format!(" [{}]", self.branch)),
            ]),
            Line::from(vec![
                Span::raw(format!("{}  {version}  ", self.remote)),
                state,
            ]),
        ];
        frame.render_widget(Paragraph::new(lines).block(self.block(Panel::Status)), area);
    }

    fn draw_files(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .files
            .iter()
            .map(|file| {
                // Green for what is staged, red for what is not: the colours
                // lazygit uses, and the fastest way to read an index.
                let colour = if file.staged && !file.unstaged {
                    Color::Green
                } else if file.staged {
                    Color::Yellow
                } else {
                    Color::Red
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", file.label), Style::new().fg(colour)),
                    Span::raw(file.path.clone()),
                ]))
            })
            .collect();
        let items = if items.is_empty() {
            vec![ListItem::new(Span::styled(
                "nothing changed",
                Style::new().fg(Color::DarkGray),
            ))]
        } else {
            items
        };
        self.render_list(frame, area, Panel::Files, items);
    }

    fn draw_versions(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .commits
            .iter()
            .enumerate()
            .map(|(index, commit)| {
                // The marker shows which version the working tree holds, which
                // is not always the newest commit.
                let (marker, style) = if index == self.position {
                    (">", Style::new().fg(Color::Green).bold())
                } else {
                    (" ", Style::new())
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} "), style),
                    Span::styled(commit.id.clone(), Style::new().fg(Color::Yellow)),
                    Span::raw(format!(" {}", commit.subject)),
                ]))
            })
            .collect();
        let items = if items.is_empty() {
            vec![ListItem::new(Span::styled(
                "no commits yet",
                Style::new().fg(Color::DarkGray),
            ))]
        } else {
            items
        };
        self.render_list(frame, area, Panel::Versions, items);
    }

    fn draw_backups(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .backups
            .iter()
            .map(|row| {
                ListItem::new(Line::from(vec![
                    Span::raw(row.name.clone()),
                    Span::styled(
                        format!("  {}", ops::human_bytes(row.bytes)),
                        Style::new().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();
        let items = if items.is_empty() {
            vec![ListItem::new(Span::styled(
                "none - press n",
                Style::new().fg(Color::DarkGray),
            ))]
        } else {
            items
        };
        self.render_list(frame, area, Panel::Backups, items);
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, panel: Panel, items: Vec<ListItem>) {
        let mut state = ListState::default();
        if self.focus == panel {
            state.select(Some(
                self.selected[Panel::ALL.iter().position(|p| *p == panel).unwrap_or(0)],
            ));
        }
        frame.render_stateful_widget(
            List::new(items)
                .block(self.block(panel))
                .highlight_style(Style::new().reversed()),
            area,
            &mut state,
        );
    }

    fn draw_main(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = self
            .main_lines
            .iter()
            .skip(self.scroll)
            .map(|line| {
                // Patch colouring, by the first character of each line.
                let style = match line.chars().next() {
                    Some('+') => Style::new().fg(Color::Green),
                    Some('-') => Style::new().fg(Color::Red),
                    Some('@') => Style::new().fg(Color::Cyan),
                    _ if line.starts_with("diff ") || line.starts_with("index ") => {
                        Style::new().fg(Color::DarkGray)
                    }
                    _ => Style::new(),
                };
                Line::from(Span::styled(line.clone(), style))
            })
            .collect();

        let scrolled = if self.scroll > 0 {
            format!("{}(+{} above) ", self.main_title, self.scroll)
        } else {
            self.main_title.clone()
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::DarkGray))
                    .title(scrolled),
            ),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        // Only the focused panel's keys, plus the handful that always work.
        let mut keys: Vec<Span> = Vec::new();
        for (key, what, _) in self.focus.keys() {
            keys.push(Span::styled(
                format!("{key} "),
                Style::new().fg(Color::Yellow),
            ));
            keys.push(Span::raw(format!("{what}   ")));
        }
        let global = "tab panel  j/k move  J/K scroll  R refresh  ? keys  q quit";
        let lines = vec![
            Line::from(Span::styled(
                self.message.clone(),
                Style::new().fg(Color::Cyan),
            )),
            Line::from(keys),
            Line::from(Span::styled(global, Style::new().fg(Color::DarkGray))),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::new().fg(Color::DarkGray)),
            ),
            area,
        );
    }

    fn draw_input(&self, frame: &mut Frame, kind: Input, buffer: &str) {
        let area = centred(frame.area(), 72, 4);
        frame.render_widget(Clear, area);
        let lines = vec![
            Line::from(vec![
                Span::raw("> "),
                Span::styled(buffer.to_string(), Style::new().bold()),
                Span::styled("_", Style::new().fg(Color::DarkGray)),
            ]),
            Line::from(Span::styled(kind.hint(), Style::new().fg(Color::DarkGray))),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::Cyan))
                    .title(kind.title()),
            ),
            area,
        );
    }

    fn draw_confirm(&self, frame: &mut Frame, question: &str) {
        let area = centred(frame.area(), 74, 5);
        frame.render_widget(Clear, area);
        let lines = vec![
            Line::from(Span::styled(question.to_string(), Style::new().bold())),
            Line::from(""),
            Line::from(Span::styled(
                "y to confirm, anything else cancels",
                Style::new().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::Red))
                    .title(" are you sure? "),
            ),
            area,
        );
    }

    fn draw_help(&self, frame: &mut Frame) {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{} - every action has a command", self.focus.title().trim()),
                Style::new().bold(),
            )),
            Line::from(""),
        ];
        lines.extend(self.focus.keys().iter().map(|(key, what, command)| {
            Line::from(vec![
                Span::styled(format!("  {key:<6}"), Style::new().fg(Color::Yellow)),
                Span::raw(format!("{what:<26}")),
                Span::styled((*command).to_string(), Style::new().fg(Color::DarkGray)),
            ])
        }));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  anywhere", Style::new().bold())));
        for (key, what) in [
            ("1-4 / tab", "change panel"),
            ("j / k", "move the selection"),
            ("J / K", "scroll the diff"),
            ("p", "push"),
            ("b", "back up the history"),
            ("R", "refresh"),
            ("q", "quit"),
        ] {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<11}"), Style::new().fg(Color::Yellow)),
                Span::raw(what),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "press any key to close",
            Style::new().fg(Color::DarkGray),
        )));

        let area = centred(frame.area(), 76, (lines.len() + 2) as u16);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::Cyan))
                    .title(" keys "),
            ),
            area,
        );
    }
}

/// A box of the given size in the middle of `area`, clamped so it still fits on
/// a small terminal.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Reject arguments rather than silently ignoring them: the TUI takes none, and
/// anyone passing some is looking for the command line tool.
fn reject_arguments(args: &[String]) -> Result<()> {
    match args.first() {
        None => Ok(()),
        Some(arg) => Err(anyhow!(
            "dotgit-tui takes no arguments (got `{arg}`) - run `dotgit {arg}` instead"
        )),
    }
}
