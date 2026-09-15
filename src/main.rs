//! The `dotgit` command line tool. The implementation lives in the library so
//! that the `dg` alias runs exactly the same code.

fn main() {
    dotgit::cli::run();
}
