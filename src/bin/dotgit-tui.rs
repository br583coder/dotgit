//! `dotgit status`: an optional full-screen browser for a dotfiles repository,
//! arranged the way lazygit arranges one.
//!
//! The left column holds four panels - status, files, versions and backups -
//! and one of them has focus at a time. The right pane always shows what the
//! focused panel's selection means: a file's patch, a version's patch, a
//! bundle's details. Keys act on the focused panel, and the footer lists the
//! ones that apply right now rather than every key that exists.
//!
//! Everything here calls the same functions the `dotgit` command calls, so the
//! two can never disagree. The TUI stays entirely optional behind a non-default
//! feature, and every action
//! names its command line equivalent in the help pane.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use git2::Repository;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};

use crate::config::{self, Source};
use crate::{backup, gh, git, history, ops};

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    // The wheel is how most people scroll, so ask the terminal for mouse
    // events. It is switched off again on the way out, including when an action
    // hands the screen back temporarily, so a terminal is never left in a state
    // where selecting text with the mouse has stopped working.
    let mouse = capture_mouse(true);
    let result = App::new().and_then(|mut app| app.run(&mut terminal));
    if mouse.is_ok() {
        let _ = capture_mouse(false);
    }
    ratatui::restore();
    result
}

/// Ask the terminal to report (or stop reporting) mouse events. A terminal that
/// refuses is not an error: the keyboard still scrolls everything.
fn capture_mouse(on: bool) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    if on {
        execute!(out, EnableMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
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
                ("e", "edit dotgit.toml", ""),
                ("p", "push", "dotgit commit"),
                ("b", "back up history", "dotgit backup"),
                ("L", "log in to the remote", "dotgit login"),
            ],
            Panel::Files => &[
                ("e", "edit in $EDITOR", ""),
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
    /// Height of the diff pane's inside, from the last frame. Scrolling needs it
    /// to stop at the point where the final line reaches the bottom, instead of
    /// letting the content slide out of view entirely.
    main_height: usize,
    /// Where each panel was drawn, so a mouse event can be sent to whatever is
    /// under the pointer rather than to whatever has focus.
    main_area: Rect,
    list_areas: [Rect; 4],
    message: String,
    mode: Mode,
    /// Read once at start-up, so every action taken here lands in dotgit.log
    /// exactly as the equivalent command would.
    logging: config::Logging,
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
            main_height: 0,
            main_area: Rect::ZERO,
            list_areas: [Rect::ZERO; 4],
            message: "? for keys, tab to change panel, q to quit".into(),
            mode: Mode::Browse,
            // A broken config must not stop the interface from opening, so fall
            // back to the defaults and say so in the status line.
            logging: match config::Config::load() {
                Ok(settings) => settings.logging,
                Err(_) => config::Logging::default(),
            },
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
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Mouse(mouse) => {
                    self.mouse(mouse);
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('c') => {
                        self.quit = true;
                        continue;
                    }
                    // Half a pane at a time, as in a pager.
                    KeyCode::Char('d') => {
                        self.scroll_main(self.half_pane());
                        continue;
                    }
                    KeyCode::Char('u') => {
                        self.scroll_main(-self.half_pane());
                        continue;
                    }
                    _ => {}
                }
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
            // The diff pane scrolls with the shifted keys - a line at a time for
            // reading code, half a pane at a time for covering ground - so the
            // list keeps the plain ones.
            KeyCode::Char('J') => self.scroll_main(1),
            KeyCode::Char('K') => self.scroll_main(-1),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll(),
            // Page keys move through whichever list has focus, which is what
            // makes a long version history navigable.
            KeyCode::PageDown => self.page_selection(1),
            KeyCode::PageUp => self.page_selection(-1),
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
            (Panel::Status, KeyCode::Char('e')) => self.edit_config(terminal)?,

            (Panel::Files, KeyCode::Char('e')) => self.edit_file(terminal)?,

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

    /// The furthest the diff pane can scroll: far enough to bring the last line
    /// into view, and no further, so the pane never ends up mostly empty.
    fn max_scroll(&self) -> usize {
        max_scroll(self.main_lines.len(), self.main_height)
    }

    fn half_pane(&self) -> isize {
        (self.main_height / 2).max(1) as isize
    }

    fn scroll_main(&mut self, delta: isize) {
        self.scroll = scrolled(self.scroll, delta, self.main_lines.len(), self.main_height);
    }

    /// Move a list's selection by one pane's worth of rows.
    fn page_selection(&mut self, direction: isize) {
        let rows = self.list_areas[self.slot()].height.saturating_sub(2).max(1) as isize;
        self.move_selection(direction * rows);
    }

    /// Send a wheel event to whatever the pointer is over, falling back to the
    /// diff pane, which is what people usually mean to scroll.
    fn mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollUp => -1,
            _ => return,
        };
        let point = Position::new(mouse.column, mouse.row);
        for (slot, area) in self.list_areas.iter().enumerate() {
            if area.contains(point) {
                let panel = Panel::ALL[slot];
                if self.focus != panel {
                    self.focus(panel);
                }
                // Three rows per notch: one feels stuck, a whole pane overshoots.
                self.move_selection(delta * 3);
                return;
            }
        }
        self.scroll_main(delta * 3);
    }

    /// Record what an action did and show it. Every action goes through here,
    /// which is what keeps the log complete without dotting logging calls
    /// through the interface.
    fn report(&mut self, command: &str, outcome: Result<String>) {
        if let Err(log_err) = self.logging.record(Source::Tui, command, &outcome) {
            // Logging is not worth losing the action's own message over.
            self.message = format!("logging failed: {log_err:#}");
        }
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
                let (command, outcome) = match kind {
                    Input::CommitMessage => ("commit", self.commit(&value)),
                    Input::UploadPath => ("upload", self.upload(&value)),
                };
                self.report(command, outcome);
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
        let (command, outcome) = match action {
            Pending::DestroyCommit => ("pull", self.destroy_commit()),
            Pending::DiscardFile(path, untracked) => ("discard", self.discard(&path, untracked)),
            Pending::DeleteBackup(path) => ("backup-delete", self.delete_backup(&path)),
        };
        self.report(command, outcome);
        self.refresh()
    }

    fn toggle_stage(&mut self) -> Result<()> {
        let Some(file) = self.files.get(self.selection()) else {
            return Ok(());
        };
        let (path, staged, unstaged) = (file.path.clone(), file.staged, file.unstaged);
        // A file with both staged and unstaged parts stages the rest, which is
        // the more useful reading of one keypress.
        let (command, outcome) = if unstaged || !staged {
            (
                "stage",
                git::stage_path(&self.repo, &path).map(|()| format!("staged {path}")),
            )
        } else {
            (
                "unstage",
                git::unstage_path(&self.repo, &path).map(|()| format!("unstaged {path}")),
            )
        };
        self.report(command, outcome);
        self.refresh()
    }

    fn stage_all(&mut self) -> Result<()> {
        let paths: Vec<String> = self.files.iter().map(|f| f.path.clone()).collect();
        let outcome = paths
            .iter()
            .try_fold(0usize, |staged, path| {
                git::stage_path(&self.repo, path).map(|()| staged + 1)
            })
            .map(|staged| format!("staged {staged} file(s)"));
        self.report("stage-all", outcome);
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
        self.report("jump", outcome);
        self.refresh()
    }

    fn step(&mut self, direction: history::Direction) -> Result<()> {
        // The same names the command line uses for these two moves.
        let command = match direction {
            history::Direction::Older => "restore",
            history::Direction::Newer => "rebase",
        };
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
        self.report(command, outcome);
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
        self.report("revert", outcome);
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
        self.report("backup", outcome);
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

    /// Open the selected file in the user's editor. A real editor beats
    /// anything this interface could offer, so the screen is handed over for as
    /// long as it runs and the panels are re-read when it exits.
    fn edit_file(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(file) = self.files.get(self.selection()) else {
            self.message = "no file selected".into();
            return Ok(());
        };
        let path = self.root.join(&file.path);
        if !path.exists() {
            // A deleted file has nothing to edit; opening it would quietly
            // recreate it, which is not what pressing `e` asks for.
            self.message = format!("{} no longer exists on disk", file.path);
            return Ok(());
        }
        let shown = file.path.clone();
        let outcome = self
            .outside(terminal, |_| edit(&path))?
            .map(|()| format!("edited {shown}"));
        self.report("edit", outcome);
        self.refresh()
    }

    /// Edit this repository's `dotgit.toml`, creating it if it is not there yet:
    /// the settings the status panel reflects are the ones in that file.
    fn edit_config(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let path = self.root.join("dotgit.toml");
        let existed = path.exists();
        let outcome = self.outside(terminal, |_| edit(&path))?.map(|()| {
            if existed {
                "edited dotgit.toml".to_string()
            } else {
                "wrote a new dotgit.toml".to_string()
            }
        });
        self.report("edit-config", outcome);
        // Settings may have changed, including where the log goes.
        if let Ok(settings) = config::Config::load() {
            self.logging = settings.logging;
        }
        self.refresh()
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
        self.report("push", outcome);
        self.refresh()
    }

    fn login(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let host = self.remote.clone();
        let outcome = self.outside(terminal, |_| {
            let forge = gh::forge_for_host(&host).unwrap_or_default();
            gh::ensure_cli(forge)?;
            gh::forge_login(forge, &host)?;
            Ok(format!("logged in to {host}"))
        })?;
        self.report("login", outcome);
        self.refresh()
    }

    /// Run something that needs the ordinary terminal - an editor, a login
    /// prompt - then come back.
    ///
    /// The alternate screen and raw mode are turned off and on around it, but
    /// the same terminal is kept throughout: building a new one asks the
    /// terminal for its cursor position, and a program that has just been in
    /// control may not answer in time, which fails the whole action.
    fn outside<T>(
        &mut self,
        terminal: &mut DefaultTerminal,
        action: impl FnOnce(&mut Self) -> T,
    ) -> Result<T> {
        // Mouse reporting has to go off with the full-screen view, or `gh auth
        // login` would receive the wheel as gibberish on its prompt.
        let _ = capture_mouse(false);
        disable_raw_mode()?;
        execute!(std::io::stdout(), LeaveAlternateScreen)?;

        let outcome = action(self);

        execute!(std::io::stdout(), EnterAlternateScreen)?;
        enable_raw_mode()?;
        let _ = capture_mouse(true);
        // The screen still holds whatever the program left behind, and the
        // editor may have drawn over the alternate screen, so repaint from
        // scratch. `Terminal::clear` would be the obvious call, but it asks the
        // terminal where the cursor is and a program that has just had control
        // may not answer in time, which fails the action. Resizing to the
        // current size clears the viewport and resets the back buffer without
        // that question.
        let size = terminal.size()?;
        terminal.resize(Rect::new(0, 0, size.width, size.height))?;
        Ok(outcome)
    }

    fn draw(&mut self, frame: &mut Frame) {
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

        // Remember the geometry: scrolling needs the pane height, and the mouse
        // needs to know what sits where.
        self.main_area = columns[1];
        self.main_height = columns[1].height.saturating_sub(2) as usize;
        self.list_areas = [side[0], side[1], side[2], side[3]];
        // A pane that has shrunk may leave the view scrolled past the end.
        self.scroll = self.scroll.min(self.max_scroll());

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
        let slot = Panel::ALL.iter().position(|p| *p == panel).unwrap_or(0);
        let selected = self.selected[slot];
        let total = items.len();

        let mut state = ListState::default();
        // The selection is always set, so the list scrolls itself to keep the
        // cursor in view; only its highlight depends on focus.
        state.select(Some(selected));
        let highlight = if self.focus == panel {
            Style::new().reversed()
        } else {
            Style::new().add_modifier(Modifier::DIM)
        };
        frame.render_stateful_widget(
            List::new(items)
                .block(self.block(panel))
                .highlight_style(highlight),
            area,
            &mut state,
        );

        // A scrollbar appears only when the list is longer than its pane, so it
        // says something when it is there.
        let height = area.height.saturating_sub(2) as usize;
        if total > height && height > 0 {
            let mut bar = ScrollbarState::new(total.saturating_sub(height))
                .position(selected.saturating_sub(height / 2).min(total - height));
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut bar,
            );
        }
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

        // Say where in the content the view is, so scrolling has a reference
        // point instead of an unmoored wall of text.
        let total = self.main_lines.len();
        let height = self.main_height.max(1);
        let title = if total > height {
            let first = self.scroll + 1;
            let last = (self.scroll + height).min(total);
            format!("{}lines {first}-{last} of {total} ", self.main_title,)
        } else {
            self.main_title.clone()
        };
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::DarkGray))
                    .title(title),
            ),
            area,
        );

        if total > height {
            let mut bar = ScrollbarState::new(total - height).position(self.scroll);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                area,
                &mut bar,
            );
        }
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
        let global = "tab panel  j/k move  pgup/pgdn page  J/K + ctrl-d/u scroll diff  wheel  ? keys  q quit";
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
            ("pgup/pgdn", "move the selection a pane at a time"),
            ("g / G", "first / last item"),
            ("J / K", "scroll the diff one line"),
            ("ctrl-d/u", "scroll the diff half a pane"),
            ("home/end", "top / bottom of the diff"),
            ("wheel", "scroll whatever is under the pointer"),
            ("e", "edit the selected file in $EDITOR"),
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

