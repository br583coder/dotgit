//! dotgit's internals, shared by the command line tool and optional terminal UI.
//!
//! Both drive the same code in [`ops`], so neither can drift from the other,
//! and anything the TUI can do the CLI can do too.

pub mod backup;
pub mod cli;
pub mod error;
pub mod fsops;
pub mod gh;
pub mod git;
pub mod history;
pub mod ops;
#[cfg(feature = "tui")]
#[path = "bin/dotgit-tui.rs"]
pub mod tui;
