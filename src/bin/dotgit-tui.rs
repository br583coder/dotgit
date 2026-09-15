//! `dotgit-tui`: an optional full-screen browser for a dotfiles repository.
//!
//! Everything here is a front end to the same functions the `dotgit` command
//! calls, so the two can never disagree about what an action does. The TUI is
//! entirely optional: it ships as a separate binary behind a non-default
//! feature, the `dotgit` command never launches it, and every action it offers
//! has a command line equivalent printed in the help pane.

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

/// One row of the commit list.
struct Commit {
    oid: git2::Oid,
    id: String,
    subject: String,
    author: String,
    date: String,
}

/// What the interface is currently doing.
enum Mode {
    Browse,
    /// Collecting a line of text for `kind`.
    Input {
        kind: Input,
        buffer: String,
    },
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
            Input::CommitMessage => "Enter commits (locally; press p to push)  Esc cancels",
            Input::UploadPath => "e.g. ~/.config/hypr   Enter uploads  Esc cancels",
        }
    }
}

struct App {
    repo: Repository,
    root: PathBuf,
    branch: String,
    remote: String,
    commits: Vec<Commit>,
    /// Index of the version currently checked out.
    position: usize,
    selected: usize,
    dirty: bool,
    backups: usize,
    status: String,
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
            commits: Vec::new(),
            position: 0,
            selected: 0,
            dirty: false,
            backups: 0,
            status: "welcome - press ? for keys, q to quit".into(),
            mode: Mode::Browse,
            quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Re-read everything the panes display. Called after every action, so the
    /// screen can never show a stale version or status.
    fn refresh(&mut self) -> Result<()> {
        self.branch = git::current_branch(&self.repo).unwrap_or_else(|_| "(detached)".into());
        self.remote = git::remote_url(&self.repo)
            .ok()
            .and_then(|url| git::host_of(&url))
            .unwrap_or_else(|| "(no remote)".into());

        let chain = history::chain(&self.repo)?;
        self.commits = chain
            .iter()
            .map(|oid| self.describe(*oid))
            .collect::<Result<Vec<_>>>()?;

        let position = history::current(&self.repo)?;
        self.position = position.index;
        self.selected = self.selected.min(self.commits.len().saturating_sub(1));
        self.dirty = git::has_changes_against(&self.repo, position.oid)?;
        // Backups of other repositories share the default directory, so count
        // only the ones belonging to this one.
        let name = self
            .root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        self.backups = backup::default_dir()
            .and_then(|dir| backup::list(&dir))
            .map(|list| {
                list.iter()
                    .filter(|b| b.repo.as_deref() == Some(name.as_str()))
                    .count()
            })
            .unwrap_or(0);
        Ok(())
    }

    fn describe(&self, oid: git2::Oid) -> Result<Commit> {
        let commit = self.repo.find_commit(oid)?;
        let (id, subject) = git::describe(&self.repo, oid)?;
        Ok(Commit {
            oid,
            id,
            subject,
            author: commit.author().name().unwrap_or("unknown").to_string(),
            date: backup::format_timestamp(commit.time().seconds().max(0) as u64),
        })
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            // Windows reports press and release; only act on the press.
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                self.quit = true;
                continue;
            }
            match &mut self.mode {
                Mode::Help => self.mode = Mode::Browse,
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
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.commits.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') => self.selected = 0,
            KeyCode::Char('G') => self.selected = self.commits.len().saturating_sub(1),
            KeyCode::Char('r') => self.step(history::Direction::Older)?,
            KeyCode::Char('f') => self.step(history::Direction::Newer)?,
            KeyCode::Char('b') => self.backup()?,
            KeyCode::Char('c') => self.ask(Input::CommitMessage),
            KeyCode::Char('u') => self.ask(Input::UploadPath),
            KeyCode::Char('p') => self.push(terminal)?,
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        Ok(())
    }

