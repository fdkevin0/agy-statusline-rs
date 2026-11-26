# AGENTS.md

Safe Rust statusline and local usage analytics CLI/TUI for Antigravity CLI (`agy`).

## Architecture Map

| Module | Ownership |
| :--- | :--- |
| `src/agy/` | AGY path discovery (`paths.rs`), read-only SQLite sniffing (`db.rs`), protobuf decoding (`protobuf.rs`), `settings.json` I/O (`settings.rs`), atomic `quota_snapshot.json` (`quota.rs`), loopback Hub RPC (`hub.rs`) |
| `src/config/` | Multi-row `LayoutConfig`, 33 `WidgetId`s, 4 presets, and XDG `config.json` persistence |
| `src/statusline/` | Stdin JSON pipeline (`statusline.rs`), pure-Rust `.git` probe (`git.rs`), quota/pace/cache metrics (`metrics.rs`), palettes/glyphs (`theme.rs`), responsive renderer (`render/`) |
| `src/stats/` | Data models (`model.rs`), metrics/sorts (`metric.rs`), `models.dev` pricing (`pricing.rs`), parallel SQLite scanner & caches (`scan/`), in-memory query engine (`query.rs`), static renderer (`render.rs`), unified TUI (`tui/`) |
| `src/cli/` | Subcommands: `apply`, 3-pane `config` TUI (`config/`), and `doctor` |
| `src/sys/` | Atomic/symlink-safe bounded FS (`fs.rs`), formatting (`fmt.rs`), bounded subprocesses (`process.rs`), RAII terminal guard (`terminal.rs`), civil time (`time.rs`) |

## Verification & Local Autonomy

Local tests use disposable `tempfile` fixtures and isolated environments with no production or network access; run them, fix failures caused by your edits, and rerun without pausing for approval:

```sh
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked --all-targets && cargo test --locked --doc
```

## Core Invariants

- **Zero unsafe code**: `#![forbid(unsafe_code)]` across library, binary, and integration tests.
- **Symlink-safe bounded I/O**: Reject symlinks (`symlink_metadata`), cap file/response sizes, and use atomic writes (`sys::fs`) for all persisted state (`config.json`, `settings.json`, cache files).
- **Statusline hot path**: Zero network I/O; stdin execution (`main.rs`) always exits `0` to avoid tripping AGY's 30-failure circuit breaker.
- **Test hygiene**: Keep Tier 1 unit tests offline and Tier 2 CLI tests isolated (`HOME`, `XDG_*`, minimal `PATH`, explicit env). Never use control flow (`if`, `for`, `while`, `match`, ternaries) or `sleep` calls inside `#[test]` functions.

## Intentional Non-Bugs

- Stdin statusline mode silently absorbs transient read/write errors and returns exit code `0`.
- Pricing refresh on the statusline path is background-only (`SpawnIfStale`); only `stats` and `pricing sync` refresh synchronously.
- Foreign forked rows (`origin_cascade_id != conversation_id`), symlinked DBs/sidecars, and non-UUID `.db` files are intentionally ignored.

## Contextual Documentation

- Use [docs/configuration.md](docs/configuration.md) when modifying CLI flags, widgets, presets, TUI controls, pricing rules, or XDG cache files.
- Use [CONTRIBUTING.md](CONTRIBUTING.md) for branch, commit, and PR conventions.