/// Run the user's editor on `path` and wait for it to close.
///
/// The command is passed to a shell with the path as an argument rather than
/// pasted into the string, so an editor set to something like `code -w` works
/// and a path with spaces or quotes in it cannot be misread as more arguments.
fn edit(path: &std::path::Path) -> Result<()> {
    let editor = editor_command()?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .map_err(|e| anyhow!("cannot run {editor}: {e}"))?;
    if !status.success() {
        return Err(anyhow!("{editor} exited without saving"));
    }
    Ok(())
}

/// `$VISUAL`, then `$EDITOR`, then `vi` if it is installed. Guessing further
/// would be worse than saying so.
fn editor_command() -> Result<String> {
    for name in ["VISUAL", "EDITOR"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    if std::process::Command::new("sh")
        .args(["-c", "command -v vi >/dev/null"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok("vi".to_string());
    }
    Err(anyhow!(
        "no editor found - set $EDITOR (for example `export EDITOR=nvim`)"
    ))
}

/// The furthest a pane of `height` rows can scroll through `total` lines: far
/// enough to bring the last line into view, and no further, so the pane never
/// ends up mostly empty below the end of the content.
fn max_scroll(total: usize, height: usize) -> usize {
    total.saturating_sub(height.max(1))
}

/// Move a scroll position by `delta`, stopping at either end.
fn scrolled(current: usize, delta: isize, total: usize, height: usize) -> usize {
    let max = max_scroll(total, height) as isize;
    (current as isize + delta).clamp(0, max) as usize
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_shorter_than_the_pane_cannot_scroll() {
        assert_eq!(max_scroll(10, 34), 0);
        assert_eq!(scrolled(0, 5, 10, 34), 0);
    }

    #[test]
    fn scrolling_stops_with_the_last_line_in_view() {
        // 242 lines in a 34-row pane: the final view starts at line 209.
        assert_eq!(max_scroll(242, 34), 208);
        assert_eq!(scrolled(200, 100, 242, 34), 208);
        assert_eq!(scrolled(208, 1, 242, 34), 208);
    }

    #[test]
    fn scrolling_stops_at_the_top() {
        assert_eq!(scrolled(0, -1, 242, 34), 0);
        assert_eq!(scrolled(3, -10, 242, 34), 0);
    }

    #[test]
    fn a_line_and_a_half_pane_move_by_the_amounts_they_promise() {
        assert_eq!(scrolled(10, 1, 242, 34), 11);
        assert_eq!(scrolled(10, -1, 242, 34), 9);
        // Half of a 34-row pane.
        assert_eq!(scrolled(10, 17, 242, 34), 27);
        assert_eq!(scrolled(27, -17, 242, 34), 10);
    }

    #[test]
    fn a_pane_of_no_height_does_not_divide_by_zero() {
        // The first frame has not been drawn yet, so the height is still zero.
        assert_eq!(max_scroll(242, 0), 241);
        assert_eq!(scrolled(0, 1, 242, 0), 1);
    }
}
