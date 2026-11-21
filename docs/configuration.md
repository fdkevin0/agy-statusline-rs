# Configuration & Reference

`agy-statusline` stores its layout configuration at `$XDG_CONFIG_HOME/agy-statusline/config.json` (default: `<home>/.config/agy-statusline/config.json`, resolving `<home>` from `HOME` or `USERPROFILE` on Windows). AGY CLI data paths honor `$ANTIGRAVITY_CLI_HOME`, then `$AGY_HOME`, defaulting to `<home>/.gemini/antigravity-cli`. Rendering respects the [`NO_COLOR`](https://no-color.org/) environment variable.

## CLI Commands

- `agy-statusline [--preset <full|powerline|minimal|addon>|--full]` — Render statusline from `stdin` JSON (prints help when run on an empty interactive TTY).
- `agy-statusline config` — Launch the 3-pane `ratatui` TUI editor.
- `agy-statusline config --preset <name>` — Apply a built-in preset non-interactively.
- `agy-statusline config --show` — Print active JSON configuration.
- `agy-statusline config --reset` — Remove `config.json` and restore defaults.
- `agy-statusline apply [--yes|-y] [--stack|--replace]` — Write `statusLine` into `<agy_cli_dir>/settings.json` (`$ANTIGRAVITY_CLI_HOME`, `$AGY_HOME`, or `~/.gemini/antigravity-cli/settings.json`) while preserving existing custom keys such as `padding` and recording the invoked launcher/symlink path rather than dereferencing symlinks.
- `agy-statusline apply --disable` — Set `statusLine.enabled = false` in `settings.json` and retain your hook configuration. Both `apply` commands preserve existing Unix file permissions.
- `agy-statusline doctor` — Report whether the layout config loaded, is missing, is invalid, or could not be read. Inspect AGY `settings.json` wiring and `NO_COLOR`, and render a sample preview.
- `agy-statusline stats [--days <N>] [--metric <METRIC>] [--tui|--plain|--json] [--ascii]` — Inspect local AGY CLI (`$ANTIGRAVITY_CLI_HOME` / `$AGY_HOME` / `~/.gemini/antigravity-cli`) and desktop (`~/.gemini/antigravity`) daily & per-model usage in the host local timezone. Launches an interactive `ratatui` TUI automatically on a TTY (or outputs a static report with daily breakdown when piped or passed `--plain`; `--json` outputs machine-readable `StatsReport`). Supports selectable & composable metrics (`total`, `cost`, `io`, `input`, `output`, `cache`, `cache_hit`, `cache_read`, `cache_write`, `thinking`, `turns`, or `+` combinations like `input+output+cache_write`), remembers the last interactively selected metric in `config.json` (`stats_metric`), and incrementally archives per-turn usage into `$XDG_DATA_HOME/agy-statusline/stats_history.db` so history survives AGY's 500-conversation startup pruning. `NO_COLOR` disables ANSI escapes.
- `agy-statusline stats sync [--if-stale]` — Incrementally sync local conversation DBs into `$XDG_DATA_HOME/agy-statusline/stats_history.db` (`--if-stale` skips when synced within the last 30 minutes; statusline runs trigger this automatically in the background at most every 30 minutes).
- `agy-statusline pricing [status|sync [--if-stale]]` — Inspect the local `models.dev` pricing cache status (`status`, default), or refresh cached pricing from `models.dev` (`sync`, or `sync --if-stale` to fetch only when missing or past the 24h TTL).
- `agy-statusline version | --version | -V` — Print version information (`agy-statusline <version>`).

## Usage Pricing

Native AGY costs take precedence. For estimates, the CLI uses the `models.dev` catalog cached for 24 hours. `stats` refreshes stale rates before reporting, and the statusline can refresh them in the background. Existing hand-written `pricing.json` files are ignored.

Catalog lookup checks exact model names case-insensitively, then names with normalized punctuation and spacing, then the longest matching substring. Within each exact-match stage, the model ID precedes the display name, and canonical provider rates take precedence over reseller punctuation variants. When prompt context tokens exceed a `models.dev` context-length tier threshold (`tiers` / `context_over_200k`), the corresponding tier rates are applied. Carbon and Argon Orchestrator aliases (including `gemini-flash-worker-agy` -> `gemini-4-hydrogen`) are used when direct matching finds no rate.

## Interactive TUI Controls

- **Pan & Select**: `Tab` or left-click switches panes (`1. Layout Lines`, `2. Catalog`, `3. Style & AGY`); `↑↓←→` / mouse wheel navigates.
- **Move Mode**: Press `Enter` (or click a selected pill) to grab (`⇄`), move within or across `Line 1–3` with arrow keys, and press `Enter` to drop. Right-click a pill to remove it.
- **Multi-Group Alignment**: Toggle `spacer` per line (or press `+` on `spacer` in `Catalog` for two spacers) to align `Left | Center | Right`.
- **Live Preview**: Click the preview panel or press `w` to cycle `Auto` / `120col` / `72col`.
- **Presets & Exit**: `1`–`4` apply presets; `s` saves `config.json` and updates AGY `settings.json`; `S` saves `config.json` only; `q` / `Esc` quits without saving.

### Interactive Stats TUI (`agy-statusline stats`)

- **Metrics & Custom Token Composition**: Press `1`–`9` (or `[` / `]`, or click top pills) to switch presets (`Total`, `Cost ($)`, `In+Out`, `Input`, `Output`, `Cache`, `Think`, `Turns`, `Cache%`). Toggle individual token buckets with `i` (Input), `o` (Output), `c` (Cache Read), and `w` (Cache Write) to compose custom non-overlapping metrics. The Daily Usage table also displays color-coded per-day `Cache%` alongside any active metric (green `>=80%`, yellow `50%–79%`, red `<50%`). The last selected metric is saved to `config.json` (`stats_metric`) on exit.
- **Range, Sorting & Filtering**: `t` / `T` cycles time ranges (`7d` / `14d` / `30d` / `90d` / `All`); `s` cycles daily sort (`Date ↓`, `Date ↑`, `Value ↓`, `Value ↑`); `z` toggles zero-usage calendar gaps; `f` filters `All` / `CLI` / `Desktop`; `g` toggles the 20-week metric-colored heatmap (clicking a tile jumps directly to that date).
- **Drill-Down & Refresh**: `↑↓` / `j` / `k` / mouse wheel navigates daily rows or models; `Tab` switches between `1. Daily Usage` and `3. Models`; `v` toggles models between `Selected Day` and `Window`; `Enter` / left-click on a model cross-filters the dashboard (`Esc` clears the filter); `r` refreshes local SQLite data; `q` quits.

## Presets & Defaults

- **`full`** *(default, replaces AGY's built-in bar)*: 2-row `Plain` layout (`Default` theme)
  - **Row 1**: `state`, `mode`, `model`, `directory`, `vcs`, `worktree`, `queue`, `vim`, `battle`
  - **Row 2**: `ctx_bar`, `cache_hit`, `speed`, `cost`, `quota_5h`, `quota_weekly`, `subagents`
- **`powerline`**: 2-row `Powerline` layout (`Catppuccin` theme) — Row 1: `state`, `mode`, `model`, `directory`, `vcs`; Row 2: `ctx_bar`, `cache_hit`, `speed`, `cost`, `quota_5h`.
- **`minimal`**: 1-row `Plain` layout — `state`, `model`, `vcs`, `ctx_pct`, `cache_hit`, `speed`.
- **`addon`**: 1-row `Pill` metrics bar stacked under AGY's built-in statusline (`stack_with_default: true`) — `cache_hit`, `cache_read`, `speed`, `cost`.
- **Defaults**: `bar_length: 8` (`4..=40`), `speed_warn_threshold: 35.0` tok/s, `style: "plain"`, `theme: "default"`, `separator: "dot"`, `glyph_mode: "unicode"`, `speed_mode: "weighted"`, `responsive: true`, `smart_hide: true`, `show_git_diff: true`, `ctx_remaining: false`.

## Widgets (33)

Every widget reads from AGY's `stdin` JSON payload (`StatuslinePayload`), local SQLite `conversations/<conversation_id>.db` protobuf metadata (`gen_metadata`), or pure-Rust `.git` probing. `Prio` is the responsive drop priority (`5` = anchor kept last, `1` = auxiliary dropped first):

| ID | Prio | Description | Data Source & Calculation |
| :--- | :---: | :--- | :--- |
| `state` | 5 | Agent state | `tool_confirmation_pending` (takes precedence as `CONFIRM`) -> `agent_state` (`idle` -> `READY`, `thinking` -> `THINKING`, `working` -> `WORKING`, `tool_use` -> `TOOL`, `reviewing` -> `REVIEW`, `authenticating` -> `AUTH`, `initializing` -> `INIT`, `completed` -> `DONE`, `error` -> `ERROR`, fallback uppercase) |
| `model` | 4 | Model + effort | `model.display_name` + `model.effort`; appends effort only when not already matched structurally in a trailing group or suffix (including `x`/`extra` equivalence) |
| `directory` | 2 | Workspace dir | Basename of `workspace.project_dir`, falling back to `workspace.current_dir`, then top-level `cwd` |
| `vcs` | 3 | VCS branch + diff | `vcs.branch`, falling back to `vcs.client` (non-Git VCS such as CitC/Perforce), then pure-Rust `.git/HEAD` probe; dirty `*` from `vcs.dirty` or non-zero diff; optional bounded/cached `+ins -del` from `git diff --numstat` when `show_git_diff` is enabled |
| `worktree` | 2 | Git worktree | Probed from `.git` file `gitdir:` pointer (supports relative `gitdir:` paths); renders `wt:<name>` inside linked worktrees (`wt:main` or hidden when `smart_hide` is enabled) |
| `ctx_bar` | 4 | Context progress bar | Effective limit prefers SQLite `max_context_tokens` (`1 -> 9 -> 10 -> 4`) over `context_window.context_window_size`; percentage computed as `current_tokens / limit * 100` (using `current_usage` sum or SQLite `estimated_tokens_used` `1 -> 9 -> 10 -> 1`, else `context_window.used_percentage`). Displays `(current/limit)` (or `(remaining/limit)` when `ctx_remaining` is enabled, falling back to `(limit)` if current tokens are absent); visual bar clamps `0..=100%` while numeric `%` displays `>100%` overflow |
| `ctx_pct` | 4 | Context % only | Same context calculation as `ctx_bar`; supports `ctx_remaining` (`100 - used` as `rem` vs `ctx`) and `>100%` overflow |
| `ctx_tokens` | 2 | Context tokens | Renders `current/limit` (from `current_usage` / SQLite `estimated_tokens_used` and effective limit) or cumulative `context_window.total_input_tokens + total_output_tokens`, formatted with `K` (`1,000..999,500`, tenth-K below `10K`), `M` (`999,500..999,500,000`), and `B` (`>= 999,500,000`) symmetric boundary promotion (`999_500 -> 1M`, `999_500_000 -> 1B`) |
| `cache_hit` | 3 | Cache hit % | From `context_window.current_usage`: `cache_read_input_tokens / (input_tokens + cache_read_input_tokens + cache_creation_input_tokens) * 100` |
| `cache_read` | 1 | Cache read tokens | `context_window.current_usage.cache_read_input_tokens` formatted with `K` / `M` / `B` symmetric boundary promotion |
| `speed` | 3 | Output speed | From SQLite `gen_metadata` (`ORDER BY idx DESC LIMIT 9`, valid turns with decode duration `>= 0.01s` and `< 10,000 tok/s`): `Weighted 5t` (`sum(out) / sum(decode_s)`), `Median` (up to 9 turns), `Instant` (latest turn), or `E2E (+TTFT)` (`out / (ttft + decode_s)`) |
| `ttft` | 1 | First token latency | Latest turn time-to-first-token extracted from SQLite `gen_metadata` (`1 -> 11`); hidden when absent if `smart_hide` is enabled |
| `thinking` | 1 | Thinking tokens | Latest turn thinking output tokens extracted from SQLite `gen_metadata` (`1 -> 4 -> 9`); hidden when absent/zero if `smart_hide` is enabled |
| `cost` | 3 | Session cost | Prefers payload `cost.total_usd` (with `(sub $...)` when `cost.subagent_usd >= 0.01`); otherwise sums native turn costs or cached `models.dev` rates across the active conversation DB and any descendant subagent DBs linked in `conversation_summaries.db`, rendering `(sub $...)` when subagent spend is `>= $0.01`, and `(N unpriced)` / `(sub $..., N unpriced)` for partial coverage. Without recorded usage it can estimate from session totals or label a current-usage estimate as `latest turn` |
| `mode` | 3 | Cycle mode | `cycle_mode` uppercased (`PLAN` / `AUTO` / `EDIT`, etc.), plus `>200K` badge when `exceeds_200k_tokens` is true |
| `quota_5h` | 4 | 5h quota + pace | Canonical JSON ID `"quota_5h"` (accepts legacy `"quota5h"` alias on deserialization); selects short-window (`<24h`) non-`disabled` bucket from `quota` via model token-affinity scoring; reads `remaining_fraction` (sub-1% precision: `<0.1%`, `0.1%`–`0.9%`) or `remaining_amount`, countdown from `reset_in_seconds` (or RFC3339 `reset_time` fallback), calibrated `(~tokens)` estimate, and burn-rate pace warning; hidden when absent if `smart_hide` is enabled |
| `quota_weekly` | 3 | Weekly quota | Selects multi-day (`>=24h`) non-`disabled` bucket from `quota` via token-affinity scoring; dynamic qualifier/window labeling, `remaining_fraction` or `remaining_amount`, countdown timer, calibrated `(~tokens)` estimate, and burn-rate pace warning; hidden when absent if `smart_hide` is enabled |
| `artifacts` | 1 | Artifact count | `artifact_count` rendered as `art <n>`; hidden when `0` if `smart_hide` is enabled |
| `subagents` | 1 | Subagent count | Counts entries in `subagents` array where `status != "completed"`; when exactly 1 active subagent has a `role` or `name`, renders `sub 1 (<label>)` (capped at 20 cols); hidden when `0` if `smart_hide` is enabled |
| `tasks` | 1 | Task count | `task_count` rendered as `tasks <n>`; hidden when `0` if `smart_hide` is enabled |
| `queue` | 2 | Queued inputs | `pending_input_count` rendered as `queue <n>`; hidden when `0` if `smart_hide` is enabled |
| `vim` | 2 | Vim mode | `vim.mode` uppercased as `vim <MODE>` (`NORMAL`, `INSERT`, `VISUAL`, `COMMAND`, etc.); hidden when absent if `smart_hide` is enabled |
| `battle` | 1 | Battle mode | `battle.status` uppercased as `battle <STATUS>` (with optional `#<battle.focused_arm>`); hidden when absent if `smart_hide` is enabled |
| `sandbox` | 1 | Sandbox badge | `sandbox.enabled` + `sandbox.allow_network`: `sandbox ON+net` when network is allowed, `sandbox ON` otherwise; hidden when `off` if `smart_hide` is enabled |
| `version` | 1 | AGY version | `version` trimmed and prefixed with `v` unless already prefixed (`v1.5.11`); omitted when absent or empty |
| `plan` | 1 | Plan tier | `plan_tier` trimmed (`Pro`, `Ultra`, etc.); omitted when absent or empty |
| `account` | 1 | Account email | `email` trimmed (`alice@example.com`); compacts to local-part (`alice`) on narrow rows; omitted when absent or empty |
| `title` | 2 | Conversation title | `conversation_title` whitespace-collapsed and truncated to 32 display columns (16 columns under responsive compaction); omitted when absent or empty |
| `agent` | 2 | Active agent | `agent.name` whitespace-collapsed as `agent <name>`; omitted when absent or empty |
| `product` | 1 | Product name | `product` trimmed (`Antigravity`); omitted when absent or empty |
| `session` | 1 | Session ID | First 8 characters of `session_id` rendered as `sid:<prefix>`; omitted when absent or empty |
| `transcript` | 1 | Transcript file | Basename of `transcript_path` rendered as `log:<filename>`; omitted when absent or empty |
| `spacer` | 5 | Flex spacer | Layout primitive (no payload data); splits row into `Left | Right` or `Left | Center | Right` groups |

## Styles, Themes, Glyphs & Data Sources

- **Styles**: `Plain`, `Pill` (256-color capsules, rounded in `Nerd` mode), `Powerline` (`` / `>` segments).
- **Themes**: `Default`, `Nord`, `Catppuccin`, `Tokyo Night`, `Gruvbox`, `Light`, `Mono`.
- **Glyph Modes**: `Unicode` (default), `Nerd Font`, `ASCII` (pure 7-bit ASCII).
- **Responsive Compaction**: When `responsive` is enabled and a row overflows terminal width, segments compact progressively before dropping lower-priority widgets: `vcs` diff strip -> `vcs` 20-col cap -> `subagents` label strip -> `cost` suffix strip (`(sub ...)`, `(N unpriced)`, `(latest turn)`) -> `quota_*` calibrated `(~tokens)` strip -> `quota_*` reset timer strip -> `ctx_*` limit strip -> `ctx_bar` collapse to `%` -> `model` leading-word abbreviation (including parenthesized names such as `Orchestrator (Carbon) · high` -> `O (Carbon) · high`) -> `model` 20-col cap -> `account` `@domain` strip -> `title` 16-col cap.
- **SQLite Sniffing**: Queries `$ANTIGRAVITY_CLI_HOME/conversations/<conversation_id>.db` (or `$AGY_HOME`, defaulting to `<home>/.gemini/antigravity-cli/conversations/<conversation_id>.db`) in read-only mode with a 25ms `busy_timeout` (`gen_metadata` `ORDER BY idx DESC LIMIT 9`) to extract `max_context_tokens` (`1 -> 9 -> 10 -> 4`), `estimated_tokens_used` (`1 -> 9 -> 10 -> 1`), output token decode speed (5-turn weighted average and up to 9-turn median, `>= 0.01s` and `< 10,000 tok/s`), thinking output tokens (`1 -> 4 -> 9`), and TTFT (`1 -> 11`). `agy-statusline stats` additionally scans desktop `<home>/.gemini/antigravity` sessions and decodes `step_indices` (`2`), `model` (`1 -> 3`), `model_cost` (`1 -> 5`), `response_model` (`1 -> 19`), `model_display_name` (`1 -> 21`), and `steps.metadata.created_at` (`1 -> 1`).
- **Quota Token Calibration**: Learns rolling quota token budgets per raw bucket ID (`$XDG_CACHE_HOME/agy-statusline/quota_cal.json`, dynamic TTL `max(3 * window_secs, 7d)`) and renders `(~<tokens>)` next to the quota percentage once calibrated. Writers hold a nonblocking lock on `quota_cal.lock` across the cache read, sampling, and atomic replacement. If the lock is busy, the statusline skips that sample and displays any unexpired cached estimate.
- **Data Files (`$XDG_DATA_HOME/agy-statusline/`)**: Defaults to `<home>/.local/share/agy-statusline/` (using `HOME` or `USERPROFILE`):
  - `stats_history.db` / `stats_history.lock` — persistent incremental SQLite archive of per-turn usage facts keyed by `(root_path, uuid)` (so historical stats survive AGY's 500-DB startup pruning and support retroactive timezone/pricing projection) and cold-init/migration lock.
- **Cache Files (`$XDG_CACHE_HOME/agy-statusline/`)**: Defaults to `<home>/.cache/agy-statusline/` (using `HOME` or `USERPROFILE`):
  - `quota_cal.json` / `quota_cal.lock` — rolling quota token calibration store and nonblocking writer lock.
  - `models_dev_pricing.json` / `models_dev_pricing.err` — 24h TTL auto-fetched `models.dev` pricing cache and fetch-failure backoff marker.
  - `stats_archive_sync.ts` — 30-minute background archive sync throttle timestamp.
  - `diff_*.txt` — short-TTL per-repository `git diff --numstat` cache files.
