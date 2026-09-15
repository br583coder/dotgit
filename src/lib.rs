//! dotgit's internals, shared by the two front ends that ship with it: the
//! `dotgit` command line tool and the optional `dotgit-tui` browser.
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