    fn input_key(&mut self, code: KeyCode, kind: Input, mut buffer: String) -> Result<()> {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.status = "cancelled".into();
            }
            KeyCode::Enter => {
                self.mode = Mode::Browse;
                self.submit(kind, buffer.trim().to_string())?;
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

    fn ask(&mut self, kind: Input) {
        self.mode = Mode::Input {
            kind,
            buffer: String::new(),
        };
    }

    fn submit(&mut self, kind: Input, value: String) -> Result<()> {
        if value.is_empty() {
            self.status = "nothing entered".into();
            return Ok(());
        }
        // An action that fails is reported in the status bar rather than
        // tearing down the interface: the user can read it and try something
        // else, exactly as they would after a failed command.
        let outcome = match kind {
            Input::CommitMessage => self.commit(&value),
            Input::UploadPath => self.upload(&value),
        };
        self.report(outcome);
        self.refresh()
    }

    fn report(&mut self, outcome: Result<String>) {
        self.status = match outcome {
            Ok(message) => message,
            Err(err) => format!("error: {err:#}"),
        };
    }

    fn commit(&mut self, message: &str) -> Result<String> {
        if !git::create_commit(&self.repo, message)? {
            return Ok("nothing to commit, working tree clean".into());
        }
        // The commit just made is the newest version, so any stepped-back
        // cursor no longer applies - the same rule the CLI follows.
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

    fn step(&mut self, direction: history::Direction) -> Result<()> {
        let outcome = ops::step(&self.repo, direction).map(|report| match report {
            ops::StepReport::Boundary => match direction {
                history::Direction::Older => "oldest change reached".to_string(),
                history::Direction::Newer => "newest change released".to_string(),
            },
            ops::StepReport::Moved(position) => {
                let verb = match direction {
                    history::Direction::Older => "restored to",
                    history::Direction::Newer => "moved forward to",
                };
                format!(
                    "{verb} version {} of {}",
                    position.index + 1,
                    position.total
                )
            }
        });
        self.report(outcome);
        // Follow the cursor, so the list shows where the working tree now is.
        self.refresh()?;
        self.selected = self.position;
        Ok(())
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

    /// Push, giving the terminal back first: a push may hand over to
    /// `gh auth login`, which needs the ordinary screen to talk to the user.
    fn push(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        ratatui::restore();
        let outcome = ops::push(&self.repo, false).map(|report| match report {
            ops::PushReport::Ssh { host } => format!("pushed to {host}"),
            ops::PushReport::Http { host, username } => format!("pushed to {host} as {username}"),
            ops::PushReport::Local { target } => format!("pushed to {target}"),
        });
        *terminal = ratatui::init();
        terminal.clear()?;
        self.report(outcome);
        self.refresh()
    }

    fn draw(&self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(5),
            // Two lines of text plus the borders above and below them.
            Constraint::Length(4),
        ])
        .split(frame.area());

        self.draw_header(frame, areas[0]);
        let panes = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(areas[1]);
        self.draw_commits(frame, panes[0]);
        self.draw_details(frame, panes[1]);
        self.draw_footer(frame, areas[2]);

        match &self.mode {
            Mode::Input { kind, buffer } => self.draw_input(frame, *kind, buffer),
            Mode::Help => self.draw_help(frame),
            Mode::Browse => {}
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
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
        let state = if self.dirty {
            Span::styled("uncommitted changes", Style::new().fg(Color::Yellow).bold())
        } else {
            Span::styled("clean", Style::new().fg(Color::Green))
        };
        let text = vec![
            Line::from(vec![
                Span::styled(self.root.display().to_string(), Style::new().bold()),
                Span::raw("  "),
                Span::styled(format!("[{}]", self.branch), Style::new().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::raw(format!("remote {}   ", self.remote)),
                Span::raw(format!("{version}   ")),
                state,
                Span::raw(format!("   {} backup(s)", self.backups)),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" dotgit ")),
            area,
        );
    }

    fn draw_commits(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .commits
            .iter()
            .enumerate()
            .map(|(index, commit)| {
                // The marker is the point of this pane: it shows which version
                // the working tree currently holds, not merely where HEAD is.
                let (marker, style) = if index == self.position {
                    (">", Style::new().fg(Color::Green).bold())
                } else {
                    (" ", Style::new())
                };
                let head = if index == 0 { " (newest)" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{marker} "), style),
                    Span::styled(commit.id.clone(), Style::new().fg(Color::Yellow)),
                    Span::raw(format!(" {}{head}", commit.subject)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" versions (newest first) "),
                )
                .highlight_style(Style::new().reversed()),
            area,
            &mut state,
        );
    }

    fn draw_details(&self, frame: &mut Frame, area: Rect) {
        let text = match self.commits.get(self.selected) {
            None => vec![Line::from("no commits yet")],
            Some(commit) => vec![
                Line::from(Span::styled(commit.subject.clone(), Style::new().bold())),
                Line::from(""),
                Line::from(format!("commit  {}", commit.oid)),
                Line::from(format!("author  {}", commit.author)),
                Line::from(format!("date    {} UTC", commit.date)),
                Line::from(""),
                Line::from(if self.selected == self.position {
                    "This is the version in your working tree."
                } else if self.selected > self.position {
                    "Older than the working tree: press r to step back."
                } else {
                    "Newer than the working tree: press f to step forward."
                }),
            ],
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title(" details ")),
            area,
        );
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let keys = "r back  f forward  c commit  u upload  p push  b backup  ? keys  q quit";
        let text = vec![
            Line::from(Span::styled(
                self.status.clone(),
                Style::new().fg(Color::Cyan),
            )),
            Line::from(Span::styled(keys, Style::new().fg(Color::DarkGray))),
        ];
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_input(&self, frame: &mut Frame, kind: Input, buffer: &str) {
        let area = centred(frame.area(), 70, 5);
        frame.render_widget(Clear, area);
        let text = vec![
            Line::from(vec![
                Span::raw("> "),
                Span::styled(buffer.to_string(), Style::new().bold()),
                Span::styled("_", Style::new().fg(Color::DarkGray)),
            ]),
            Line::from(Span::styled(kind.hint(), Style::new().fg(Color::DarkGray))),
        ];
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(kind.title())),
            area,
        );
    }

    fn draw_help(&self, frame: &mut Frame) {
        // Each line names the command that does the same thing, so the TUI
        // stays a convenience rather than the only way to work.
        let rows = [
            ("j / k", "move through the versions", ""),
            ("r", "step back one version", "dotgit restore"),
            ("f", "step forward one version", "dotgit rebase"),
            ("c", "commit the working tree", "dotgit commit"),
            ("u", "upload a path into the repo", "dotgit upload <path>"),
            ("p", "push to the remote", "dotgit commit"),
            ("b", "back up the history locally", "dotgit backup"),
            ("q", "quit", ""),
        ];
        let mut text = vec![
            Line::from(Span::styled(
                "Every action here has a command line equivalent:",
                Style::new().bold(),
            )),
            Line::from(""),
        ];
        text.extend(rows.iter().map(|(key, what, command)| {
            Line::from(vec![
                Span::styled(format!("  {key:<6}"), Style::new().fg(Color::Yellow)),
                Span::raw(format!("{what:<30}")),
                Span::styled(command.to_string(), Style::new().fg(Color::DarkGray)),
            ])
        }));
        text.push(Line::from(""));
        text.push(Line::from(Span::styled(
            "press any key to close",
            Style::new().fg(Color::DarkGray),
        )));

        let area = centred(frame.area(), 74, (text.len() + 2) as u16);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" keys ")),
            area,
        );
    }
}

/// A box of the given size in the middle of `area`, clamped so it still fits
/// on a small terminal.
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

/// Reject arguments rather than silently ignoring them: the TUI takes none,
/// and anyone passing some is looking for the command line tool.
fn reject_arguments(args: &[String]) -> Result<()> {
    match args.first() {
        None => Ok(()),
        Some(arg) => Err(anyhow!(
            "dotgit-tui takes no arguments (got `{arg}`) - run `dotgit {arg}` instead"
        )),
    }
}
