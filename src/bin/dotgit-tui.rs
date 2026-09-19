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
use ratatui::crossterm::cursor::SetCursorStyle;
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
use crate::editor::{self, Editor};
use crate::highlight::{self, Kind};
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
    // Never leave the user's shell with a cursor shape dotgit chose.
    let _ = Shape::Default.apply();
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
    /// The commit message, typed in place. There is no pop-up: the message is
    /// part of the layout, so it survives looking at a diff and can be written
    /// over several visits.
    Commit,
}

impl Panel {
    const ALL: [Panel; 5] = [
        Panel::Status,
        Panel::Files,
        Panel::Versions,
        Panel::Backups,
        Panel::Commit,
    ];

    fn title(self) -> &'static str {
        match self {
            Panel::Status => " 1 status ",
            Panel::Files => " 2 files ",
            Panel::Versions => " 3 versions ",
            Panel::Backups => " 4 backups ",
            Panel::Commit => " 5 commit message ",
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
                ("e", "edit dotgit.toml here", ""),
                ("E", "edit it in $EDITOR", ""),
                ("p", "push", "dotgit commit"),
                ("b", "back up history", "dotgit backup"),
                ("L", "log in to the remote", "dotgit login"),
            ],
            Panel::Files => &[
                ("e", "edit here (vim keys)", ""),
                ("E", "edit in $EDITOR", ""),
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
            Panel::Commit => &[
                ("type", "write the message", ""),
                ("enter", "commit and push", "dotgit commit"),
                ("ctrl-l", "commit locally only", "dotgit commit --no-push"),
                ("esc", "back to the files", ""),
            ],
        }
    }
}

/// The cursor shapes the interface asks the terminal for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Whatever the user's terminal normally uses.
    Default,
    /// A block sitting on the character, as in vim's normal mode.
    Block,
    /// A thin blinking bar between characters, as in VS Code - and what neovim
    /// switches to for insert mode.
    Bar,
}

impl Shape {
    /// Writing mode gets the bar; everything else gets a block.
    fn for_editor(mode: editor::Mode) -> Self {
        match mode {
            editor::Mode::Insert => Shape::Bar,
            // The command line is typed into, but the cursor on screen is still
            // the buffer's, so it keeps the block.
            editor::Mode::Normal | editor::Mode::Command => Shape::Block,
        }
    }

