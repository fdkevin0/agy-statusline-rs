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
- `agy-statusline stats [--days <N>] [--json] [--ascii]` — Summarize local AGY token usage in the host local timezone (Today / 7d / 30d / All-Time), 4-week Monday-anchored activity calendar, per-model thinking tokens / median speed / median TTFT from SQLite `conversations/<uuid>.db` (`gen_metadata` + `steps.metadata`, with deduplicated `transcript.jsonl` fallback for non-DB sessions), and optional user-configured cost from `$XDG_CONFIG_HOME/agy-statusline/pricing.json` (`NO_COLOR` disables ANSI escapes).

## Interactive TUI Controls

- **Pan & Select**: `Tab` or left-click switches panes (`1. Layout Lines`, `2. Catalog`, `3. Style & AGY`); `↑↓←→` / mouse wheel navigates.
- **Move Mode**: Press `Enter` (or click a selected pill) to grab (`⇄`), move within or across `Line 1–3` with arrow keys, and press `Enter` to drop. Right-click a pill to remove it.
- **Multi-Group Alignment**: Toggle `spacer` per line (or press `+` on `spacer` in `Catalog` for two spacers) to align `Left | Center | Right`.
- **Live Preview**: Click the preview panel or press `w` to cycle `Auto` / `120col` / `72col`.
- **Presets & Exit**: `1`–`4` apply presets; `s` saves `config.json` and updates AGY `settings.json`; `S` saves `config.json` only; `q` / `Esc` quits without saving.

## Widgets (25)

| ID | Description | Notes |
| :--- | :--- | :--- |
| `state` | Agent state | `READY`, `THINKING`, `WORKING`, `TOOL`, `REVIEW`, `AUTH`, `INIT`, `DONE`, `ERROR`, or `CONFIRM` when `tool_confirmation_pending` is true |
| `model` | Model + effort | Appends effort only when not already matched structurally in a trailing group or suffix (including `x`/`extra` equivalence) |
| `directory` | Workspace dir | Basename of `project_dir`, `current_dir`, or `cwd` |
| `vcs` | Git branch + diff | Pure-Rust `.git/HEAD` fallback + bounded/cached `+ins -del` diff stats |
| `worktree` | Git worktree | `wt:<name>` inside linked worktrees including relative `gitdir:` paths |
| `ctx_bar` | Context progress bar | Uses sniffed `max_context_tokens` when available; visual bar clamps `0..=100%` while numeric `%` displays `>100%` overflow |
| `ctx_pct` | Context % only | Supports `ctx_remaining` (`rem` vs `ctx`) and `>100%` overflow |
| `ctx_tokens` | Context tokens | `current/limit` or cumulative session tokens (`1K..10K` tenth-K, `10K..999K` rounded `K`) |
| `cache_hit` | Cache hit % | `cache_read / (input + cache_read + cache_creation)` |
| `cache_read` | Cache read tokens | Formatted with `K` / `M` grouping |
| `speed` | Output speed | `Weighted 5t`, `Median`, `Instant`, or `E2E (+TTFT)` |
| `ttft` | First token latency | Extracted from `gen_metadata` (`chat.11`) |
| `thinking` | Thinking tokens | Extracted from `gen_metadata` (`chat.4.9`); hidden when absent/zero if `smart_hide` is enabled |
| `cost` | Session cost | `~$usd` and `(sub $...)` when `subagent_usd >= 0.01` |
| `mode` | Cycle mode | `PLAN` / `AUTO` / `EDIT` plus `>200K` indicator when exceeded |
| `quota_5h` | 5h quota + pace | Dynamic short-window (`<24h`) bucket via token-affinity scoring, `disabled` filtering, discrete credit amounts, sub-1% precision (`<0.1%`, `0.1%`–`0.9%`), `reset_time` RFC3339 fallback, calibrated `(~tokens)` estimate, and pace warn |
| `quota_weekly` | Weekly quota | Dynamic multi-day (`>=24h`) bucket via token-affinity scoring, dynamic qualifier/window labeling, `disabled` filtering, discrete credit amounts, and calibrated `(~tokens)` estimate |
| `artifacts` | Artifact count | Hidden when `0` if `smart_hide` is enabled |
| `subagents` | Subagent count | Hidden when `0` if `smart_hide` is enabled |
| `tasks` | Task count | Hidden when `0` if `smart_hide` is enabled |
| `queue` | Queued inputs | `queue <n>` from `pending_input_count`; hidden when `0` if `smart_hide` is enabled |
| `vim` | Vim mode | `vim <MODE>` (`NORMAL`, `INSERT`, `VISUAL`, `COMMAND`, etc.); hidden when absent if `smart_hide` is enabled |
| `battle` | Battle mode | `battle <STATUS>` (with optional `#<focused_arm>`); hidden when absent if `smart_hide` is enabled |
| `sandbox` | Sandbox badge | `sandbox ON+net` when `allow_network` is true, `sandbox ON` otherwise; hidden when `off` if `smart_hide` is enabled |
| `spacer` | Flex spacer | Splits row into `Left | Right` or `Left | Center | Right` groups |

## Styles, Themes, Glyphs & Data Sources

- **Styles**: `Plain`, `Pill` (256-color capsules, rounded in `Nerd` mode), `Powerline` (`` / `>` segments).
- **Themes**: `Default`, `Nord`, `Catppuccin`, `Tokyo Night`, `Gruvbox`, `Light`, `Mono`.
- **Glyph Modes**: `Unicode` (default), `Nerd Font`, `ASCII` (pure 7-bit ASCII).
- **Responsive Compaction**: When `responsive` is enabled and a row overflows terminal width, segments compact progressively (`vcs` diff strip / 20-col cap -> `quota_*` timer strip -> `ctx_*` limit strip & `ctx_bar` collapse -> `model` abbreviation / 20-col cap) before dropping lower-priority widgets.
- **SQLite Sniffing**: Queries `$ANTIGRAVITY_CLI_HOME/conversations/<conversation_id>.db` (or `$AGY_HOME`, defaulting to `~/.gemini/antigravity-cli/conversations/<conversation_id>.db`) in read-only mode with a 25ms `busy_timeout` (`gen_metadata` `ORDER BY idx DESC LIMIT 9`) to extract `max_context_tokens` (`1 -> 9 -> 10 -> 4`), `estimated_tokens_used` (`1 -> 9 -> 10 -> 1`), output token decode speed (5-turn weighted average and up to 9-turn median, `>= 0.01s` and `< 10,000 tok/s`), thinking output tokens (`1 -> 4 -> 9`), and TTFT (`1 -> 11`). `agy-statusline stats` additionally decodes `step_indices` (`2`), `model` (`1 -> 3`), `model_cost` (`1 -> 5`), `response_model` (`1 -> 19`), `model_display_name` (`1 -> 21`), and `steps.metadata.created_at` (`1 -> 1`).
- **Quota Token Calibration**: Learns rolling quota token budgets per raw bucket ID (`$XDG_CACHE_HOME/agy-statusline/quota_cal.json`, dynamic TTL `max(3 * window_secs, 7d)`) and renders `(~<tokens>)` next to the quota percentage once calibrated.
