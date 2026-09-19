//! A small modal text editor, built on vim's principles rather than wrapping an
//! external one.
//!
//! The rules it keeps, because they are what make vim feel like vim:
//!
//! - Normal mode is where you start, and keys are commands rather than text.
//! - The cursor rests *on* a character in normal mode, and may sit one past the
//!   end of the line in insert mode.
//! - Commands take a count, so `3j`, `5x` and `2dd` mean what you expect.
//! - Operators wait for a second key: `dd`, `dw`, `yy`, `gg`.
//! - An entire insert session is one undo step, not one step per keystroke.
//! - `:` opens a command line, and `:q` refuses to discard unsaved changes
//!   unless you add `!`.
//!
//! None of this module knows about terminals or drawing: it takes keys and
//! reports what changed, which is what lets the behaviour be tested directly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Keys the editor understands, named independently of any terminal library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Esc,
    Enter,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// `ctrl-r`, `ctrl-d`, `ctrl-u`.
    Redo,
    HalfPageDown,
    HalfPageUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Typing a `:` command.
    Command,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Command => "COMMAND",
        }
    }
}

/// What a keypress did that the surrounding interface needs to know about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The buffer was written to disk, so anything showing the file is stale.
    pub saved: bool,
    /// The editor should be closed.
    pub closed: bool,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    line: usize,
    column: usize,
}

pub struct Editor {
    pub path: PathBuf,
    /// Lines without their terminators.
    pub lines: Vec<String>,
    pub line: usize,
    pub column: usize,
    pub mode: Mode,
    pub modified: bool,
    /// First visible line, kept in step with the cursor by [`Editor::follow`].
    pub scroll: usize,
    /// The `:` line being typed, or the last message shown.
    pub command: String,
    pub message: String,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Lines held by `yy`/`dd`, pasted by `p`.
    yanked: Vec<String>,
    /// A count being typed, as in `12j`.
    count: Option<usize>,
    /// An operator waiting for its second key: `d`, `y`, `c` or `g`.
    operator: Option<char>,
    /// Whether the current insert session already has an undo snapshot, so the
    /// whole session undoes as one step rather than one step per keystroke.
    insert_group: bool,
    /// Whether the file ended with a newline, so writing it back does not
    /// silently add or remove one.
    trailing_newline: bool,
}

