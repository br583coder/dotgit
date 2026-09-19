//! Line-based syntax highlighting for the built-in editor.
//!
//! A dotfiles repository holds config files, shell, and the odd script, so the
//! highlighter is a small tokeniser for those rather than a grammar engine: it
//! costs no dependencies, runs per visible line, and - being a pure function
//! from a line to a list of spans - can be tested directly.
//!
//! It works a line at a time, which is what keeps it fast and simple. A string
//! or comment that spans several lines is therefore highlighted only on the
//! lines where its opening delimiter appears.

use std::path::Path;

/// What a piece of a line is, which the editor turns into a colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Comment,
    Str,
    Number,
    Keyword,
    /// `true`, `false`, `null`, and friends.
    Constant,
    /// The left-hand side of `key = value` or `key: value`.
    Key,
    /// A `[section]` header.
    Section,
    Punctuation,
}

/// A run of characters sharing one kind. Spans cover the whole line in order,
/// so a renderer can emit them one after another without gaps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Toml,
    Ini,
    Json,
    Yaml,
    Shell,
    Lua,
    Vimscript,
    Rust,
    Python,
    Markdown,
    Plain,
}

/// Guess the language from the file name, the way an editor does. Dotfiles are
/// often extension-less, so well-known names are recognised too.
pub fn language_for(path: &Path) -> Language {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        "toml" => return Language::Toml,
        "ini" | "cfg" | "conf" | "desktop" | "service" => return Language::Ini,
        "json" => return Language::Json,
        "yaml" | "yml" => return Language::Yaml,
        "sh" | "bash" | "zsh" | "fish" | "zshrc" | "bashrc" | "profile" => {
            return Language::Shell;
        }
        "lua" => return Language::Lua,
        "vim" => return Language::Vimscript,
        "rs" => return Language::Rust,
        "py" => return Language::Python,
        "md" | "markdown" => return Language::Markdown,
        _ => {}
    }

    // Names that carry their type without an extension.
    match name.as_str() {
        ".zshrc" | ".bashrc" | ".bash_profile" | ".profile" | ".zprofile" | ".zshenv"
        | ".bash_aliases" => Language::Shell,
        ".gitconfig" | ".gitmodules" | ".npmrc" | ".editorconfig" => Language::Ini,
        "config" | ".inputrc" | ".tmux.conf" => Language::Ini,
        ".vimrc" | ".gvimrc" => Language::Vimscript,
        "init.lua" => Language::Lua,
        "cargo.lock" => Language::Toml,
        _ => Language::Plain,
    }
}

/// The rules for one language. Deliberately small: what a config file needs is
/// comments, strings, numbers, keys and a handful of keywords.
struct Syntax {
    line_comments: &'static [&'static str],
    strings: &'static [char],
    keywords: &'static [&'static str],
    constants: &'static [&'static str],
    /// `key = value` / `key: value` lines have a highlighted left-hand side.
    key_values: bool,
    /// `[section]` headers are highlighted.
    sections: bool,
}

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "return", "export", "local", "set", "alias", "source", "echo", "cd", "in",
];
const LUA_KEYWORDS: &[&str] = &[
    "local", "function", "end", "if", "then", "else", "elseif", "for", "while", "do", "return",
    "require", "and", "or", "not", "repeat", "until", "break",
];
const RUST_KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "match", "if", "else",
    "for", "while", "loop", "return", "self", "crate", "const", "static", "trait", "where",
    "async", "await", "move", "ref", "as", "in", "dyn",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "def", "class", "import", "from", "return", "if", "elif", "else", "for", "while", "try",
    "except", "finally", "with", "as", "pass", "lambda", "yield", "global", "not", "and", "or",
    "in", "is",
];
const VIM_KEYWORDS: &[&str] = &[
    "set",
    "let",
    "call",
    "function",
    "endfunction",
    "if",
    "endif",
    "else",
    "for",
    "endfor",
    "while",
    "endwhile",
    "map",
    "nmap",
    "imap",
    "nnoremap",
    "inoremap",
    "autocmd",
    "augroup",
    "source",
    "return",
];

