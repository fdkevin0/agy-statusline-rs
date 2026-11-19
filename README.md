# agy-statusline

A `#![forbid(unsafe_code)]` Rust statusline for Antigravity CLI (`agy`) with quota pacing, real compaction-threshold sniffing, pure-Rust `.git/HEAD` + worktree awareness, and an interactive 3-pane TUI configurator.

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

4. Inspect local CLI & desktop token usage, per-model speed, TTFT, and estimated API cost (native `model_cost`, auto-cached `models.dev` rates with 24h TTL, and optional `pricing.json` overrides):

```sh
agy-statusline stats --days 30
```

> Press `s` inside `agy-statusline config` to save and apply in one step.

## Documentation

- [Configuration, Widgets & TUI Guide](docs/configuration.md)
- [Contributing](CONTRIBUTING.md) · [Code of Conduct](CODE_OF_CONDUCT.md) · [Security Policy](SECURITY.md)

## License

[MIT](LICENSE)