    fn apply(self) -> std::io::Result<()> {
        let mut out = std::io::stdout();
        match self {
            Shape::Default => execute!(out, SetCursorStyle::DefaultUserShape),
            Shape::Block => execute!(out, SetCursorStyle::SteadyBlock),
            Shape::Bar => execute!(out, SetCursorStyle::BlinkingBar),
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
    RevertCommit(String),
    DiscardFile(String, bool),
    DeleteBackup(PathBuf),
}

enum Mode {
    Browse,
    /// The built-in modal editor has the screen.
    Edit(Box<Editor>),
    Input {
        kind: Input,
        buffer: String,
    },
    Confirm {
        question: String,
        action: Pending,
    },
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    UploadPath,
}

impl Input {
    fn title(self) -> &'static str {
        match self {
            Input::UploadPath => " path to upload ",
        }
    }

    fn hint(self) -> &'static str {
        match self {
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
    selected: [usize; 5],
    /// The commit message being written in panel 5.
    message_draft: String,
    position: usize,
    dirty: bool,
    /// The right-hand pane: a title and the lines beneath it.
    main_title: String,
    /// Whether the pane holds a patch, which decides whether lines are coloured
    /// as a diff or as the file's own syntax.
    main_is_diff: bool,
    /// The language of the file shown in the pane, when it is not a patch.
    main_language: highlight::Language,
    main_lines: Vec<String>,
    scroll: usize,
    /// Height of the diff pane's inside, from the last frame. Scrolling needs it
    /// to stop at the point where the final line reaches the bottom, instead of
    /// letting the content slide out of view entirely.
    main_height: usize,
    /// Where the editor's text was last drawn, so a click can be turned into a
    /// line and column.
    edit_area: Rect,
    /// The cursor shape currently set, so the escape sequence is only sent when
    /// it actually changes rather than on every frame.
    cursor_shape: Shape,
    /// Where each panel was drawn, so a mouse event can be sent to whatever is
    /// under the pointer rather than to whatever has focus.
    main_area: Rect,
    list_areas: [Rect; 5],
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
            selected: [0; 5],
            message_draft: String::new(),
            position: 0,
            dirty: false,
            main_title: String::new(),
            main_is_diff: true,
            main_language: highlight::Language::Plain,
            main_lines: Vec::new(),
            scroll: 0,
            main_height: 0,
            edit_area: Rect::ZERO,
            cursor_shape: Shape::Default,
            main_area: Rect::ZERO,
            list_areas: [Rect::ZERO; 5],
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
            Panel::Commit => 0,
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

        // Changed files first, then every other tracked file, so a file stays
        // reachable after it has been committed - otherwise committing a file
        // would be the last time you could open it here.
        self.files = git::status_entries(&self.repo).unwrap_or_default();
        let changed: std::collections::HashSet<String> =
            self.files.iter().map(|f| f.path.clone()).collect();
        self.files.extend(
            git::tracked_files(&self.repo)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|path| {
                    if changed.contains(&path) {
                        return None;
                    }
                    Some(git::FileStatus {
                        path,
                        // Two spaces where git would print a status letter.
                        label: "  ".to_string(),
                        staged: false,
                        unstaged: false,
                        untracked: false,
                    })
                }),
        );

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
                Panel::Status | Panel::Commit => 0,
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
                self.main_is_diff = true;
            }
            // Writing a message is about what is going into the commit, so show
            // that rather than leaving the pane on whatever was there before.
            Panel::Commit => {
                self.main_title = " to be committed ".into();
                let staged: Vec<String> = self
                    .files
                    .iter()
                    .filter(|f| f.staged)
                    .map(|f| format!("{}  {}", f.label, f.path))
                    .collect();
                self.main_lines = if staged.is_empty() {
                    vec![
                        "nothing is staged".into(),
                        String::new(),
                        "stage files in panel 2 with space, or a to stage everything".into(),
                    ]
                } else {
                    let mut lines =
                        vec![format!("{} file(s) staged:", staged.len()), String::new()];
                    lines.extend(staged);
                    lines
                };
            }
            Panel::Files => match self.files.get(index) {
                None => {
                    self.main_title = " diff ".into();
                    self.main_lines = vec!["no files yet".into()];
                    self.main_is_diff = true;
                }
                Some(file) if !file.staged && !file.unstaged && !file.untracked => {
                    // A committed file: show what is in it, so you can read it
                    // before opening it with `e`.
                    self.main_title = format!(" {} ", file.path);
                    self.main_is_diff = false;
                    let path = self.root.join(&file.path);
                    self.main_language = highlight::language_for(&path);
                    self.main_lines = match std::fs::read_to_string(&path) {
                        Ok(contents) => contents.lines().map(str::to_string).collect(),
                        Err(err) => vec![format!("cannot read: {err}")],
                    };
                    if self.main_lines.is_empty() {
                        self.main_lines = vec!["(empty file)".into()];
                    }
                }
                Some(file) => {
                    self.main_title = format!(" {} ", file.path);
                    self.main_is_diff = true;
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
            self.shape_cursor();
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
                    // In the editor this is vim's "return to normal mode", so
                    // it must not tear the whole interface down.
                    KeyCode::Char('c') if !matches!(self.mode, Mode::Edit(_)) => {
                        self.quit = true;
                        continue;
                    }
                    // Half a pane at a time, as in a pager.
                    KeyCode::Char('d') => {
                        self.scroll_main(self.half_pane());
                        continue;
                    }
                    KeyCode::Char('u') if !matches!(self.mode, Mode::Edit(_)) => {
                        self.scroll_main(-self.half_pane());
                        continue;
                    }
                    // Commit and push from the message panel; `ctrl-l` keeps the
                    // commit local, for an offline machine or a repo with no
                    // remote yet.
                    KeyCode::Char('p')
                        if self.focus == Panel::Commit && matches!(self.mode, Mode::Browse) =>
                    {
                        self.commit_draft(true, terminal)?;
                        continue;
                    }
                    KeyCode::Char('l')
                        if self.focus == Panel::Commit && matches!(self.mode, Mode::Browse) =>
                    {
                        self.commit_draft(false, terminal)?;
                        continue;
                    }
                    _ => {}
                }
            }
            match &self.mode {
                Mode::Edit(_) => self.edit_key(key)?,
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

    /// Ask the terminal for the cursor shape this mode wants, when it differs
    /// from the one already set.
    fn shape_cursor(&mut self) {
        let wanted = match &self.mode {
            Mode::Edit(editor) => Shape::for_editor(editor.mode),
            // Outside the editor no cursor is shown, so leave the terminal's own
            // shape alone for whatever comes next.
            _ => Shape::Default,
        };
        if wanted != self.cursor_shape {
            // A terminal that ignores the request is not worth reporting: the
            // interface works the same either way.
            let _ = wanted.apply();
            self.cursor_shape = wanted;
        }
    }

    fn browse_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        if self.focus == Panel::Commit {
            return self.commit_panel_key(code, terminal);
        }
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

    /// The commit panel takes text, so only a few keys are commands here.
    fn commit_panel_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        match code {
            // Leaving keeps the draft: a message half-written is not lost by
            // looking at a diff.
            KeyCode::Esc => self.focus(Panel::Files),
            KeyCode::Tab => self.focus(self.focus.next()),
            KeyCode::BackTab => self.focus(self.focus.previous()),
            // The same thing `dotgit commit` does: commit, then push.
            KeyCode::Enter => self.commit_draft(true, terminal)?,
            KeyCode::Backspace => {
                self.message_draft.pop();
            }
            KeyCode::Char(c) => self.message_draft.push(c),
            _ => {}
        }
        Ok(())
    }

