# Configuration & Reference

Requires **Antigravity CLI `>= 1.5.15`** (and **Antigravity Desktop `>= 2.35.0`** when aggregating desktop `~/.gemini/antigravity` sessions).

`agy-statusline` stores its layout configuration at `$XDG_CONFIG_HOME/agy-statusline/config.json` (default: `<home>/.config/agy-statusline/config.json`, resolving `<home>` from `HOME` or `USERPROFILE` on Windows). AGY CLI data paths honor `$ANTIGRAVITY_CLI_HOME`, defaulting to `<home>/.gemini/antigravity-cli`; `stats` also scans desktop `<home>/.gemini/antigravity` and any comma-separated roots in `$ANTIGRAVITY_DATA_DIR`. Rendering respects the [`NO_COLOR`](https://no-color.org/) environment variable.

## CLI Commands

- `agy-statusline [--preset <full|powerline|minimal|addon>|--full]` — Render statusline from `stdin` JSON (prints help when run on an empty interactive TTY).
- `agy-statusline config` — Launch the 3-pane `ratatui` TUI editor.
- `agy-statusline config --preset <name>` — Apply a built-in preset non-interactively.
- `agy-statusline config --show` — Print active JSON configuration.
- `agy-statusline config --reset` — Remove `config.json` and restore defaults.
- `agy-statusline apply [--yes|-y] [--stack|--replace]` — Write `statusLine` into `<agy_cli_dir>/settings.json` (`$ANTIGRAVITY_CLI_HOME` or `~/.gemini/antigravity-cli/settings.json`) while preserving existing custom keys such as `padding` and recording the invoked launcher/symlink path rather than dereferencing symlinks.
- `agy-statusline apply --disable` — Set `statusLine.enabled = false` in `settings.json` and retain your hook configuration. Both `apply` commands preserve existing Unix file permissions.
- `agy-statusline doctor` — Report whether the layout config loaded, is missing, is invalid, or could not be read. Probe `agy --version` on `PATH` (1s timeout), inspect AGY `settings.json` wiring, `<agy_cli_dir>/conversations` session `.db` count, `models_dev_pricing.json` cache status, and `NO_COLOR`, and render a sample preview.
- `agy-statusline stats [<CONVERSATION_ID>] [--all] [--days <N>] [--metric <METRIC>] [--tui|--plain|--json] [--ascii]` — Inspect local AGY CLI (`$ANTIGRAVITY_CLI_HOME` / `~/.gemini/antigravity-cli`), desktop (`~/.gemini/antigravity`), and optional `$ANTIGRAVITY_DATA_DIR` SQLite sessions for daily & per-model usage in the host local timezone. Launches an interactive `ratatui` TUI automatically on a TTY (or outputs a static report when piped or passed `--plain`; `--json` outputs machine-readable JSON). Pass a `<CONVERSATION_ID>` (full UUID or unique `>=4`-char hex prefix) to inspect a single conversation including descendant subagents, peak context, mid-session cache misses, per-model cache hit rates, and turn-by-turn breakdown (`--all` prints all turns statically instead of only the latest 20; `--json` outputs `ConversationStatsReport`). Global `stats` supports selectable metrics (`total`, `cost`, `io`, `output`, `turns`) and remembers the last interactively selected metric in `config.json` (`stats_metric`). `NO_COLOR` disables ANSI escapes.
- `agy-statusline conversations [QUERY] [--subagents] [--limit <N>] [--tui|--plain|--json] [--ascii]` *(aliases: `conv`, `sessions`)* — Browse and search historical conversations in a full-screen interactive picker on a TTY (without scanning all conversation DBs upfront); press `Enter` to open per-turn stats and `b` / `Esc` to return to the list. Pass an optional `[QUERY]` to pre-filter by title, workspace, agent name, or ID; pass `--plain`, `--limit <N>`, `--json`, or pipe stdout for non-interactive listing.
- `agy-statusline pricing [status|sync [--if-stale]]` — Inspect the local `models.dev` pricing cache status (`status`, default), or refresh cached pricing from `models.dev` (`sync`, or `sync --if-stale` to fetch only when missing or past the 24h TTL).
- `agy-statusline version | --version | -V` — Print version information (`agy-statusline <version>`).

## Usage Pricing

Native AGY costs take precedence. For estimates, the CLI uses the `models.dev` catalog cached for 24 hours. `stats` refreshes stale rates before reporting, and the statusline can refresh them in the background.

Catalog lookup checks exact model names case-insensitively, then names with normalized punctuation and spacing, then the longest matching substring. Within each exact-match stage, the model ID precedes the display name, and canonical provider rates take precedence over reseller punctuation variants. When prompt context tokens exceed a `models.dev` context-length tier threshold (`tiers` / `context_over_200k`), the corresponding tier rates are applied. When direct matching finds no rate for an Orchestrator name, explicit post-`orchestrator` codenames are resolved dynamically against canonical unprefixed `gemini-<major>[.<minor>]-<codename>` catalog entries (honoring any explicit pre-`orchestrator` generation or selecting the latest generation, without cross-tier fallback). In `stats`, turns with generic display names can additionally inherit a rate when the same `response_model` unambiguously resolves to a single catalog entry elsewhere in the dataset.

## Interactive TUI Controls

- **Pan & Select**: `Tab` or left-click switches panes (`1. Layout Lines`, `2. Catalog`, `3. Style & AGY`); `↑↓←→` / mouse wheel navigates.
- **Move Mode**: Press `Enter` (or click a selected pill) to grab (`⇄`), move within or across `Line 1–3` with arrow keys, and press `Enter` to drop. Right-click a pill to remove it.
- **Multi-Group Alignment**: Toggle `spacer` per line (or press `+` on `spacer` in `Catalog` for two spacers) to align `Left | Center | Right`.
- **Live Preview**: Click the preview panel or press `w` to cycle `Auto` / `120col` / `72col`.
- **Presets & Exit**: `1`–`4` apply presets; `s` saves `config.json` and updates AGY `settings.json`; `S` saves `config.json` only; `q` / `Esc` quits without saving.

### Interactive Stats TUI (`agy-statusline stats`)

- **Metrics & Daily Cache Rate**: Press `1`–`5` (or `[` / `]`) to switch presets (`Total`, `Cost ($)`, `In+Out`, `Output`, `Turns`). Both `1. Daily Usage` (with window total in the footer) and `3. Models` display color-coded `Cache%` (green `>=80%`, yellow `50%–79%`, red `<50%`). The last selected metric is saved to `config.json` (`stats_metric`) immediately on change.
- **Range, Sorting & Filtering**: `t` / `T` cycles time ranges (`7d` / `14d` / `30d` / `90d` / `All`); `s` cycles daily sort (`Date ↓`, `Date ↑`, `Value ↓`, `Cache% ↑`); `z` toggles zero-usage calendar gaps; `f` filters `All` / `CLI` / `Desktop`; `g` toggles the 20-week activity heatmap.
- **Drill-Down, Mouse, Conversations & Refresh**: `↑↓` / `j` / `k` / `PgUp` / `PgDn` / `Home` / `End` or mouse wheel/click navigates daily rows or models; `Tab` switches between `1. Daily Usage` and `3. Models`; `v` (or clicking the `3. Models` title bar) toggles `Selected Day` vs `Window`; `Enter` (or clicking the selected model again) cross-filters the dashboard (`Esc` clears the filter); `c` switches to the full-screen **Conversations** browser (`/` or click search bar for fuzzy search with cursor editing, wheel/click to select, click selected row or `Enter` to inspect, `b` / `Esc` returns); `r` refreshes local SQLite data; `q` quits.

### Interactive Conversation TUI (`agy-statusline stats <CONVERSATION_ID>` / `conversations`)

- **Turn Inspection, Clickable Sorting & Live Watch**: `↑↓` / `j` / `k` / `PgUp` / `PgDn` / `g` / `G` (`Home` / `End`) or mouse wheel/click browses every turn with live `2. Turn Detail` (timestamp, model/subagent, cost, tok/s, TTFT, prompt context, and token composition bar). Defaults to turn ascending (`# ↑`); click any column header (`#`, `Time`, `Model`, `Ctx`, `Input`, `Cache`, `Out`, `Hit%`, `tok/s`, `TTFT`, `Cost`) to sort by that field or toggle `↑`/`↓` (`s` cycles columns, `S` flips direction). Both the conversation list and detail view watch local SQLite `.db` / `-wal` metadata (`● Live`) for automatic updates (following the tail in `# ↑` mode while pinning mid-list inspection), and remember the last selected sort order across sessions and restarts in `config.json` (`conv_turn_sort`, `conv_list_sort`).
- **Switch Conversations, Anomaly & Subagent Filtering**: Press `c` to switch to the full-screen **Conversations** browser (or launch `agy-statusline conversations` / `conv` / `sessions` directly) with fuzzy relevance ranking (`/` live search, click any column header or press `s` / `S` to sort, `a` include subagents, `f` source filter, `r` reload list, `Enter` open); `m` filters to cache anomalies (`Cache% < 50%`); `Tab` switches between `1. Turns` and `3. Models / Subagents`; `v` (or clicking the pane title bar) toggles `Models` vs `Subagents`; `Enter` (or clicking the selected row again) cross-filters turns (`Esc` clears active filters first, then returns/quits); `r` reloads live SQLite turns; `q` quits.

## Presets & Defaults

- **`full`** *(default, replaces AGY's built-in bar)*: 2-row `Plain` layout (`Default` theme)
  - **Row 1**: `state`, `mode`, `model`, `directory`, `vcs`, `worktree`, `queue`, `vim`, `battle`, `version`
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
| `cost` | 3 | Session cost | Prefers payload `cost.total_usd` (with `(sub $...)` when `cost.subagent_usd >= 0.01`); otherwise sums native turn costs or cached `models.dev` rates across the active conversation DB and any descendant subagent DBs linked in `conversation_summaries.db`, rendering `(sub $...)` when subagent spend is `>= $0.01`, and `(N unpriced)` / `(sub $..., N unpriced)` for partial coverage |
| `mode` | 3 | Cycle mode | `cycle_mode` uppercased (`PLAN` / `AUTO` / `EDIT`, etc.), plus `>200K` badge when `exceeds_200k_tokens` is true |
| `quota_5h` | 4 | 5h quota + pace | Canonical JSON ID `"quota_5h"`; selects short-window (`<24h`) non-`disabled` bucket from `quota` via model token-affinity scoring; reads `remaining_fraction` (sub-1% precision: `<0.1%`, `0.1%`–`0.9%`) or `remaining_amount`, countdown from `reset_in_seconds` (or RFC3339 `reset_time` fallback), and burn-rate pace warning; hidden when absent if `smart_hide` is enabled |
| `quota_weekly` | 3 | Weekly quota | Selects multi-day (`>=24h`) non-`disabled` bucket from `quota` via token-affinity scoring; dynamic qualifier/window labeling, `remaining_fraction` or `remaining_amount`, countdown timer, and burn-rate pace warning; hidden when absent if `smart_hide` is enabled |
| `artifacts` | 1 | Artifact count | `artifact_count` rendered as `art <n>`; hidden when `0` if `smart_hide` is enabled |
| `subagents` | 1 | Subagent count | Counts entries in `subagents` array where `status != "completed"`; when exactly 1 active subagent has a `role` or `name`, renders `sub 1 (<label>)` (capped at 20 cols); hidden when `0` if `smart_hide` is enabled |
| `tasks` | 1 | Task count | `task_count` rendered as `tasks <n>`; hidden when `0` if `smart_hide` is enabled |
| `queue` | 2 | Queued inputs | `pending_input_count` rendered as `queue <n>`; hidden when `0` if `smart_hide` is enabled |
| `vim` | 2 | Vim mode | `vim.mode` uppercased as `vim <MODE>` (`NORMAL`, `INSERT`, `VISUAL`, `COMMAND`, etc.); hidden when absent if `smart_hide` is enabled |
| `battle` | 1 | Battle mode | `battle.status` uppercased as `battle <STATUS>` (with optional `#<battle.focused_arm>`); hidden when absent if `smart_hide` is enabled |
| `sandbox` | 1 | Sandbox badge | `sandbox.enabled` + `sandbox.allow_network`: `sandbox ON+net` when network is allowed, `sandbox ON` otherwise; hidden when `off` if `smart_hide` is enabled |
| `version` | 1 | AGY version | `version` trimmed and prefixed with `v` unless already prefixed (`v1.5.16`); omitted when absent or empty |
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
- **Responsive Compaction**: When `responsive` is enabled and a row overflows terminal width, segments compact progressively before dropping lower-priority widgets: `vcs` diff strip -> `vcs` 20-col cap -> `subagents` label strip -> `cost` suffix strip (`(sub ...)`, `(N unpriced)`) -> `quota_*` reset timer strip -> `ctx_*` limit strip -> `ctx_bar` collapse to `%` -> `model` leading-word abbreviation (including parenthesized names such as `Orchestrator (Carbon) · high` -> `O (Carbon) · high`) -> `model` 20-col cap -> `account` `@domain` strip -> `title` 16-col cap.
- **SQLite Sniffing & Direct Stats Scanning**: Statusline queries `$ANTIGRAVITY_CLI_HOME/conversations/<conversation_id>.db` (defaulting to `<home>/.gemini/antigravity-cli/conversations/<conversation_id>.db`) in read-only mode with a 25ms `busy_timeout` (`gen_metadata` `ORDER BY idx DESC LIMIT 9`) to extract `max_context_tokens` (`1 -> 9 -> 10 -> 4`), `estimated_tokens_used` (`1 -> 9 -> 10 -> 1`), output token decode speed (5-turn weighted average and up to 9-turn median, `>= 0.01s` and `< 10,000 tok/s`), thinking output tokens (`1 -> 4 -> 9`), and TTFT (`1 -> 11`). `agy-statusline stats` directly scans CLI and desktop `<home>/.gemini/antigravity` SQLite `conversations/*.db` files in parallel (the single authoritative source — no separate archive DB or JSONL transcript fallback), decoding `gen_metadata`, `steps.metadata`, and `trajectory_metadata_blob` with transitive Union-Find turn deduplication.
- **Cache Files (`$XDG_CACHE_HOME/agy-statusline/`)**: Defaults to `<home>/.cache/agy-statusline/` (using `HOME` or `USERPROFILE`):
  - `models_dev_pricing.json` / `models_dev_pricing.err` — 24h TTL auto-fetched `models.dev` pricing cache and fetch-failure backoff marker.
  - `conv_cost_cache.json` — bounded LRU per-conversation cost cache invalidated by DB/WAL/pricing file fingerprints.
  - `diff_*.txt` — short-TTL per-repository `git diff --numstat` cache files.
