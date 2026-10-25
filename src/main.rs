//! rcache: a copy-on-write rustc cache shared by every checkout on the
//! machine — jj workspaces, git worktrees, plain clones.
//!
//! Install as Cargo's `rustc-wrapper`. Each workspace keeps its own target
//! directory (no shared locks, no cross-workspace staleness); compiles are
//! keyed by workspace-independent inputs, and a hit is cloned into the target
//! directory from one stored copy, so N workspaces cost about one copy of
//! disk.

mod args;
mod cli;
mod depinfo;
mod fsutil;
mod paths;
mod store;
mod wrapper;

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let code = match argv.first() {
        Some(first) if wrapper::is_compiler(first) => wrapper::run(first, &argv[1..]),
        _ => cli::run(&argv),
    };
    std::process::exit(code);
}