    /// Commit what is staged with the message in the panel, and optionally push.
    fn commit_draft(&mut self, push_after: bool, terminal: &mut DefaultTerminal) -> Result<()> {
        let message = self.message_draft.trim().to_string();
        if message.is_empty() {
            self.message = "write a commit message first".into();
            return Ok(());
        }

        let outcome = self.commit_message(&message);
        let committed = outcome.as_ref().ok().cloned();
        self.report("commit", outcome);
        let Some(committed) = committed else {
            return Ok(());
        };
        self.message_draft.clear();
        self.refresh()?;

        if !push_after {
            self.message = format!("{committed} (not pushed)");
            return Ok(());
        }
        if git::remote_url(&self.repo).is_err() {
            self.message = format!("{committed}; no remote to push to");
            return Ok(());
        }

        // A push that fails must not hide the commit that succeeded.
        let pushed = self.push_report(terminal)?;
        self.message = match pushed {
            Ok(where_to) => format!("{committed} and pushed to {where_to}"),
            Err(err) => format!("{committed}, but the push failed: {err:#}"),
        };
        self.refresh()
    }

    /// Commit exactly what is staged, so unstaging a file means it is left out.
    fn commit_message(&mut self, message: &str) -> Result<String> {
        if !git::commit_staged(&self.repo, message)? {
            return Err(anyhow!("nothing staged - stage files in panel 2 first"));
        }
        history::clear(&self.repo)?;
        let head = self.repo.head()?.peel_to_commit()?;
        let (id, _) = git::describe(&self.repo, head.id())?;
        Ok(format!("committed {id}"))
    }