fn syntax(language: Language) -> Syntax {
    match language {
        Language::Toml => Syntax {
            line_comments: &["#"],
            strings: &['"', '\''],
            keywords: &[],
            constants: &["true", "false"],
            key_values: true,
            sections: true,
        },
        Language::Ini => Syntax {
            line_comments: &["#", ";"],
            strings: &['"'],
            keywords: &[],
            constants: &["true", "false", "yes", "no", "on", "off"],
            key_values: true,
            sections: true,
        },
        Language::Json => Syntax {
            line_comments: &[],
            strings: &['"'],
            keywords: &[],
            constants: &["true", "false", "null"],
            key_values: true,
            sections: false,
        },
        Language::Yaml => Syntax {
            line_comments: &["#"],
            strings: &['"', '\''],
            keywords: &[],
            constants: &["true", "false", "null", "yes", "no"],
            key_values: true,
            sections: false,
        },
        Language::Shell => Syntax {
            line_comments: &["#"],
            strings: &['"', '\'', '`'],
            keywords: SHELL_KEYWORDS,
            constants: &["true", "false"],
            key_values: false,
            sections: false,
        },
        Language::Lua => Syntax {
            line_comments: &["--"],
            strings: &['"', '\''],
            keywords: LUA_KEYWORDS,
            constants: &["true", "false", "nil"],
            key_values: false,
            sections: false,
        },
        Language::Vimscript => Syntax {
            line_comments: &["\""],
            strings: &['\''],
            keywords: VIM_KEYWORDS,
            constants: &["true", "false"],
            key_values: false,
            sections: false,
        },
        Language::Rust => Syntax {
            line_comments: &["//"],
            strings: &['"'],
            keywords: RUST_KEYWORDS,
            constants: &["true", "false", "None", "Some", "Ok", "Err"],
            key_values: false,
            sections: false,
        },
        Language::Python => Syntax {
            line_comments: &["#"],
            strings: &['"', '\''],
            keywords: PYTHON_KEYWORDS,
            constants: &["True", "False", "None"],
            key_values: false,
            sections: false,
        },
        Language::Markdown => Syntax {
            line_comments: &[],
            strings: &['`'],
            keywords: &[],
            constants: &[],
            key_values: false,
            sections: false,
        },
        Language::Plain => Syntax {
            line_comments: &[],
            strings: &[],
            keywords: &[],
            constants: &[],
            key_values: false,
            sections: false,
        },
    }
}

/// Split one line into highlighted spans.
pub fn highlight(language: Language, line: &str) -> Vec<Span> {
    let syntax = syntax(language);
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();

    // A markdown heading colours the whole line, which is the one whole-line
    // rule worth having.
    if language == Language::Markdown && line.trim_start().starts_with('#') {
        return vec![Span {
            text: line.to_string(),
            kind: Kind::Section,
        }];
    }

    // `[section]` headers, allowing for indentation.
    if syntax.sections {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.contains(']') {
            let indent = line.len() - trimmed.len();
            let end = trimmed.find(']').unwrap() + 1;
            let mut spans = Vec::new();
            if indent > 0 {
                spans.push(Span {
                    text: line[..indent].to_string(),
                    kind: Kind::Plain,
                });
            }
            spans.push(Span {
                text: trimmed[..end].to_string(),
                kind: Kind::Section,
            });
            if end < trimmed.len() {
                spans.extend(highlight(language, &trimmed[end..]));
            }
            return spans;
        }
    }

    // Where the key ends on a `key = value` line, so the name can be coloured
    // differently from the value.
    let key_end = if syntax.key_values {
        key_boundary(&chars)
    } else {
        None
    };

    let mut index = 0;
    while index < chars.len() {
        // Comments run to the end of the line.
        if let Some(prefix) = syntax
            .line_comments
            .iter()
            .find(|prefix| starts_with_at(&chars, index, prefix))
        {
            let _ = prefix;
            flush(&mut spans, &mut plain);
            spans.push(Span {
                text: chars[index..].iter().collect(),
                kind: Kind::Comment,
            });
            return spans;
        }

        let c = chars[index];

        // Strings, honouring backslash escapes.
        if syntax.strings.contains(&c) {
            flush(&mut spans, &mut plain);
            let mut text = String::from(c);
            let mut cursor = index + 1;
            while cursor < chars.len() {
                let inner = chars[cursor];
                text.push(inner);
                cursor += 1;
                if inner == '\\' && cursor < chars.len() {
                    text.push(chars[cursor]);
                    cursor += 1;
                    continue;
                }
                if inner == c {
                    break;
                }
            }
            spans.push(Span {
                text,
                kind: Kind::Str,
            });
            index = cursor;
            continue;
        }

        // Numbers, but not the digits inside an identifier like `utf8`.
        let after_word = index > 0 && is_word(chars[index - 1]);
        if c.is_ascii_digit() && !after_word {
            flush(&mut spans, &mut plain);
            let mut text = String::new();
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric()
                    || chars[index] == '.'
                    || chars[index] == '_')
            {
                text.push(chars[index]);
                index += 1;
            }
            spans.push(Span {
                text,
                kind: Kind::Number,
            });
            continue;
        }

        // Words: a keyword, a constant, a key name, or ordinary text.
        if is_word_start(c) {
            let start = index;
            let mut text = String::new();
            while index < chars.len() && is_word(chars[index]) {
                text.push(chars[index]);
                index += 1;
            }
            let kind = if key_end.is_some_and(|end| start < end) {
                Kind::Key
            } else if syntax.constants.contains(&text.as_str()) {
                Kind::Constant
            } else if syntax.keywords.contains(&text.as_str()) {
                Kind::Keyword
            } else {
                Kind::Plain
            };
            if kind == Kind::Plain {
                plain.push_str(&text);
            } else {
                flush(&mut spans, &mut plain);
                spans.push(Span { text, kind });
            }
            continue;
        }

        if "=:,{}[]()<>|&;+-*/".contains(c) {
            flush(&mut spans, &mut plain);
            spans.push(Span {
                text: c.to_string(),
                kind: Kind::Punctuation,
            });
            index += 1;
            continue;
        }

        plain.push(c);
        index += 1;
    }

    flush(&mut spans, &mut plain);
    spans
}

