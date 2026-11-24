# agy-statusline

A `#![forbid(unsafe_code)]` Rust statusline and local usage dashboard for Antigravity CLI (`agy >= 1.5.15`), with quota pacing, real compaction-threshold sniffing, subagent-aware `models.dev` cost estimation, pure-Rust `.git/HEAD` + worktree awareness, and an interactive 3-pane TUI configurator.

## Requirements

- **Antigravity CLI `>= 1.5.15`** (and **Antigravity Desktop `>= 2.35.0`** for shared `~/.gemini/antigravity` stats), where upstream 500-conversation startup pruning is disabled and `conversations/*.db` serves as the single authoritative usage store.

## Install

```sh
cargo install agy-statusline
```

Or build from source (requires Rust 2021; `libsqlite3` is bundled by default, pass `--no-default-features` to link system `libsqlite3`):

```sh
git clone https://github.com/fdkevin0/agy-statusline-rs.git
cd agy-statusline-rs
cargo install --path . --locked
```

## Quick Start

1. Configure your layout interactively (keyboard + mouse):

```sh
agy-statusline config
```

2. Register with AGY (`~/.gemini/antigravity-cli/settings.json`):

```sh
agy-statusline apply --yes
```

3. Verify your configuration, AGY hook, and sample output:

```sh
agy-statusline doctor
```

4. Inspect local CLI & desktop daily/model token usage, cache hit rates, speed, TTFT, and estimated API cost interactively (or `--plain` / `--json` for static output):

```sh
agy-statusline stats
agy-statusline stats --plain --metric cost --days 30
```

> Press `s` inside `agy-statusline config` to save and apply in one step.

## Documentation

- [Configuration, Widgets & TUI Guide](docs/configuration.md)
- [Contributing](CONTRIBUTING.md) · [Code of Conduct](CODE_OF_CONDUCT.md) · [Security Policy](SECURITY.md)

## License

[MIT](LICENSE)
