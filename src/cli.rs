//! `rcache stats | gc | clear` and help.

use std::ffi::OsString;
use std::fs;

use crate::store::{self, Store};

const HELP: &str = "\
rcache — copy-on-write rustc cache shared across jj workspaces and git worktrees

Enable for every Cargo build (~/.cargo/config.toml):
    [build]
    rustc-wrapper = \"/path/to/rcache\"

Usage:
    rcache stats              hit/miss counts, time saved, store size
    rcache gc [--max-size S]  evict least recently used entries (default $RCACHE_MAX_SIZE or 40G)
    rcache clear              remove every entry

Environment:
    RCACHE_DIR       store location (default ~/Library/Caches/rcache, ~/.cache/rcache);
                     keep it on the same APFS/btrfs/XFS volume as your checkouts so
                     entries are clones, not copies
    RCACHE_MAX_SIZE  automatic gc threshold (logical size, e.g. 40G)
    RCACHE_DISABLE=1 run rustc untouched
";

pub fn run(argv: &[OsString]) -> i32 {
    let args: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] | ["help" | "--help" | "-h"] => {
            print!("{HELP}");
            Ok(())
        }
        ["--version" | "-V"] => {
            println!("rcache {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        ["stats"] => stats(),
        ["gc"] => gc(store::max_size()),
        ["gc", "--max-size", size] => match store::parse_size(size) {
            Some(max) => gc(max),
            None => Err(anyhow::anyhow!("bad size {size:?}")),
        },
        ["clear"] => Store::open().and_then(|s| s.clear()),
        _ => {
            eprint!("rcache: unknown arguments {args:?}\n\n{HELP}");
            return 2;
        }
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rcache: {e:#}");
            1
        }
    }
}

fn stats() -> anyhow::Result<()> {
    let store = Store::open()?;
    let mut counts = [0u64; 4];
    let (mut saved_ms, mut compiled_ms) = (0u64, 0u64);
    let log = fs::read_to_string(store.log_path()).unwrap_or_default();
    for line in log.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [_, kind, _, ms, saved, ..] = f.as_slice() else {
            continue;
        };
        let (ms, saved) = (ms.parse().unwrap_or(0u64), saved.parse().unwrap_or(0u64));
        match *kind {
            "hit" => {
                counts[0] += 1;
                saved_ms += saved.saturating_sub(ms);
            }
            "miss" => {
                counts[1] += 1;
                compiled_ms += ms;
            }
            "nostore" => counts[2] += 1,
            _ => counts[3] += 1,
        }
    }
    let total = counts[0] + counts[1] + counts[2];
    let entries = fs::read_dir(store.root().join("entries"))?.count();
    let bytes = crate::fsutil::tree_size(&store.root().join("entries"));
    println!("store       {}", store.root().display());
    println!(
        "entries     {entries} ({} logical; blocks are shared with target dirs)",
        human(bytes)
    );
    println!(
        "hits        {} ({:.0}%), saved ~{} of compile time",
        counts[0],
        if total > 0 {
            100.0 * counts[0] as f64 / total as f64
        } else {
            0.0
        },
        duration(saved_ms)
    );
    println!(
        "misses      {} ({} compiling)",
        counts[1],
        duration(compiled_ms)
    );
    println!("not stored  {}", counts[2]);
    println!(
        "errors      {} (see {})",
        counts[3],
        store.log_path().display()
    );
    Ok(())
}

fn gc(max: u64) -> anyhow::Result<()> {
    let report = Store::open()?.gc(max)?;
    println!(
        "evicted {}; {} entries, {} logical (limit {})",
        report.evicted,
        report.entries,
        human(report.bytes),
        human(max)
    );
    Ok(())
}

fn human(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", units[i])
}

fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{:.1}s", ms as f64 / 1000.0),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}