    /// Keys that mean different things depending on which panel has focus.
    fn panel_key(&mut self, code: KeyCode, terminal: &mut DefaultTerminal) -> Result<()> {
        match (self.focus, code) {
            (_, KeyCode::Char('p')) => self.push(terminal)?,
            (_, KeyCode::Char('b')) | (Panel::Backups, KeyCode::Char('n')) => self.backup()?,
            (Panel::Status, KeyCode::Char('L')) => self.login(terminal)?,
            (Panel::Status, KeyCode::Char('e')) => self.open_editor(self.root.join("dotgit.toml")),
            (Panel::Status, KeyCode::Char('E')) => self.edit_config(terminal)?,

            (Panel::Files, KeyCode::Char('e')) => self.edit_selected(),
            (Panel::Files, KeyCode::Char('E')) => self.edit_file(terminal)?,

            (Panel::Files, KeyCode::Char(' ')) => self.toggle_stage()?,
            (Panel::Files, KeyCode::Char('a')) => self.stage_all()?,
            // No dialog: `c` moves to the message panel, where the message is
            // typed in place.
            (_, KeyCode::Char('c')) => self.focus(Panel::Commit),
            (Panel::Files, KeyCode::Char('u')) => self.ask(Input::UploadPath),
            (Panel::Files, KeyCode::Char('d')) => self.ask_discard(),

            (Panel::Versions, KeyCode::Enter) => self.jump()?,
            (Panel::Versions, KeyCode::Char('r')) => self.step(history::Direction::Older)?,
            (Panel::Versions, KeyCode::Char('f')) => self.step(history::Direction::Newer)?,
            (Panel::Versions, KeyCode::Char('D')) => self.ask_destroy()?,
            (Panel::Versions, KeyCode::Char('v')) => self.ask_revert(),

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
        // The editor owns the screen while it is open, so it owns the mouse too.
        if matches!(self.mode, Mode::Edit(_)) {
            self.edit_mouse(mouse);
            return;
        }
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
                let outcome = match kind {
                    Input::UploadPath => self.upload(&value),
                };
                self.report("upload", outcome);
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
            Pending::RevertCommit(revision) => ("revert", self.revert(&revision)),
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
        if !staged && !unstaged && !file.untracked {
            // A committed file with no changes has nothing to stage; say so
            // rather than appearing to do something.
            self.message = format!("{path} has no changes to stage");
            return Ok(());
        }
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
        // Only the changed files: the panel also lists committed ones, and
        // staging those would be a no-op with a misleading count.
        let paths: Vec<String> = self
            .files
            .iter()
            .filter(|f| f.staged || f.unstaged || f.untracked)
            .map(|f| f.path.clone())
            .collect();
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
        if !file.staged && !file.unstaged && !file.untracked {
            self.message = format!("{} has no changes to discard", file.path);
            return;
        }
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

    fn ask_revert(&mut self) {
        let Some(commit) = self.commits.get(self.selection()) else {
            return;
        };
        self.mode = Mode::Confirm {
            question: format!(
                "add a commit that undoes {} \"{}\"?",
                commit.id, commit.subject
            ),
            action: Pending::RevertCommit(commit.oid.to_string()),
        };
    }

    fn revert(&mut self, revision: &str) -> Result<String> {
        let message = ops::revert(&self.repo, revision)?;
        Ok(format!("{message} (press p to push)"))
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

    /// Clicks and wheel notches inside the editor, as neovim handles them with
    /// `mouse=a`: a click moves the cursor, the wheel scrolls the view.
    fn edit_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        let area = self.edit_area;
        let Mode::Edit(editor) = &mut self.mode else {
            return;
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => editor.scroll_view(3, area.height as usize),
            MouseEventKind::ScrollUp => editor.scroll_view(-3, area.height as usize),
            MouseEventKind::Down(_) => {
                // Ignore clicks outside the text, such as on the status line.
                if !area.contains(Position::new(mouse.column, mouse.row)) {
                    return;
                }
                let line = editor.scroll + (mouse.row - area.y) as usize;
                let column = (mouse.column - area.x) as usize;
                editor.click(line, column);
            }
            _ => {}
        }
    }

    /// Open the built-in editor on the selected file.
    fn edit_selected(&mut self) {
        let Some(file) = self.files.get(self.selection()) else {
            self.message = "no file selected".into();
            return;
        };
        let path = self.root.join(&file.path);
        if !path.exists() {
            self.message = format!("{} no longer exists on disk", file.path);
            return;
        }
        self.open_editor(path);
    }

    fn open_editor(&mut self, path: PathBuf) {
        match Editor::open(&path) {
            Ok(editor) => {
                self.message = format!("editing {} - :w writes, :q closes", editor.name());
                self.mode = Mode::Edit(Box::new(editor));
            }
            Err(err) => self.message = format!("error: {err:#}"),
        }
    }

    /// Hand a key to the open editor, then act on what it reports.
    fn edit_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Result<()> {
        let Mode::Edit(editor) = &mut self.mode else {
            return Ok(());
        };
        let Some(translated) = translate(key) else {
            return Ok(());
        };
        let outcome = editor.key(translated);
        let (name, path) = (editor.name(), editor.path.clone());

        if outcome.saved {
            // The file on disk changed, so the diff and the staging state shown
            // behind the editor are now stale.
            let logged: Result<String> = Ok(format!("wrote {name}"));
            self.report("edit", logged);
            let _ = path;
        }
        if outcome.closed {
            self.mode = Mode::Browse;
            self.message = format!("closed {name}");
        }
        if outcome.saved {
            self.refresh()?;
        }
        Ok(())
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

    /// Push and hand the result back, for callers that want to say what
    /// happened to the commit as well as the push.
    fn push_report(&mut self, terminal: &mut DefaultTerminal) -> Result<Result<String>> {
        let outcome = self.outside(terminal, |app| {
            ops::push(&app.repo, false).map(|report| match report {
                ops::PushReport::Ssh { host } => host,
                ops::PushReport::Http { host, username } => format!("{host} as {username}"),
                ops::PushReport::Local { target } => target,
            })
        })?;
        if let Err(err) = &outcome {
            let failed: Result<String> = Err(anyhow!("{err:#}"));
            self.report("push", failed);
        } else {
            let ok: Result<String> = Ok(String::new());
            self.report("push", ok);
        }
        Ok(outcome)
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
        // An external program should not inherit the editor's bar cursor.
        let _ = Shape::Default.apply();
        self.cursor_shape = Shape::Default;
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
        // The editor is not a panel: while it is open it is the whole screen,
        // the way opening a file in vim replaces what you were looking at.
        if let Mode::Edit(editor) = &mut self.mode {
            let area = frame.area();
            // One row goes to the status line beneath the buffer.
            let text_height = area.height.saturating_sub(1) as usize;
            editor.follow(text_height);
            self.edit_area = draw_editor(frame, area, editor);
            return;
        }

        let rows = Layout::vertical([
            Constraint::Min(6),
            // The commit message: one line of text between its borders.
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .split(frame.area());
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
        self.list_areas = [side[0], side[1], side[2], side[3], rows[1]];
        // A pane that has shrunk may leave the view scrolled past the end.
        self.scroll = self.scroll.min(self.max_scroll());

        self.draw_status(frame, side[0]);
        self.draw_files(frame, side[1]);
        self.draw_versions(frame, side[2]);
        self.draw_backups(frame, side[3]);
        self.draw_main(frame, columns[1]);
        self.draw_commit(frame, rows[1]);
        self.draw_footer(frame, rows[2]);

        match &self.mode {
            Mode::Edit(_) => {}
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
                let unchanged = !file.staged && !file.unstaged && !file.untracked;
                let colour = if unchanged {
                    Color::DarkGray
                } else if file.staged && !file.unstaged {
                    Color::Green
                } else if file.staged {
                    Color::Yellow
                } else {
                    Color::Red
                };
                let path = if unchanged {
                    // Committed files recede, so the changed ones still stand out.
                    Span::styled(file.path.clone(), Style::new().fg(Color::Gray))
                } else {
                    Span::raw(file.path.clone())
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", file.label), Style::new().fg(colour)),
                    path,
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
                if !self.main_is_diff {
                    // A file's own contents, so highlight them as the language
                    // they are rather than as a patch.
                    return Line::from(
                        highlight::highlight(self.main_language, line)
                            .into_iter()
                            .map(|span| Span::styled(span.text, style_for(span.kind)))
                            .collect::<Vec<_>>(),
                    );
                }
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

    /// The commit message panel: the text being written, or a hint when empty.
    fn draw_commit(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Panel::Commit;
        let line = if self.message_draft.is_empty() && !focused {
            Line::from(Span::styled(
                "press c to write a commit message",
                Style::new().fg(Color::DarkGray),
            ))
        } else {
            let mut spans = vec![Span::raw(self.message_draft.clone())];
            if focused {
                // A visible caret, since the terminal cursor is not used here.
                spans.push(Span::styled("_", Style::new().fg(Color::Cyan).bold()));
            }
            Line::from(spans)
        };
        frame.render_widget(Paragraph::new(line).block(self.block(Panel::Commit)), area);
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

/// Turn a terminal key event into one of the editor's keys. Anything it has no
/// meaning for is dropped rather than guessed at.
fn translate(key: ratatui::crossterm::event::KeyEvent) -> Option<editor::Key> {
    use editor::Key;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('r') => Some(Key::Redo),
            KeyCode::Char('d') => Some(Key::HalfPageDown),
            KeyCode::Char('u') => Some(Key::HalfPageUp),
            // vim treats ctrl-c as "back to normal mode".
            KeyCode::Char('c') => Some(Key::Esc),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char(c) => Some(Key::Char(c)),
        KeyCode::Esc => Some(Key::Esc),
        KeyCode::Enter => Some(Key::Enter),
        KeyCode::Backspace => Some(Key::Backspace),
        KeyCode::Tab => Some(Key::Tab),
        KeyCode::Left => Some(Key::Left),
        KeyCode::Right => Some(Key::Right),
        KeyCode::Up => Some(Key::Up),
        KeyCode::Down => Some(Key::Down),
        KeyCode::Home => Some(Key::Home),
        KeyCode::End => Some(Key::End),
        KeyCode::PageDown => Some(Key::HalfPageDown),
        KeyCode::PageUp => Some(Key::HalfPageUp),
        _ => None,
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

/// Draw the buffer, a gutter of line numbers, and vim's status line. Returns
/// the rectangle the text itself occupies, which is what turns a click into a
/// line and column.
fn draw_editor(frame: &mut Frame, area: Rect, editor: &Editor) -> Rect {
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let gutter_width = format!("{}", editor.lines.len()).len().max(3) as u16 + 1;
    let columns =
        Layout::horizontal([Constraint::Length(gutter_width), Constraint::Min(1)]).split(rows[0]);

    let height = rows[0].height as usize;
    let visible = editor
        .lines
        .iter()
        .enumerate()
        .skip(editor.scroll)
        .take(height);

    let language = highlight::language_for(&editor.path);
    let mut numbers: Vec<Line> = Vec::new();
    let mut text: Vec<Line> = Vec::new();
    for (index, line) in visible {
        let current = index == editor.line;
        numbers.push(Line::from(Span::styled(
            format!("{:>width$} ", index + 1, width = gutter_width as usize - 1),
            if current {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::DarkGray)
            },
        )));
        // Highlight per visible line: nothing off-screen is ever tokenised.
        text.push(Line::from(
            highlight::highlight(language, line)
                .into_iter()
                .map(|span| Span::styled(span.text, style_for(span.kind)))
                .collect::<Vec<_>>(),
        ));
    }

    frame.render_widget(Paragraph::new(numbers), columns[0]);
    frame.render_widget(Paragraph::new(text), columns[1]);

    // The status line: mode on the left, position on the right, as in vim.
    let mode = editor.mode.label();
    let modified = if editor.modified { " [+]" } else { "" };
    let left = format!(" {mode}  {}{modified}  {}", editor.name(), editor.status());
    let right = format!("{},{} ", editor.line + 1, editor.column + 1);
    let padding =
        (rows[1].width as usize).saturating_sub(left.chars().count() + right.chars().count());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                left,
                match editor.mode {
                    editor::Mode::Insert => Style::new().fg(Color::Black).bg(Color::Green),
                    editor::Mode::Command => Style::new().fg(Color::Black).bg(Color::Cyan),
                    editor::Mode::Normal => Style::new().fg(Color::Black).bg(Color::Gray),
                },
            ),
            Span::raw(" ".repeat(padding)),
            Span::styled(right, Style::new().fg(Color::DarkGray)),
        ])),
        rows[1],
    );

    // Put the terminal's own cursor where the editor's is, so it blinks in the
    // right place and follows the mode's shape.
    let row = (editor.line - editor.scroll) as u16;
    let column = editor.column as u16;
    if row < columns[1].height {
        frame.set_cursor_position(Position::new(
            columns[1].x + column.min(columns[1].width.saturating_sub(1)),
            columns[1].y + row,
        ));
    }
    columns[1]
}

/// Colours for the highlighter's token kinds, following the conventions a
/// neovim user expects: comments recede, strings and numbers stand out, and
/// keywords carry the structure.
fn style_for(kind: Kind) -> Style {
    match kind {
        Kind::Comment => Style::new().fg(Color::DarkGray).italic(),
        Kind::Str => Style::new().fg(Color::Green),
        Kind::Number => Style::new().fg(Color::Magenta),
        Kind::Keyword => Style::new().fg(Color::Blue).bold(),
        Kind::Constant => Style::new().fg(Color::Yellow),
        Kind::Key => Style::new().fg(Color::Cyan),
        Kind::Section => Style::new().fg(Color::Yellow).bold(),
        Kind::Punctuation => Style::new().fg(Color::Gray),
        Kind::Plain => Style::new(),
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