impl Editor {
    /// Open a file, or start an empty buffer if it does not exist yet.
    pub fn open(path: &Path) -> Result<Self> {
        let (contents, existed) = match std::fs::read_to_string(path) {
            Ok(contents) => (contents, true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let trailing_newline = contents.ends_with('\n') || !existed;
        let mut lines: Vec<String> = contents.lines().map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Ok(Self {
            path: path.to_path_buf(),
            lines,
            line: 0,
            column: 0,
            mode: Mode::Normal,
            modified: false,
            scroll: 0,
            command: String::new(),
            message: if existed {
                format!("\"{}\" {} lines", path.display(), 0)
            } else {
                format!("\"{}\" [New]", path.display())
            },
            undo: Vec::new(),
            redo: Vec::new(),
            yanked: Vec::new(),
            count: None,
            operator: None,
            insert_group: false,
            trailing_newline,
        })
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    /// Feed one key to the editor.
    pub fn key(&mut self, key: Key) -> Outcome {
        match self.mode {
            Mode::Insert => self.insert_key(key),
            Mode::Command => self.command_key(key),
            Mode::Normal => self.normal_key(key),
        }
    }

    // ----- normal mode -----

    fn normal_key(&mut self, key: Key) -> Outcome {
        let count = self.count.unwrap_or(1);
        match key {
            Key::Esc => {
                // Abandon a half-typed count or operator, as vim does.
                self.count = None;
                self.operator = None;
            }
            Key::Char(c @ '0'..='9') if !(c == '0' && self.count.is_none()) => {
                let digit = c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + digit);
                return Outcome::default();
            }
            Key::Char(c) => return self.normal_char(c, count),
            Key::Left => self.move_left(count),
            Key::Right => self.move_right(count),
            Key::Down => self.move_down(count),
            Key::Up => self.move_up(count),
            Key::Home => self.column = 0,
            Key::End => self.column = self.last_column(),
            Key::Enter => self.move_down(count),
            Key::Backspace => self.move_left(count),
            Key::HalfPageDown => self.move_down(10 * count),
            Key::HalfPageUp => self.move_up(10 * count),
            Key::Redo => self.redo(),
            Key::Tab => {}
        }
        self.count = None;
        Outcome::default()
    }

    fn normal_char(&mut self, c: char, count: usize) -> Outcome {
        // An operator is pending, so this key is its target.
        if let Some(operator) = self.operator.take() {
            self.apply_operator(operator, c, count);
            self.count = None;
            return Outcome::default();
        }

        match c {
            'h' => self.move_left(count),
            'l' => self.move_right(count),
            'j' => self.move_down(count),
            'k' => self.move_up(count),
            '0' => self.column = 0,
            '$' => self.column = self.last_column(),
            '^' => self.column = self.first_non_blank(),
            'w' => self.move_word_forward(count),
            'b' => self.move_word_back(count),
            'G' => {
                // `G` goes to the last line, `{count}G` to that line.
                self.line = match self.count {
                    Some(n) => n.saturating_sub(1).min(self.lines.len() - 1),
                    None => self.lines.len() - 1,
                };
                self.clamp_column();
            }
            'i' => self.enter_insert(),
            'a' => {
                self.enter_insert();
                self.column = (self.column + 1).min(self.lines[self.line].chars().count());
            }
            'I' => {
                self.column = self.first_non_blank();
                self.enter_insert();
            }
            'A' => {
                self.enter_insert();
                self.column = self.lines[self.line].chars().count();
            }
            'o' => {
                self.enter_insert();
                self.lines.insert(self.line + 1, String::new());
                self.line += 1;
                self.column = 0;
                self.modified = true;
            }
            'O' => {
                self.enter_insert();
                self.lines.insert(self.line, String::new());
                self.column = 0;
                self.modified = true;
            }
            'x' => self.delete_chars(count),
            'D' => {
                self.snapshot();
                let line = &mut self.lines[self.line];
                let byte = char_to_byte(line, self.column);
                line.truncate(byte);
                self.modified = true;
                self.clamp_column();
            }
            'p' => self.paste(count, true),
            'P' => self.paste(count, false),
            'u' => self.undo(),
            'd' | 'y' | 'c' | 'g' => {
                self.operator = Some(c);
                // Keep the count: `2dd` deletes two lines.
                return Outcome::default();
            }
            ':' => {
                self.mode = Mode::Command;
                self.command = String::new();
                self.message.clear();
            }
            _ => {}
        }
        self.count = None;
        Outcome::default()
    }

    /// The second key of a two-key command.
    fn apply_operator(&mut self, operator: char, target: char, count: usize) {
        match (operator, target) {
            ('d', 'd') => self.delete_lines(count),
            ('d', 'w') => {
                self.snapshot();
                for _ in 0..count {
                    self.delete_word();
                }
            }
            ('d', '$') => {
                self.snapshot();
                let line = &mut self.lines[self.line];
                let byte = char_to_byte(line, self.column);
                line.truncate(byte);
                self.modified = true;
                self.clamp_column();
            }
            ('y', 'y') => {
                let end = (self.line + count).min(self.lines.len());
                self.yanked = self.lines[self.line..end].to_vec();
                self.message = format!("{} line(s) yanked", self.yanked.len());
            }
            ('c', 'c') => {
                self.enter_insert();
                self.lines[self.line].clear();
                self.column = 0;
                self.mode = Mode::Insert;
                self.modified = true;
            }
            ('g', 'g') => {
                self.line = match self.count {
                    Some(n) => n.saturating_sub(1).min(self.lines.len() - 1),
                    None => 0,
                };
                self.clamp_column();
            }
            _ => self.message = format!("unknown command {operator}{target}"),
        }
    }

    // ----- insert mode -----

    fn insert_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                self.insert_group = false;
                // Vim steps the cursor back onto the last character.
                self.column = self.column.saturating_sub(1);
                self.clamp_column();
            }
            Key::Char(c) => self.insert_char(c),
            Key::Tab => {
                for _ in 0..4 {
                    self.insert_char(' ');
                }
            }
            Key::Enter => {
                self.snapshot_once();
                let line = self.lines[self.line].clone();
                let byte = char_to_byte(&line, self.column);
                let (left, right) = line.split_at(byte);
                self.lines[self.line] = left.to_string();
                self.lines.insert(self.line + 1, right.to_string());
                self.line += 1;
                self.column = 0;
                self.modified = true;
            }
            Key::Backspace => {
                self.snapshot_once();
                if self.column > 0 {
                    let byte = char_to_byte(&self.lines[self.line], self.column - 1);
                    self.lines[self.line].remove(byte);
                    self.column -= 1;
                } else if self.line > 0 {
                    // Joining onto the previous line leaves the cursor at the seam.
                    let current = self.lines.remove(self.line);
                    self.line -= 1;
                    self.column = self.lines[self.line].chars().count();
                    self.lines[self.line].push_str(&current);
                }
                self.modified = true;
            }
            Key::Left => self.column = self.column.saturating_sub(1),
            Key::Right => {
                self.column = (self.column + 1).min(self.lines[self.line].chars().count());
            }
            Key::Down => {
                self.move_down(1);
                self.column = self.column.min(self.lines[self.line].chars().count());
            }
            Key::Up => {
                self.move_up(1);
                self.column = self.column.min(self.lines[self.line].chars().count());
            }
            Key::Home => self.column = 0,
            Key::End => self.column = self.lines[self.line].chars().count(),
            Key::Redo | Key::HalfPageDown | Key::HalfPageUp => {}
        }
        Outcome::default()
    }

    fn insert_char(&mut self, c: char) {
        self.snapshot_once();
        let byte = char_to_byte(&self.lines[self.line], self.column);
        self.lines[self.line].insert(byte, c);
        self.column += 1;
        self.modified = true;
    }

    // ----- command line -----

    fn command_key(&mut self, key: Key) -> Outcome {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
            }
            Key::Char(c) => self.command.push(c),
            Key::Backspace => {
                if self.command.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            Key::Enter => {
                let command = std::mem::take(&mut self.command);
                self.mode = Mode::Normal;
                return self.run_command(&command);
            }
            _ => {}
        }
        Outcome::default()
    }

    fn run_command(&mut self, command: &str) -> Outcome {
        let force = command.ends_with('!');
        let name = command.trim_end_matches('!');
        match name {
            "w" => match self.save() {
                Ok(()) => Outcome {
                    saved: true,
                    closed: false,
                },
                Err(err) => {
                    self.message = format!("{err:#}");
                    Outcome::default()
                }
            },
            "wq" | "x" => match self.save() {
                Ok(()) => Outcome {
                    saved: true,
                    closed: true,
                },
                Err(err) => {
                    self.message = format!("{err:#}");
                    Outcome::default()
                }
            },
            "q" => {
                if self.modified && !force {
                    // Vim's own refusal, because losing an edit to a stray key
                    // is worse than an extra keystroke.
                    self.message =
                        "E37: No write since last change (add ! to override)".to_string();
                    return Outcome::default();
                }
                Outcome {
                    saved: false,
                    closed: true,
                }
            }
            "" => Outcome::default(),
            other => {
                // `:42` jumps to a line, as in vim.
                if let Ok(number) = other.parse::<usize>() {
                    self.line = number.saturating_sub(1).min(self.lines.len() - 1);
                    self.clamp_column();
                } else {
                    self.message = format!("E492: Not an editor command: {other}");
                }
                Outcome::default()
            }
        }
    }

    pub fn save(&mut self) -> Result<()> {
        let mut contents = self.lines.join("\n");
        if self.trailing_newline {
            contents.push('\n');
        }
        std::fs::write(&self.path, contents)
            .with_context(|| format!("cannot write {}", self.path.display()))?;
        self.modified = false;
        self.message = format!("\"{}\" {}L written", self.path.display(), self.lines.len());
        Ok(())
    }

    // ----- motion -----

    fn move_left(&mut self, count: usize) {
        self.column = self.column.saturating_sub(count);
    }

    fn move_right(&mut self, count: usize) {
        self.column = (self.column + count).min(self.last_column());
    }

    fn move_down(&mut self, count: usize) {
        self.line = (self.line + count).min(self.lines.len() - 1);
        self.clamp_column();
    }

    fn move_up(&mut self, count: usize) {
        self.line = self.line.saturating_sub(count);
        self.clamp_column();
    }

    /// The last column the cursor may rest on in normal mode: on the final
    /// character, not past it, and column 0 on an empty line.
    fn last_column(&self) -> usize {
        self.lines[self.line].chars().count().saturating_sub(1)
    }

    fn first_non_blank(&self) -> usize {
        self.lines[self.line]
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0)
    }

    fn clamp_column(&mut self) {
        self.column = self.column.min(self.last_column());
    }

    fn move_word_forward(&mut self, count: usize) {
        for _ in 0..count {
            let chars: Vec<char> = self.lines[self.line].chars().collect();
            let mut column = self.column;
            // Leave the current word, then any space before the next one.
            while column < chars.len() && !chars[column].is_whitespace() {
                column += 1;
            }
            while column < chars.len() && chars[column].is_whitespace() {
                column += 1;
            }
            if column >= chars.len() {
                // Past the end of the line: carry on at the start of the next.
                if self.line + 1 < self.lines.len() {
                    self.line += 1;
                    self.column = self.first_non_blank();
                    continue;
                }
                self.column = self.last_column();
                continue;
            }
            self.column = column;
        }
    }

    fn move_word_back(&mut self, count: usize) {
        for _ in 0..count {
            if self.column == 0 {
                if self.line == 0 {
                    continue;
                }
                self.line -= 1;
                self.column = self.last_column();
                continue;
            }
            let chars: Vec<char> = self.lines[self.line].chars().collect();
            let mut column = self.column - 1;
            while column > 0 && chars[column].is_whitespace() {
                column -= 1;
            }
            while column > 0 && !chars[column - 1].is_whitespace() {
                column -= 1;
            }
            self.column = column;
        }
    }

    // ----- changes -----

    fn delete_chars(&mut self, count: usize) {
        if self.lines[self.line].is_empty() {
            return;
        }
        self.snapshot();
        for _ in 0..count {
            let line = &self.lines[self.line];
            if self.column >= line.chars().count() {
                break;
            }
            let byte = char_to_byte(line, self.column);
            self.lines[self.line].remove(byte);
        }
        self.modified = true;
        self.clamp_column();
    }

    fn delete_lines(&mut self, count: usize) {
        self.snapshot();
        let end = (self.line + count).min(self.lines.len());
        // Deleted lines are yanked, so `dd` then `p` moves a line.
        self.yanked = self.lines[self.line..end].to_vec();
        self.lines.drain(self.line..end);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.line = self.line.min(self.lines.len() - 1);
        self.column = 0;
        self.modified = true;
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.lines[self.line].chars().collect();
        let mut end = self.column;
        while end < chars.len() && !chars[end].is_whitespace() {
            end += 1;
        }
        while end < chars.len() && chars[end].is_whitespace() {
            end += 1;
        }
        let from = char_to_byte(&self.lines[self.line], self.column);
        let to = char_to_byte(&self.lines[self.line], end);
        self.lines[self.line].replace_range(from..to, "");
        self.modified = true;
        self.clamp_column();
    }

    fn paste(&mut self, count: usize, after: bool) {
        if self.yanked.is_empty() {
            return;
        }
        self.snapshot();
        let at = if after { self.line + 1 } else { self.line };
        let mut inserted = 0;
        for _ in 0..count {
            for (offset, text) in self.yanked.iter().enumerate() {
                self.lines.insert(at + inserted + offset, text.clone());
            }
            inserted += self.yanked.len();
        }
        self.line = at;
        self.column = 0;
        self.modified = true;
    }

    // ----- undo -----

    fn snapshot(&mut self) {
        self.redo.clear();
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            line: self.line,
            column: self.column,
        });
        // Keeping every step of a long session would grow without bound.
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
    }

    /// Snapshot only at the start of an insert session, so undoing a typed word
    /// removes the word rather than its last letter.
    fn snapshot_once(&mut self) {
        if !self.insert_group {
            self.snapshot();
            self.insert_group = true;
        }
    }

    fn enter_insert(&mut self) {
        self.snapshot();
        self.insert_group = true;
        self.mode = Mode::Insert;
    }

    fn undo(&mut self) {
        match self.undo.pop() {
            None => self.message = "already at the oldest change".to_string(),
            Some(snapshot) => {
                self.redo.push(Snapshot {
                    lines: self.lines.clone(),
                    line: self.line,
                    column: self.column,
                });
                self.restore(snapshot);
            }
        }
    }

    fn redo(&mut self) {
        match self.redo.pop() {
            None => self.message = "already at the newest change".to_string(),
            Some(snapshot) => {
                self.undo.push(Snapshot {
                    lines: self.lines.clone(),
                    line: self.line,
                    column: self.column,
                });
                self.restore(snapshot);
            }
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        self.line = snapshot.line.min(self.lines.len() - 1);
        self.column = snapshot.column;
        self.clamp_column();
        self.modified = true;
    }

    /// Put the cursor where the mouse was clicked, given a position already
    /// translated into the buffer's own coordinates.
    ///
    /// Clicking past the end of a line lands on its last character, and
    /// clicking below the final line lands on that line, which is what every
    /// editor does and what neovim does with `mouse=a`.
    pub fn click(&mut self, line: usize, column: usize) {
        self.line = line.min(self.lines.len() - 1);
        let width = self.lines[self.line].chars().count();
        self.column = match self.mode {
            // Insert mode may rest one past the end; normal mode may not.
            Mode::Insert => column.min(width),
            _ => column.min(width.saturating_sub(1)),
        };
    }

    /// Scroll the view by `delta` lines without moving the cursor, dragging the
    /// cursor along only when it would otherwise leave the window - the way a
    /// wheel scroll behaves in neovim.
    pub fn scroll_view(&mut self, delta: isize, height: usize) {
        let height = height.max(1);
        let last = self.lines.len().saturating_sub(1);
        self.scroll = (self.scroll as isize + delta).clamp(0, last as isize) as usize;
        if self.line < self.scroll {
            self.line = self.scroll;
        } else if self.line >= self.scroll + height {
            self.line = (self.scroll + height - 1).min(last);
        }
        self.clamp_column();
    }

    /// Keep the cursor inside a window `height` lines tall.
    pub fn follow(&mut self, height: usize) {
        let height = height.max(1);
        if self.line < self.scroll {
            self.scroll = self.line;
        } else if self.line >= self.scroll + height {
            self.scroll = self.line + 1 - height;
        }
    }

    /// The `-- INSERT --` style line under the buffer.
    pub fn status(&self) -> String {
        match self.mode {
            Mode::Command => format!(":{}", self.command),
            _ if !self.message.is_empty() => self.message.clone(),
            Mode::Insert => "-- INSERT --".to_string(),
            Mode::Normal => String::new(),
        }
    }
}

