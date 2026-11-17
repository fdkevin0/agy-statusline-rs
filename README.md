# agy-statusline

A `#![forbid(unsafe_code)]` Rust statusline for Antigravity CLI (`agy`).

Reads AGY's statusline JSON from `stdin` and displays:
- **Cache hit rate** (`cache_read_input_tokens / (input_tokens + cache_read_input_tokens)`) and formatted cache read token count. Displays `--` when data is missing, input is zero, or `cache_creation_input_tokens` is non-zero.
- **Last turn average output speed** (tokens/s, including thinking tokens) from a read-only query against `~/.gemini/antigravity-cli/conversations/<conversation_id>.db`. Color-codes the bar yellow when `< 50 token/s` and blue otherwise.

## Installation

### From crates.io

```sh
cargo install agy-statusline
```

### From source

Requires Rust (2021 edition) and `libsqlite3` development headers:

```sh
git clone https://github.com/fdkevin0/agy-statusline-rs.git
cd agy-statusline-rs
cargo install --path .
```

## Quick Setup

Automatically configure `~/.gemini/antigravity-cli/settings.json`:

```sh
agy-statusline config --yes
```

Omit `--yes` to preview the configuration and confirm interactively. Existing keys in `settings.json` are preserved.

### Manual Configuration

Edit `~/.gemini/antigravity-cli/settings.json` (Windows: `%USERPROFILE%\.gemini\antigravity-cli\settings.json`):

```json
{
  "statusLine": {
    "type": "command",
    "command": "/path/to/agy-statusline",
    "stack_with_default": true
  }
}
```

To restore the default statusline, run `/statusline delete` in AGY or remove `statusLine` from `settings.json`.

## Development

```sh
cargo test --all-targets
cargo test --doc
cargo clippy --all-targets -- -D warnings
```

## License

MIT
