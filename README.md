# rcache

A shared rustc cache for every checkout on the machine: jj workspaces, git
worktrees, plain clones. Each checkout keeps its own `target/` (so there is no
shared Cargo lock and no cross-checkout staleness), but a compile that already
happened anywhere is placed into it instead of rerun, sharing storage with the
stored copy: a copy-on-write clone where the filesystem supports it (APFS
`clonefile`, btrfs/XFS `FICLONE`), hard links on ext4 and the like.

## Install

```sh
cargo install --path .
```

`~/.cargo/config.toml`:

```toml
[build]
rustc-wrapper = "/Users/you/.cargo/bin/rcache"
```

`RCACHE_DISABLE=1` runs rustc untouched; `rcache stats` shows hits and time
saved; `rcache gc [--max-size 40G]` and `rcache clear` manage the store
(`~/Library/Caches/rcache`, `~/.cache/rcache`, or `$RCACHE_DIR`). Keep the
store on the same filesystem as your checkouts, or entries become real
copies. `RCACHE_PLACE=link` forces the hard-link path (testing it on a
copy-on-write filesystem); `RCACHE_PLACE=copy` disables sharing.

## Why not a shared `CARGO_TARGET_DIR` / `build-dir`

Cargo hashes workspace crates by their path *relative to the workspace root*,
so `core` in checkout A and `core` in checkout B are the same unit in a shared
target dir. Freshness is then decided by mtimes: if B's edit is older than A's
last build, B silently gets A's binary. Builds also serialize on the one
directory lock. Neither happens here.

## How a compile is keyed

- **Primary key** (known before compiling): `rustc -vV`, every argument with the
  target dir spelled `$T` and rustc's cwd spelled `$CWD`, `-C incremental`
  dropped, each `--extern` replaced by its identity, the Cargo-set crate
  environment (`CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, …), and the build script's
  whole `OUT_DIR` with workspace roots normalized.
- **Candidates** under a primary key each list rustc's dep-info: every source
  file (hashed) and every `env!`/`option_env!` value. The first candidate
  matching this checkout wins. `CARGO_MANIFEST_DIR` read through `env!` is
  compared verbatim, so a binary that locates files at runtime from it is
  never handed another checkout's path.
- **Identity**: an entry id hashes the primary key and the matched inputs.
  Outputs carry it in the `user.rcache.id` xattr (voided by any rewrite), and
  dependents key on it instead of rlib bytes, which embed absolute paths. The
  `.rmeta` is tagged as soon as rustc announces it, before Cargo starts
  pipelined dependents.

A hit clones the outputs into `--out-dir`, rewrites the dep-info and replayed
diagnostics to this checkout's paths, and stamps them with the current time.
Inputs modified after the compile started are never stored. Registry and git
crates (immutable sources) take a per-unit lock, so parallel agents building a
fresh checkout compile each dependency once.

## Without copy-on-write (ext4)

Clones are independent files; hard links are the *same* file in the store and
in every checkout, so the hard-link path follows these rules:

- What gets linked: for workspace crates, what only the compiler reads
  (rlibs, `.rmeta`, codegen objects); for registry and git crates, every
  output (build scripts and proc macros included). Executables and
  cdylibs/static libraries of workspace crates stay per-checkout copies:
  tools and people copy or modify those. Dep-info is always rewritten.
- A shared file's mtime is never touched (every other checkout would see its
  dependencies change). Cargo marks a unit stale when a dependency's output is
  newer than the unit's newest output, so when a dependency in this checkout
  is newer than the store's copy — typically a build script that just ran
  here — the unit's `.rmeta` (else every output Cargo tracks) becomes a
  private copy stamped now.
- Linked files are read-only, so an in-place write fails instead of
  corrupting the store. Before compiling a unit, rcache unlinks its shared
  outputs, so rustc and the linker only ever write fresh files.
- `gc` evicts entries no checkout links any more first (link count 1),
  least recently used first; removing a linked entry would free nothing.

Per checkout this still costs the private copies; on clones it costs nothing.

## Limits

- Build-script *runs* are not cached (Cargo runs them itself); their compiles
  are, and the results they feed into rustc are keyed through `OUT_DIR`.
- A proc macro that reads files or environment without rustc tracking it
  (`std::env::var`, `std::fs`) is only keyed by the Cargo-set environment, the
  same blind spot sccache has.
- Debug info in a hit points at the checkout that compiled it.
- `rustc` invoked through another workspace wrapper (`clippy-driver`) is passed
  through untouched.
