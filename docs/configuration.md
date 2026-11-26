# Configuration & Reference

Requires **Antigravity CLI `>= 1.5.15`** (and **Antigravity Desktop `>= 2.35.0`** to aggregate desktop `~/.gemini/antigravity` sessions).

**Sections:** [CLI Commands](#cli-commands) · [Presets & Settings](#presets--settings) · [Widgets (33)](#widgets-33) · [Styles, Themes & Compaction](#styles-themes--compaction) · [Interactive TUI Controls](#interactive-tui-controls) · [Pricing, Storage & Cache Files](#pricing-storage--cache-files)

## Environment Variables

| Variable | Purpose | Default |
| :--- | :--- | :--- |
| `XDG_CONFIG_HOME` | Root for `agy-statusline/config.json` | `<home>/.config` |
| `XDG_CACHE_HOME` | Root for `agy-statusline/` cache files | `<home>/.cache` |
| `ANTIGRAVITY_CLI_HOME` | AGY CLI data & `settings.json` directory | `<home>/.gemini/antigravity-cli` |
| `ANTIGRAVITY_DATA_DIR` | Optional comma-separated extra AGY roots scanned by `stats`, `conversations` & `doctor` (also probes `<dir>/daemon` for Hub RPC) | *(unset)* |
| `NO_COLOR` | Disables ANSI color escapes when non-empty | *(unset)* |

`<home>` resolves from `HOME` (or `USERPROFILE` on Windows).

## CLI Commands

| Command | Flags & Notes |
| :--- | :--- |
| `agy-statusline` | Render statusline from `stdin` JSON (prints help on an empty TTY). Always exits `0` on `stdin` runs to avoid tripping AGY's circuit breaker. Flags: `--preset <full\|powerline\|minimal\|addon>` or `--full`. |
| `agy-statusline config` | Launch 3-pane interactive TUI editor. Non-interactive flags (mutually exclusive): `--show`, `--reset`, `--preset <NAME>`. |
| `agy-statusline apply` | Register `statusLine` in `<agy_cli_dir>/settings.json`, preserving existing keys (e.g. `padding`), Unix file mode, and invoked launcher symlink. Flags: `-y`/`--yes`, `--stack`/`--replace`, or `--disable` (sets `enabled = false`). |
| `agy-statusline doctor` | Inspect `config.json`, `NO_COLOR`, `agy --version`, `git`/`curl`, AGY hook wiring, all AGY roots' `conversations/*.db` counts, pricing cache, live Hub RPC / cached `quota_snapshot.json` pools & AI credits, and render a live sample preview. |
| `agy-statusline stats [<CONVERSATION_ID>]` | Local CLI + Desktop usage dashboard in host local timezone (interactive TUI on a TTY). Flags: `--days <N>` (`>= 1`, global only), `--metric <total\|cost\|io\|output\|turns>` (non-JSON), `--all` (all turns statically for a single conversation), `--tui\|--plain\|--json`, `--ascii`. `<CONVERSATION_ID>` accepts a full UUID or unique `>=4`-char hex prefix. |
| `agy-statusline conversations [QUERY]` | *(Aliases: `conv`, `sessions`)* Full-screen fuzzy conversation browser (interactive TUI on a TTY). Flags: `--subagents`, `--limit <N>` (`>= 1`, non-TUI), `--tui\|--plain\|--json`, `--ascii`. |
| `agy-statusline pricing [status\|sync]` | Show pricing cache status (`status`, default) or refresh from `models.dev` (`sync`, or `sync --if-stale` to refresh only when missing/expired past 24h TTL). |
| `agy-statusline version` | Print `agy-statusline <version>` (also `--version` / `-V`). |

## Presets & Settings

### Built-in Presets

| Preset | Style / Theme | Stack | Rows |
| :--- | :--- | :---: | :--- |
| `full` *(default)* | `Plain` / `Default` | Replace | **R1:** `state`, `mode`, `model`, `directory`, `vcs`, `worktree`, `queue`, `vim`, `battle`, `version`<br>**R2:** `ctx_bar`, `cache_hit`, `speed`, `cost`, `quota_5h`, `quota_weekly`, `subagents` |
| `powerline` | `Powerline` / `Catppuccin` | Replace | **R1:** `state`, `mode`, `model`, `directory`, `vcs`<br>**R2:** `ctx_bar`, `cache_hit`, `speed`, `cost`, `quota_5h` |
| `minimal` | `Plain` / `Default` | Replace | **R1:** `state`, `model`, `vcs`, `ctx_pct`, `cache_hit`, `speed` |
| `addon` | `Pill` (`Space`) / `Default` | Stack | **R1:** `cache_hit`, `cache_read`, `speed`, `cost` |

### `config.json` Options

| Key | Default | Values / Range |
| :--- | :--- | :--- |
| `rows` | *(preset)* | Up to 3 rows (`MAX_ROWS = 3`) of widget IDs |
| `style` | `"plain"` | `plain`, `pill`, `powerline` |
| `theme` | `"default"` | `default`, `nord`, `catppuccin`, `tokyo_night`, `gruvbox`, `light`, `mono` |
| `separator` | `"dot"` | `dot` (`·`), `pipe` (`│`), `slash` (`╱`), `space` (used in `Plain`, `Mono`, `NO_COLOR`) |
| `glyph_mode` | `"unicode"` | `unicode`, `nerd`, `ascii` |
| `speed_mode` | `"weighted"` | `weighted` (5-turn token-weighted), `median` (up to 9 turns), `instant`, `e2e` (+TTFT) |
| `bar_length` | `8` | `4..=40` columns (`ctx_bar`) |
| `speed_warn_threshold` | `35.0` | `0.0..=200.0` tok/s (TUI step `5`; `0` = `OFF`) |
| `ctx_remaining` | `false` | Show remaining `%` (`rem`) instead of used `%` (`ctx`) |
| `smart_hide` | `true` | Hide zero/absent/inactive optional widgets |
| `show_git_diff` | `true` | Show cached `+ins -del` diff counts on `vcs` |
| `responsive` | `true` | Compact and priority-drop segments to fit terminal width |
| `stack_with_default` | `false` | Stack below AGY's built-in statusline (`true`) or replace it (`false`) |
| `stats_metric`, `conv_turn_sort`, `conv_list_sort` | *(omitted)* | Persisted `stats` / `conversations` TUI preferences (preserved across preset changes) |

## Widgets (33)

`Prio` is the responsive drop priority (`5` = anchor kept last, `1` = auxiliary dropped first).

| ID | Prio | Description | Source & Behavior |
| :--- | :---: | :--- | :--- |
| `state` | 5 | Agent state | `tool_confirmation_pending` -> `CONFIRM`; else `agent_state` (`READY`, `THINKING`, `WORKING`, `TOOL`, `REVIEW`, `AUTH`, `INIT`, `DONE`, `ERROR`, or uppercased fallback) |
| `model` | 4 | Model + effort | `model.display_name` + `model.effort` (dedupes trailing effort suffixes and `x`/`extra` equivalence) |
| `directory` | 2 | Workspace dir | Basename of `workspace.project_dir` -> `workspace.current_dir` -> `cwd` |
| `vcs` | 3 | VCS branch + diff | `vcs.branch` -> `vcs.client` -> `.git/HEAD`; dirty `*`; optional cached `+ins -del` when `show_git_diff` is on |
| `worktree` | 2 | Git worktree | `.git` `gitdir:` pointer -> `wt:<name>` (`wt:main` when `smart_hide: false`) |
| `ctx_bar` | 4 | Context bar | Progress bar + `%` + `(current/limit)`; prefers SQLite `max_context_tokens` over payload size; bar clamps `0..=100%`, `%` shows overflow |
| `ctx_pct` | 4 | Context % | Same context calculation without the bar (`ctx` or `rem`) |
| `ctx_tokens` | 2 | Session tokens | `current/limit` or cumulative `total_input_tokens + total_output_tokens` (`K`/`M`/`B`) |
| `cache_hit` | 3 | Cache hit % | `cache_read / (input + cache_read + cache_creation)`; warns `<50%` / alerts `<20%` when context `>= 10K` |
| `cache_read` | 1 | Cache read tokens | `current_usage.cache_read_input_tokens` (`K`/`M`/`B`) |
| `speed` | 3 | Output speed | SQLite `gen_metadata` (last 9 non-fork turns, `>= 0.01s`, `< 10,000 tok/s`) + recent retry badge (`! 429`, `! 503`, etc.) |
| `ttft` | 1 | First token latency | Latest turn TTFT from SQLite `gen_metadata`; hidden when absent if `smart_hide` |
| `thinking` | 1 | Thinking tokens | Latest turn thinking tokens from SQLite `gen_metadata`; hidden when absent/zero if `smart_hide` |
| `cost` | 3 | Session cost | Payload `cost.total_usd`, else native/catalog estimate across active DB + descendant subagents; appends `(sub $...)` and `(N unpriced)` |
| `mode` | 3 | Cycle mode | Uppercased `cycle_mode` (`PLAN`/`AUTO`/`EDIT`), plus `>200K` badge when `exceeds_200k_tokens` |
| `quota_5h` | 4 | 5h quota + pace | Short-window (`<24h`) bucket from payload or `<=6h` `quota_snapshot.json`; model token-affinity matching, Proto3 `0%` recovery, burn-rate ETA (`!<eta>`/`!`), low/critical AI credits (`<=50` warn, `<=0` critical) |
| `quota_weekly` | 3 | Weekly quota | Multi-day (`>=24h`) bucket with token-affinity matching, burn-rate ETA, and credit fallback |
| `artifacts` | 1 | Artifact count | `art <n>`; hidden at `0` if `smart_hide` |
| `subagents` | 1 | Active subagents | Non-completed `subagents` count (`sub <n>`, plus single active `(<label>)` up to 20 cols); hidden at `0` if `smart_hide` |
| `tasks` | 1 | Task count | `tasks <n>`; hidden at `0` if `smart_hide` |
| `queue` | 2 | Queued inputs | `queue <n>`; hidden at `0` if `smart_hide` |
| `vim` | 2 | Vim mode | `vim <MODE>`; hidden when absent if `smart_hide` |
| `battle` | 1 | Battle mode | `battle <STATUS>` (plus optional `#<focused_arm>`); hidden when absent if `smart_hide` |
| `sandbox` | 1 | Sandbox status | `sandbox ON+net` / `sandbox ON` / `sandbox off`; hidden when off if `smart_hide` |
| `version` | 1 | AGY version | `v<version>`; omitted when absent |
| `plan` | 1 | Plan tier | `plan_tier` enriched with cached Hub RPC Ultra multiplier (`Ultra 5x` / `Ultra 20x`) |
| `account` | 1 | Account email | `email` (compacts to local-part on narrow rows) |
| `title` | 2 | Conversation title | `conversation_title` capped to 32 cols (16 cols under compaction) |
| `agent` | 2 | Active agent | `agent <name>` |
| `product` | 1 | Product name | `product` (`Antigravity`) |
| `session` | 1 | Session ID | `sid:<first-8-chars>` |
| `transcript` | 1 | Transcript file | `log:<filename>` |
| `spacer` | 5 | Flex spacer | Layout primitive; 1 spacer aligns `Left \| Right`, 2 spacers align `Left \| Center \| Right` |

## Styles, Themes & Compaction

- **Styles**: `Plain`, `Pill` (256-color capsules; rounded caps in `Nerd` mode), `Powerline` (`` / `>` segments).
- **Themes**: `Default`, `Nord`, `Catppuccin`, `Tokyo Night`, `Gruvbox`, `Light`, `Mono`.
- **Glyph Modes**: `Unicode`, `Nerd Font`, `ASCII`.
- **Responsive Compaction Order** (applied before dropping lowest-priority widgets):
  1. `vcs` diff strip -> 2. `vcs` 20-col cap -> 3. `subagents` label strip -> 4. `cost` suffix strip -> 5. `speed` retry strip -> 6. `quota_*` reset/ETA/credit suffix strip -> 7. `ctx_*` limit strip -> 8. `ctx_bar` collapse to `%` -> 9. `model` leading-word abbreviation -> 10. `model` 20-col cap -> 11. `account` `@domain` strip -> 12. `title` 16-col cap.

## Interactive TUI Controls

### Layout Configurator (`agy-statusline config`)

| Action | Keyboard | Mouse |
| :--- | :--- | :--- |
| Switch panes / navigate | `Tab`, `Shift+Tab`, `↑↓←→` / `hjkl` | Left-click pane, scroll wheel |
| Move Mode (Canvas) | `Enter` / `m` grab (`⇄`), arrows/`hjkl` move across Line 1–3, `Enter` / `Esc` drop (`[` / `]` swaps directly) | Click selected pill to grab, click target pill/line to move, click grabbed pill or `Enter/Esc Drop` to drop |
| Add / remove widget | `Enter` / `Space` in Catalog (`+` adds 2nd `spacer`); `Space` / `Backspace` / `Delete` / `x` in Canvas | Left-click Catalog row; right-click Canvas pill to remove |
| Adjust setting | `←→` / `hl` / `Space` / `Enter` in Settings | Left-click row (`+1`), click `<` or right-click row (`-1`) |
| Presets, preview & exit | `1`–`4` presets, `w` preview width (`Auto`/`120col`/`72col`), `s` save+apply, `S`/`Ctrl+S` save only, `q`/`Esc`/`Ctrl+C` quit | Click preview box or footer actions |

### Stats Dashboard (`agy-statusline stats`)

| Action | Keyboard | Mouse |
| :--- | :--- | :--- |
| Switch metric (persisted) | `1`–`5`, `[` / `]` (`Total`, `Cost ($)`, `In+Out`, `Output`, `Turns`) | Click top-bar metric pills |
| Range, sort, gaps, source, heatmap | `t`/`T` range, `s` sort (`Date ↓/↑`, `Value ↓`, `Cache% ↑`), `z` zero-day gaps, `f` source (`All`/`CLI`/`Desk`), `g` 20w heatmap | Click range pills (right-click compact range = prev), click column headers / `1. Daily Usage` title, click `Src:...`, click any heatmap cell to jump |
| Navigate & cross-filter | `↑↓`/`jk`/`PgUp`/`PgDn`/`Home`/`End`, `Tab` pane, `v` (`Day` vs `Window`), `Enter` filter model, `Esc` clear/quit | Wheel/click rows, click `3. Models` title, re-click selected model to filter, click `× <model>` pill to clear |
| Conversations, refresh & exit | `c` Conversations, `r` refresh, `q`/`Ctrl+C` quit | Click footer actions |

### Conversations Browser & Detail (`conversations` / `stats <ID>`)

Both screens watch local SQLite `.db` / `-wal` metadata (`● Live`) and persist sort orders (`conv_list_sort`, `conv_turn_sort`).

| Screen | Action | Keyboard / Mouse |
| :--- | :--- | :--- |
| **Conversations** | Search & filter | `/` or click search bar (`Ctrl+N/P`/`↑↓` select while typing, `Enter` open, `Esc` exit search), click `(Esc clear)`, `a` subagents, `f` / click `[Src:...]` source |
| **Conversations** | Sort, open & return | `s`/`S` or click any column header (`Modified`, `ID`, `Title`, `Workspace`, `Steps`, `Sub`, `Src`); `Enter` or re-click selected row to inspect; `r` reload; `b`/`Esc` back; `q` quit |
| **Detail** | Sort & inspect turns | `↑↓`/`jk`/`PgUp`/`PgDn`/`g`/`G` or wheel/click; `s`/`S` or click any column header (`#`, `Time`, `Model`, `Ctx`, `Input`, `Cache`, `Out`, `Hit%`, `tok/s`, `TTFT`, `Cost`) or header sort pill (right-click cycles field); auto-follows tail in `# ↑` mode |
| **Detail** | Filter & navigate | `m` or click `N miss` KPI badge / `Misses` pill; `Tab` switch to `3. Models / Subagents`; `v` or click title to toggle; `Enter` or re-click row to cross-filter; `Esc` or click `×` pills to clear; `c` Conversations; `r` refresh; `b` back; `q` quit |

## Pricing, Storage & Cache Files

- **Pricing Resolution**: Native AGY turn costs take precedence; otherwise estimates use the 24h `models.dev` catalog (`stats` refreshes stale rates before reporting; statusline refreshes in the background). Lookup order: case-insensitive exact match (ID before display name, canonical provider over reseller punctuation) -> normalized punctuation/spacing -> longest substring -> dynamic `orchestrator` codename matching -> unambiguous `response_model` inheritance in `stats`. Context-length tiers (`tiers` / `context_over_200k`) are applied when prompt tokens exceed the threshold.
- **Read-Only SQLite & Fork Deduplication**: Statusline reads `$ANTIGRAVITY_CLI_HOME/conversations/<id>.db` read-only (zero-lock 100-byte header readiness check, 25ms `busy_timeout`), skipping foreign forked rows (`origin_cascade_id != conversation_id`). `stats` scans CLI and Desktop `conversations/*.db` in parallel with transitive Union-Find turn deduplication, decoding `gen_metadata`, `steps.metadata`, `steps.step_type` (`17` errors, `23` compactions), and `trajectory_metadata_blob`.
- **Quota & Loopback Hub RPC**: Statusline hot path performs zero network I/O and persists stdin quota updates to `quota_snapshot.json` (with a 16-point per-bucket burn-rate ring buffer). `stats` and `doctor` opportunistically probe active local `daemon/hub_*.json` processes over `127.0.0.1` (`10s` fresh RPC TTL) for multi-pool quota, `GOOGLE_ONE_AI` credits, and Ultra `5x`/`20x` tier labels.

### XDG Files

| Path | Role |
| :--- | :--- |
| `$XDG_CONFIG_HOME/agy-statusline/config.json` | Layout rows, style/theme settings, and persisted `stats`/`conversations` TUI preferences |
| `$XDG_CACHE_HOME/agy-statusline/models_dev_pricing.json` | 24h TTL cached `models.dev` rate catalog (`.err` marker records fetch failure backoff) |
| `$XDG_CACHE_HOME/agy-statusline/stats_scan_cache.json` | Persistent per-DB unpriced SQLite scan cache for global `stats` (invalidated by DB/WAL fingerprints) |
| `$XDG_CACHE_HOME/agy-statusline/quota_snapshot.json` | Atomic multi-pool quota, AI credits, Ultra tier, and burn-rate history snapshot |
| `$XDG_CACHE_HOME/agy-statusline/conv_cost_cache.json` | Bounded LRU per-conversation cost cache for the statusline `cost` widget |
| `$XDG_CACHE_HOME/agy-statusline/diff_*.txt` | Short-TTL per-repository `git diff --numstat` cache |