/// Byte offset of a character index, so multi-byte characters are never split.
fn char_to_byte(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

#[cfg(test)]
mod tests {
    // Vim's commands are case-sensitive, and a test named for `A` or `gg`
    // reads wrong if it is spelled otherwise.
    #![allow(non_snake_case)]

    use super::*;

    /// An editor over the given lines, without touching the filesystem.
    fn editor(lines: &[&str]) -> Editor {
        Editor {
            path: PathBuf::from("/tmp/dotgit-editor-test"),
            lines: lines.iter().map(|l| l.to_string()).collect(),
            line: 0,
            column: 0,
            mode: Mode::Normal,
            modified: false,
            scroll: 0,
            command: String::new(),
            message: String::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            yanked: Vec::new(),
            count: None,
            operator: None,
            insert_group: false,
            trailing_newline: true,
        }
    }

    /// Type a sequence of keys, where plain characters are `Key::Char` and a few
    /// names stand in for the special keys.
    fn keys(editor: &mut Editor, sequence: &str) -> Outcome {
        let mut outcome = Outcome::default();
        let mut rest = sequence;
        while !rest.is_empty() {
            let key = if let Some(tail) = rest.strip_prefix("<esc>") {
                rest = tail;
                Key::Esc
            } else if let Some(tail) = rest.strip_prefix("<cr>") {
                rest = tail;
                Key::Enter
            } else if let Some(tail) = rest.strip_prefix("<bs>") {
                rest = tail;
                Key::Backspace
            } else if let Some(tail) = rest.strip_prefix("<c-r>") {
                rest = tail;
                Key::Redo
            } else {
                let c = rest.chars().next().unwrap();
                rest = &rest[c.len_utf8()..];
                Key::Char(c)
            };
            outcome = editor.key(key);
        }
        outcome
    }

    fn text(editor: &Editor) -> String {
        editor.lines.join("\n")
    }

    #[test]
    fn starts_in_normal_mode_so_letters_are_commands_not_text() {
        let mut e = editor(&["hello world"]);
        keys(&mut e, "l");
        assert_eq!(e.mode, Mode::Normal);
        // `l` moved the cursor instead of typing an `l`.
        assert_eq!(text(&e), "hello world");
        assert_eq!(e.column, 1);
        assert!(!e.modified);
    }

    #[test]
    fn hjkl_move_and_stop_at_the_edges() {
        let mut e = editor(&["abc", "de"]);
        keys(&mut e, "lll");
        // The cursor rests on the last character, never past it.
        assert_eq!(e.column, 2);
        keys(&mut e, "j");
        // Moving onto a shorter line pulls the cursor in.
        assert_eq!((e.line, e.column), (1, 1));
        keys(&mut e, "jjj");
        assert_eq!(e.line, 1);
        keys(&mut e, "kkk");
        assert_eq!(e.line, 0);
        keys(&mut e, "hhhh");
        assert_eq!(e.column, 0);
    }

    #[test]
    fn counts_multiply_a_motion() {
        let mut e = editor(&["one", "two", "three", "four", "five"]);
        keys(&mut e, "3j");
        assert_eq!(e.line, 3);
        keys(&mut e, "2k");
        assert_eq!(e.line, 1);
        // A leading zero is still the `0` command, not the start of a count.
        keys(&mut e, "2l0");
        assert_eq!(e.column, 0);
    }

    #[test]
    fn line_and_word_motions() {
        let mut e = editor(&["  hello brave world"]);
        keys(&mut e, "$");
        assert_eq!(e.column, 18);
        keys(&mut e, "0");
        assert_eq!(e.column, 0);
        keys(&mut e, "^");
        assert_eq!(e.column, 2);
        keys(&mut e, "w");
        assert_eq!(e.column, 8);
        keys(&mut e, "w");
        assert_eq!(e.column, 14);
        keys(&mut e, "b");
        assert_eq!(e.column, 8);
    }

    #[test]
    fn gg_and_G_jump_to_the_ends_and_to_a_numbered_line() {
        let mut e = editor(&["one", "two", "three", "four"]);
        keys(&mut e, "G");
        assert_eq!(e.line, 3);
        keys(&mut e, "gg");
        assert_eq!(e.line, 0);
        keys(&mut e, "3G");
        assert_eq!(e.line, 2);
    }

    #[test]
    fn i_inserts_before_the_cursor_and_esc_returns_to_normal() {
        let mut e = editor(&["bc"]);
        keys(&mut e, "ia<esc>");
        assert_eq!(text(&e), "abc");
        assert_eq!(e.mode, Mode::Normal);
        // Vim leaves the cursor on the last inserted character.
        assert_eq!(e.column, 0);
        assert!(e.modified);
    }

    #[test]
    fn a_appends_after_the_cursor_and_A_at_the_end_of_the_line() {
        let mut e = editor(&["ab"]);
        keys(&mut e, "aX<esc>");
        assert_eq!(text(&e), "aXb");
        let mut e = editor(&["ab"]);
        keys(&mut e, "A!<esc>");
        assert_eq!(text(&e), "ab!");
    }

    #[test]
    fn I_inserts_at_the_first_non_blank() {
        let mut e = editor(&["    indented"]);
        keys(&mut e, "I- <esc>");
        assert_eq!(text(&e), "    - indented");
    }

    #[test]
    fn o_and_O_open_lines_below_and_above() {
        let mut e = editor(&["first", "second"]);
        keys(&mut e, "onew<esc>");
        assert_eq!(text(&e), "first\nnew\nsecond");
        let mut e = editor(&["first"]);
        keys(&mut e, "Otop<esc>");
        assert_eq!(text(&e), "top\nfirst");
    }

    #[test]
    fn enter_in_insert_mode_splits_a_line_and_backspace_joins_it() {
        let mut e = editor(&["abcd"]);
        keys(&mut e, "lli<cr><esc>");
        assert_eq!(text(&e), "ab\ncd");
        let mut e = editor(&["ab", "cd"]);
        keys(&mut e, "ji<bs><esc>");
        assert_eq!(text(&e), "abcd");
    }

    #[test]
    fn x_deletes_characters_and_takes_a_count() {
        let mut e = editor(&["hello"]);
        keys(&mut e, "x");
        assert_eq!(text(&e), "ello");
        keys(&mut e, "2x");
        assert_eq!(text(&e), "lo");
        // Deleting past the end of the line stops rather than panicking.
        keys(&mut e, "9x");
        assert_eq!(text(&e), "");
    }

    #[test]
    fn dd_deletes_a_line_and_dollar_D_deletes_to_the_end() {
        let mut e = editor(&["one", "two", "three"]);
        keys(&mut e, "jdd");
        assert_eq!(text(&e), "one\nthree");
        let mut e = editor(&["one", "two", "three", "four"]);
        keys(&mut e, "2dd");
        assert_eq!(text(&e), "three\nfour");
        let mut e = editor(&["keep this"]);
        keys(&mut e, "4lD");
        assert_eq!(text(&e), "keep");
    }

    #[test]
    fn deleting_the_only_line_leaves_an_empty_buffer_not_no_buffer() {
        let mut e = editor(&["only"]);
        keys(&mut e, "dd");
        assert_eq!(e.lines, vec![String::new()]);
        assert_eq!(e.line, 0);
    }

    #[test]
    fn dw_deletes_a_word() {
        let mut e = editor(&["alpha beta gamma"]);
        keys(&mut e, "dw");
        assert_eq!(text(&e), "beta gamma");
    }

    #[test]
    fn yy_and_p_copy_lines_and_dd_fills_the_same_register() {
        let mut e = editor(&["one", "two"]);
        keys(&mut e, "yyp");
        assert_eq!(text(&e), "one\none\ntwo");
        // A deleted line can be pasted back, which is how vim moves lines.
        let mut e = editor(&["one", "two", "three"]);
        keys(&mut e, "ddGp");
        assert_eq!(text(&e), "two\nthree\none");
        // `P` pastes above.
        let mut e = editor(&["one", "two"]);
        keys(&mut e, "yyjP");
        assert_eq!(text(&e), "one\none\ntwo");
    }

    #[test]
    fn cc_replaces_a_line_and_leaves_you_in_insert_mode() {
        let mut e = editor(&["throw away", "keep"]);
        keys(&mut e, "ccnew<esc>");
        assert_eq!(text(&e), "new\nkeep");
    }

    #[test]
    fn u_undoes_and_ctrl_r_redoes() {
        let mut e = editor(&["one", "two"]);
        keys(&mut e, "dd");
        assert_eq!(text(&e), "two");
        keys(&mut e, "u");
        assert_eq!(text(&e), "one\ntwo");
        keys(&mut e, "<c-r>");
        assert_eq!(text(&e), "two");
    }

    #[test]
    fn a_whole_insert_session_undoes_as_one_step() {
        let mut e = editor(&["x"]);
        keys(&mut e, "ihello<esc>");
        assert_eq!(text(&e), "hellox");
        keys(&mut e, "u");
        // Not "hellx": the session is one change, as in vim.
        assert_eq!(text(&e), "x");
    }

    #[test]
    fn undo_at_the_oldest_change_says_so_instead_of_panicking() {
        let mut e = editor(&["one"]);
        keys(&mut e, "u");
        assert_eq!(text(&e), "one");
        assert!(e.message.contains("oldest change"));
    }

    #[test]
    fn colon_q_refuses_to_throw_away_unsaved_changes() {
        let mut e = editor(&["one"]);
        keys(&mut e, "x");
        let outcome = keys(&mut e, ":q<cr>");
        assert!(!outcome.closed);
        assert!(e.message.contains("E37"));
        // `:q!` overrides, exactly as vim's does.
        let outcome = keys(&mut e, ":q!<cr>");
        assert!(outcome.closed);
        assert!(!outcome.saved);
    }

    #[test]
    fn colon_q_closes_an_unchanged_buffer() {
        let mut e = editor(&["one"]);
        let outcome = keys(&mut e, ":q<cr>");
        assert!(outcome.closed);
    }

    #[test]
    fn esc_cancels_a_command_line_without_running_it() {
        let mut e = editor(&["one"]);
        keys(&mut e, ":q<esc>");
        assert_eq!(e.mode, Mode::Normal);
        assert!(e.command.is_empty());
    }

    #[test]
    fn colon_number_jumps_to_a_line_and_nonsense_reports_itself() {
        let mut e = editor(&["one", "two", "three"]);
        keys(&mut e, ":3<cr>");
        assert_eq!(e.line, 2);
        keys(&mut e, ":nonsense<cr>");
        assert!(e.message.contains("E492"));
    }

    #[test]
    fn a_half_typed_operator_or_count_is_abandoned_by_esc() {
        let mut e = editor(&["one", "two"]);
        keys(&mut e, "2d<esc>");
        // Neither the count nor the `d` survived, so the buffer is untouched.
        assert_eq!(text(&e), "one\ntwo");
        keys(&mut e, "dd");
        assert_eq!(text(&e), "two");
    }

    #[test]
    fn multi_byte_characters_are_never_split() {
        let mut e = editor(&["héllo wörld"]);
        keys(&mut e, "x");
        assert_eq!(text(&e), "éllo wörld");
        keys(&mut e, "$x");
        assert_eq!(text(&e), "éllo wörl");
        keys(&mut e, "0iä<esc>");
        assert_eq!(text(&e), "äéllo wörl");
    }

    #[test]
    fn the_view_follows_the_cursor() {
        let lines: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        let mut e = editor(&[]);
        e.lines = lines;
        keys(&mut e, "50G");
        e.follow(10);
        // The cursor is the last visible line after moving down.
        assert_eq!(e.scroll, 40);
        keys(&mut e, "gg");
        e.follow(10);
        assert_eq!(e.scroll, 0);
    }

    #[test]
    fn clicking_puts_the_cursor_where_the_pointer_is() {
        let mut e = editor(&["alpha", "be", "gamma"]);
        e.click(2, 3);
        assert_eq!((e.line, e.column), (2, 3));
        // Past the end of a short line lands on its last character.
        e.click(1, 9);
        assert_eq!((e.line, e.column), (1, 1));
        // Below the last line lands on the last line.
        e.click(99, 0);
        assert_eq!(e.line, 2);
    }

    #[test]
    fn clicking_in_insert_mode_may_rest_one_past_the_end() {
        let mut e = editor(&["ab"]);
        keys(&mut e, "i");
        e.click(0, 9);
        assert_eq!(e.column, 2);
    }

    #[test]
    fn the_wheel_scrolls_the_view_and_only_drags_the_cursor_when_it_must() {
        let lines: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        let mut e = editor(&[]);
        e.lines = lines;
        // The cursor is on line 1 and the window is 10 tall.
        e.scroll_view(3, 10);
        assert_eq!(e.scroll, 3);
        // Scrolling past the cursor pulls it down to the top of the window.
        assert_eq!(e.line, 3);

        // Scrolling back up leaves the cursor where it is while it stays visible.
        e.scroll_view(-1, 10);
        assert_eq!((e.scroll, e.line), (2, 3));

        // And it cannot scroll above the first line.
        e.scroll_view(-99, 10);
        assert_eq!((e.scroll, e.line), (0, 3));
    }

    #[test]
    fn writing_keeps_the_files_final_newline_as_it_found_it() {
        let dir = std::env::temp_dir().join("dotgit-editor-newline");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let with = dir.join("with.conf");
        std::fs::write(&with, "a\nb\n").unwrap();
        let mut e = Editor::open(&with).unwrap();
        keys(&mut e, "x");
        e.save().unwrap();
        assert_eq!(std::fs::read_to_string(&with).unwrap(), "\nb\n");

        let without = dir.join("without.conf");
        std::fs::write(&without, "a\nb").unwrap();
        let mut e = Editor::open(&without).unwrap();
        keys(&mut e, "x");
        e.save().unwrap();
        assert_eq!(std::fs::read_to_string(&without).unwrap(), "\nb");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn opening_a_file_that_does_not_exist_starts_an_empty_buffer() {
        let path = std::env::temp_dir().join("dotgit-editor-missing.conf");
        let _ = std::fs::remove_file(&path);
        let mut e = Editor::open(&path).unwrap();
        assert_eq!(e.lines, vec![String::new()]);
        assert!(e.message.contains("[New]"));
        keys(&mut e, "ihello<esc>");
        let outcome = keys(&mut e, ":wq<cr>");
        assert!(outcome.saved && outcome.closed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn saving_clears_the_modified_flag_so_quitting_is_allowed() {
        let path = std::env::temp_dir().join("dotgit-editor-save.conf");
        std::fs::write(&path, "one\n").unwrap();
        let mut e = Editor::open(&path).unwrap();
        keys(&mut e, "ddi1<esc>");
        assert!(e.modified);
        keys(&mut e, ":w<cr>");
        assert!(!e.modified);
        let outcome = keys(&mut e, ":q<cr>");
        assert!(outcome.closed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n");
        std::fs::remove_file(&path).unwrap();
    }
}
