# agy-statusline

A `#![forbid(unsafe_code)]` Rust statusline for Antigravity CLI (`agy`) with real compaction-threshold sniffing, `git worktree` awareness, and an interactive 3-pane TUI configurator.

## Install

```sh
cargo install agy-statusline
```

Or build from source (requires Rust 2021 + `libsqlite3` dev headers):

```sh
git clone https://github.com/fdkevin0/agy-statusline-rs.git
cd agy-statusline-rs
cargo install --path .
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

> Press `s` inside `agy-statusline config` to save and apply in one step.

## Documentation

- [Configuration, Widgets & TUI Guide](docs/configuration.md)
- [Contributing](CONTRIBUTING.md) · [Code of Conduct](CODE_OF_CONDUCT.md) · [Security Policy](SECURITY.md)

## License

[MIT](LICENSE)
