# Configuration & Reference

`agy-statusline` stores its layout configuration at `$XDG_CONFIG_HOME/agy-statusline/config.json` (default: `~/.config/agy-statusline/config.json`). Rendering respects the [`NO_COLOR`](https://no-color.org/) environment variable.

## CLI Commands

- `agy-statusline [--preset <full|powerline|minimal|addon>|--full]` — Render statusline from `stdin` JSON (prints help when run on an empty interactive TTY).
- `agy-statusline config` — Launch the 3-pane `ratatui` TUI editor.
- `agy-statusline config --preset <name>` — Apply a built-in preset non-interactively.
- `agy-statusline config --show` — Print active JSON configuration.
- `agy-statusline config --reset` — Remove `config.json` and restore defaults.
- `agy-statusline apply [--yes|-y] [--stack|--replace]` — Write `statusLine` into `~/.gemini/antigravity-cli/settings.json` while preserving existing custom keys such as `padding`.
- `agy-statusline apply --disable` — Set `statusLine.enabled = false` in `settings.json` without deleting your hook configuration.
- `agy-statusline doctor` — Inspect config, AGY `settings.json` wiring, `NO_COLOR`, and render a live sample preview.

## Interactive TUI Controls

- **Pan & Select**: `Tab` or left-click switches panes (`1. Layout Lines`, `2. Catalog`, `3. Style & AGY`); `↑↓←→` / mouse wheel navigates.
- **Move Mode**: Press `Enter` (or click a selected pill) to grab (`⇄`), move within or across `Line 1–3` with arrow keys, and press `Enter` to drop. Right-click a pill to remove it.
- **Multi-Group Alignment**: Toggle `spacer` per line (or press `+` on `spacer` in `Catalog` for two spacers) to align `Left | Center | Right`.
- **Live Preview**: Click the preview panel or press `w` to cycle `Auto` / `120col` / `72col`.
- **Presets & Exit**: `1`–`4` apply presets; `s` saves `config.json` and updates AGY `settings.json`; `S` saves `config.json` only; `q` / `Esc` quits without saving.

## Widgets (21)

| ID | Description | Notes |
| :--- | :--- | :--- |
| `state` | Agent state | `READY`, `THINKING`, `WORKING`, `TOOL`, `REVIEW`, `AUTH`, `INIT`, `ERROR` |
| `model` | Model + effort | Appends effort only if not already in `display_name` |
| `directory` | Workspace dir | Basename of `project_dir`, `current_dir`, or `cwd` |
| `vcs` | Git branch + diff | Pure-Rust `.git/HEAD` fallback + bounded/cached `+ins -del` diff stats |
| `worktree` | Git worktree | `wt:<name>` inside linked worktrees including relative `gitdir:` paths |
| `ctx_bar` | Context progress bar | Uses sniffed `max_context_tokens` when available |
| `ctx_pct` | Context % only | Supports `ctx_remaining` (`rem` vs `ctx`) |
| `ctx_tokens` | Context tokens | `current/limit` or cumulative session tokens |
| `cache_hit` | Cache hit % | `cache_read / (input + cache_read + cache_creation)` |
| `cache_read` | Cache read tokens | Formatted with `K` / `M` grouping |
| `speed` | Output speed | `Weighted 5t`, `Instant`, or `E2E (+TTFT)` |
| `ttft` | First token latency | Extracted from `gen_metadata` (`chat.11`) |
| `cost` | Session cost | `~$usd` and `(sub $...)` when `subagent_usd >= 0.01` |
| `mode` | Cycle mode | `PLAN` / `AUTO` / `EDIT` plus `>200K` indicator when exceeded |
| `quota_5h` | 5h quota + pace | Model-aware `gemini-5h` / `3p-5h` remaining % + reset timer + pace warn |
| `quota_weekly` | Weekly / Argon quota | Model-aware `gemini-weekly` / `3p-weekly` / `gemini-argon-wtu-weekly` |
| `artifacts` | Artifact count | Hidden when `0` if `smart_hide` is enabled |
| `subagents` | Subagent count | Hidden when `0` if `smart_hide` is enabled |
| `tasks` | Task count | Hidden when `0` if `smart_hide` is enabled |
| `sandbox` | Sandbox badge | Hidden when `off` if `smart_hide` is enabled |
| `spacer` | Flex spacer | Splits row into `Left | Right` or `Left | Center | Right` groups |

## Styles, Themes, Glyphs & Data Sources

- **Styles**: `Plain`, `Pill` (256-color capsules, rounded in `Nerd` mode), `Powerline` (`` / `>` segments).
- **Themes**: `Default`, `Nord`, `Catppuccin`, `Tokyo Night`, `Gruvbox`, `Light`, `Mono`.
- **Glyph Modes**: `Unicode` (default), `Nerd Font`, `ASCII` (pure 7-bit ASCII).
- **SQLite Sniffing**: Queries `~/.gemini/antigravity-cli/conversations/<conversation_id>.db` (`gen_metadata` `ORDER BY idx DESC LIMIT 5`) in read-only mode with a 25ms `busy_timeout` to extract `max_context_tokens` (`1 -> 9 -> 10 -> 4`), output token decode speed, and TTFT.