/// The character index of the `=` or `:` that separates a key from its value,
/// if this line looks like an assignment rather than prose.
fn key_boundary(chars: &[char]) -> Option<usize> {
    let mut index = 0;
    // Skip indentation and a YAML list marker.
    while index < chars.len() && (chars[index].is_whitespace() || chars[index] == '-') {
        index += 1;
    }
    let start = index;
    while index < chars.len()
        && (is_word(chars[index]) || chars[index] == '.' || chars[index] == '"')
    {
        index += 1;
    }
    if index == start {
        return None;
    }
    let mut lookahead = index;
    while lookahead < chars.len() && chars[lookahead] == ' ' {
        lookahead += 1;
    }
    match chars.get(lookahead) {
        Some('=') | Some(':') => Some(index),
        _ => None,
    }
}

fn starts_with_at(chars: &[char], index: usize, prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(offset, c)| chars.get(index + offset) == Some(&c))
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$' || c == '@'
}

/// A colon is deliberately not part of a word: `name: value` in YAML has to
/// split there for the key to be recognised.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '$'
}

fn flush(spans: &mut Vec<Span>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span {
            text: std::mem::take(plain),
            kind: Kind::Plain,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kinds a line produces, for asserting shape without spelling out text.
    fn kinds(language: Language, line: &str) -> Vec<Kind> {
        highlight(language, line)
            .into_iter()
            .map(|s| s.kind)
            .collect()
    }

    fn find(language: Language, line: &str, kind: Kind) -> Vec<String> {
        highlight(language, line)
            .into_iter()
            .filter(|s| s.kind == kind)
            .map(|s| s.text)
            .collect()
    }

    #[test]
    fn spans_always_reconstruct_the_original_line() {
        // A renderer draws the spans in order, so anything else would corrupt
        // the file on screen.
        for (language, line) in [
            (Language::Toml, "enabled = true # on"),
            (Language::Shell, "alias ll='ls -la' # list"),
            (Language::Rust, "let x = \"quoted\"; // note"),
            (Language::Json, "{\"key\": [1, 2.5, null]}"),
            (Language::Yaml, "  - name: value"),
            (Language::Markdown, "# Heading"),
            (Language::Plain, "just some text 123"),
            (Language::Toml, ""),
            (Language::Lua, "local map = vim.keymap.set -- shorthand"),
        ] {
            let joined: String = highlight(language, line)
                .iter()
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(joined, line, "{language:?} mangled {line:?}");
        }
    }

    #[test]
    fn comments_run_to_the_end_of_the_line() {
        assert_eq!(
            find(Language::Toml, "x = 1 # why", Kind::Comment),
            ["# why"]
        );
        assert_eq!(
            find(Language::Lua, "x = 1 -- why", Kind::Comment),
            ["-- why"]
        );
        assert_eq!(
            find(Language::Rust, "let x = 1; // why", Kind::Comment),
            ["// why"]
        );
        assert_eq!(find(Language::Ini, "x = 1 ; why", Kind::Comment), ["; why"]);
        // A `#` inside a string is not a comment.
        assert_eq!(
            find(Language::Shell, "echo \"# not a comment\"", Kind::Comment),
            Vec::<String>::new()
        );
    }

    #[test]
    fn strings_keep_their_quotes_and_escapes() {
        assert_eq!(
            find(Language::Toml, "path = \"~/.config\"", Kind::Str),
            ["\"~/.config\""]
        );
        assert_eq!(
            find(Language::Rust, "let s = \"a \\\" b\";", Kind::Str),
            ["\"a \\\" b\""]
        );
        // An unterminated string still highlights to the end of the line.
        assert_eq!(find(Language::Shell, "echo \"open", Kind::Str), ["\"open"]);
    }

    #[test]
    fn a_key_is_distinguished_from_its_value() {
        assert_eq!(
            find(Language::Toml, "enabled = true", Kind::Key),
            ["enabled"]
        );
        assert_eq!(
            find(Language::Toml, "enabled = true", Kind::Constant),
            ["true"]
        );
        assert_eq!(find(Language::Yaml, "  name: dotgit", Kind::Key), ["name"]);
        // Prose is not an assignment, so nothing is a key.
        assert!(find(Language::Toml, "this is just text", Kind::Key).is_empty());
    }

    #[test]
    fn section_headers_are_highlighted_whole() {
        assert_eq!(
            find(Language::Toml, "[logging]", Kind::Section),
            ["[logging]"]
        );
        assert_eq!(
            find(Language::Toml, "  [tool.dotgit]  # indented", Kind::Section),
            ["[tool.dotgit]"]
        );
        assert_eq!(
            find(Language::Markdown, "## Install", Kind::Section),
            ["## Install"]
        );
    }

    #[test]
    fn numbers_are_highlighted_but_not_inside_words() {
        assert_eq!(find(Language::Toml, "port = 8080", Kind::Number), ["8080"]);
        assert_eq!(find(Language::Toml, "ratio = 2.5", Kind::Number), ["2.5"]);
        // `utf8` is one identifier, not a word and a number.
        assert!(find(Language::Shell, "export LANG=en_utf8", Kind::Number).is_empty());
    }

    #[test]
    fn keywords_are_recognised_per_language() {
        assert!(find(Language::Shell, "if true; then", Kind::Keyword).contains(&"if".to_string()));
        assert!(find(Language::Lua, "local x = 1", Kind::Keyword).contains(&"local".to_string()));
        assert!(find(Language::Rust, "fn main() {}", Kind::Keyword).contains(&"fn".to_string()));
        // A shell keyword is not a keyword in TOML.
        assert!(find(Language::Toml, "if = 1", Kind::Keyword).is_empty());
    }

    #[test]
    fn the_language_comes_from_the_extension_or_a_known_name() {
        use std::path::PathBuf;
        let of = |name: &str| language_for(&PathBuf::from(name));
        assert_eq!(of("dotgit.toml"), Language::Toml);
        assert_eq!(of("init.lua"), Language::Lua);
        assert_eq!(of(".zshrc"), Language::Shell);
        assert_eq!(of(".vimrc"), Language::Vimscript);
        assert_eq!(of("hypr.conf"), Language::Ini);
        assert_eq!(of("settings.json"), Language::Json);
        assert_eq!(of("README.md"), Language::Markdown);
        assert_eq!(of("script.py"), Language::Python);
        // Something unrecognised is left alone rather than guessed at.
        assert_eq!(of("mystery.xyz"), Language::Plain);
    }

    #[test]
    fn a_plain_file_is_one_span_of_plain_text() {
        assert_eq!(kinds(Language::Plain, "anything at all"), [Kind::Plain]);
    }

    #[test]
    fn multi_byte_lines_are_not_split_mid_character() {
        let line = "greeting = \"héllo wörld\" # ünicode";
        let joined: String = highlight(Language::Toml, line)
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(joined, line);
    }
}
