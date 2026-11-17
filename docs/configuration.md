# Configuration & Reference

`agy-statusline` stores its layout configuration at `$XDG_CONFIG_HOME/agy-statusline/config.json` (default: `~/.config/agy-statusline/config.json`).

## CLI Commands

- `agy-statusline [--preset <full|powerline|minimal|addon>|--full]` — Render statusline from `stdin` JSON.
- `agy-statusline config` — Launch the 3-pane `ratatui` TUI editor.
- `agy-statusline config --preset <name>` — Apply a built-in preset non-interactively.
- `agy-statusline config --show` — Print active JSON configuration.
- `agy-statusline config --reset` — Remove `config.json` and restore defaults.
- `agy-statusline apply [--yes|-y] [--stack|--replace]` — Write `statusLine` into `~/.gemini/antigravity-cli/settings.json`.

## Interactive TUI Controls

- **Pan & Select**: `Tab` or left-click switches panes (`1. Layout Lines`, `2. Catalog`, `3. Style & AGY`); `↑↓←→` / mouse wheel navigates.
- **Move Mode**: Press `Enter` (or click a selected pill) to grab (`⇄`), move within or across `Line 1–3` with arrow keys, and press `Enter` to drop. Right-click a pill to remove it.
- **Live Preview**: Click the preview panel or press `w` to cycle `Auto` / `120col` / `72col`.
- **Presets & Exit**: `1`–`4` apply presets; `s` saves `config.json` and updates AGY `settings.json`; `S` saves `config.json` only; `q` / `Esc` quits without saving.

## Widgets (18)

| ID | Description | Notes |
| :--- | :--- | :--- |
| `state` | Agent state | `● READY`, `◆ THINKING`, `⚙ WORKING`, `🔧 TOOL` |
| `model` | Model + effort | Appends effort only if not already in `display_name` |
| `directory` | Workspace dir | Basename of `project_dir` or `current_dir` |
| `vcs` | Git branch + diff | Shows `+ins -del` when `show_git_diff` is enabled |
| `worktree` | Git worktree | `⎇ wt:<name>` inside linked worktrees (auto-hidden otherwise) |
| `ctx_bar` | Context progress bar | Uses sniffed `max_context_tokens` when available |
| `ctx_pct` | Context % only | Supports `ctx_remaining` (`rem` vs `ctx`) |
| `ctx_tokens` | Context tokens | `current/limit` or cumulative session tokens |
| `cache_hit` | Cache hit % | `cache_read / (input + cache_read)` |
| `cache_read` | Cache read tokens | Formatted with `K` grouping |
| `speed` | Output speed | `Weighted 5t`, `Instant`, or `E2E (+TTFT)` |
| `ttft` | First token latency | Extracted from `gen_metadata` (`chat.11`) |
| `cost` | Session cost | `~$usd` when estimated |
| `artifacts` | Artifact count | Hidden when `0` if `smart_hide` is enabled |
| `subagents` | Subagent count | Hidden when `0` if `smart_hide` is enabled |
| `tasks` | Task count | Hidden when `0` if `smart_hide` is enabled |
| `sandbox` | Sandbox badge | Hidden when `off` if `smart_hide` is enabled |
| `spacer` | Flex spacer | Splits row into left- and right-aligned groups |

## Styles, Themes & Data Sources

- **Styles**: `Plain`, `Pill` (256-color capsules), `Powerline` (`` segments).
- **Themes**: `Default`, `Nord`, `Catppuccin`, `Mono`.
- **SQLite Sniffing**: Queries `~/.gemini/antigravity-cli/conversations/<conversation_id>.db` (`gen_metadata` `ORDER BY idx DESC LIMIT 5`) in read-only mode to extract `max_context_tokens` (`1 -> 9 -> 10 -> 4`), output token decode speed, and TTFT.
