//! Serializable data models for `agy-statusline stats` and `agy-statusline conversations`.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::scan::ScanDiagnostics;

/// Serializable window summary for a single quota pool in `stats --json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindowReport {
    /// Window label (e.g. `"5h"`, `"7d"`, `"3p 5h"`).
    pub label: String,
    /// Remaining quota percentage (`0.0..=100.0`).
    pub remaining_pct: f64,
    /// Remaining discrete credit amount when fraction is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_amount: Option<i64>,
    /// Compact reset countdown (e.g. `"3h10m"`), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_compact: Option<String>,
    /// Whether remaining quota is lagging behind remaining window time.
    #[serde(default)]
    pub pace_behind: bool,
    /// Observed consumption rate in percentage points per hour (`%/hr`), if history exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_rate_pct_per_hr: Option<f64>,
    /// Projected seconds until exhaustion at current burn rate, if history exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    /// Whether burn rate projects exhaustion before reset (falling back to `pace_behind`).
    #[serde(default)]
    pub will_exhaust_before_reset: bool,
}

/// Serializable multi-pool quota entry (`Gemini`, `Argon`, `Carbon`, `Claude/3P`, etc.).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaPoolReport {
    /// Display name of the pool (`"Gemini"`, `"Argon"`, `"Carbon"`, `"Claude/3P"`, etc.).
    pub name: String,
    /// Short-window (`< 24h`) bucket summary, if active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_window: Option<QuotaWindowReport>,
    /// Multi-day (`>= 24h`) bucket summary, if active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_window: Option<QuotaWindowReport>,
}

/// Serializable non-windowed credit balance entry (`AI credits`, `Prompt credits`, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaCreditReport {
    /// Canonical bucket key.
    pub key: String,
    /// Display credit label.
    pub label: String,
    /// Remaining credit units.
    pub remaining_amount: i64,
    /// Compact reset countdown, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_compact: Option<String>,
    /// Warning severity (`"ok"`, `"low"`, or `"critical"`).
    pub severity: String,
}

/// Live or cached multi-pool quota and AI credits summary exposed on [`StatsReport`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsQuotaReport {
    /// Data origin (`"hub_rpc"` or `"stdin"`).
    pub source: String,
    /// `true` when returned from `quota_snapshot.json` rather than a fresh live Hub RPC call.
    pub from_cache: bool,
    /// Unix epoch timestamp in seconds when the snapshot was last updated.
    pub updated_at: u64,
    /// Snapshot age in seconds relative to `now_unix_secs`.
    pub age_secs: u64,
    /// Resolved account tier label (e.g. `"Ultra 20x"`, `"Ultra 5x"`, `"Pro"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_tier: Option<String>,
    /// Available `GOOGLE_ONE_AI` / AI credits balance, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_credits: Option<i64>,
    /// Grouped active quota pools (`Gemini`, `Argon`, `Carbon`, `Claude/3P`, etc.).
    #[serde(default)]
    pub pools: Vec<QuotaPoolReport>,
    /// Non-windowed credit buckets (`AI credits`, `Prompt credits`, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credits: Vec<QuotaCreditReport>,
}

impl StatsQuotaReport {
    /// Returns `true` when at least one pool, credit balance, or plan tier is available to display.
    pub fn has_visible_content(&self) -> bool {
        !self.pools.is_empty()
            || self.ai_credits.is_some()
            || !self.credits.is_empty()
            || self.plan_tier.is_some()
    }

    /// Credit buckets excluding `google_one_ai`/`ai_credits` when already surfaced via `ai_credits`.
    pub(super) fn extra_credits(&self) -> impl Iterator<Item = &QuotaCreditReport> {
        self.credits.iter().filter(|credit| {
            !(self.ai_credits.is_some()
                && (credit.key.eq_ignore_ascii_case("google_one_ai")
                    || credit.key.eq_ignore_ascii_case("ai_credits")))
        })
    }
}

/// Aggregated retry, error-step, and compaction diagnostics for a single conversation
/// and its descendant subagents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationDiagnostics {
    /// Combined diagnostics across the main conversation and descendant subagents.
    #[serde(default)]
    pub total: ScanDiagnostics,
    /// Diagnostics attributed strictly to the target conversation.
    #[serde(default)]
    pub main: ScanDiagnostics,
    /// Combined diagnostics attributed to descendant subagents.
    #[serde(default)]
    pub subagents: ScanDiagnostics,
}

impl ConversationDiagnostics {
    /// Returns `true` when neither main nor subagent conversations recorded any anomalies.
    pub fn is_empty(&self) -> bool {
        self.total.is_empty()
    }
}

/// Computes a chronological boolean mask aligned with `turns` marking each mid-session cache miss
/// (a turn with prompt tokens and `cache_read_tokens == 0` after an earlier prompt turn in the
/// same `conversation_id`).
pub(in crate::stats) fn mid_session_miss_flags(turns: &[ConversationTurnStats]) -> Vec<bool> {
    let mut seen_prompt_convs: HashSet<&str> = HashSet::new();
    turns
        .iter()
        .map(|t| {
            let prompt_tokens = t
                .input_tokens
                .saturating_add(t.cache_read_tokens)
                .saturating_add(t.cache_write_tokens);
            if prompt_tokens == 0 {
                return false;
            }
            let had_earlier_prompt = !seen_prompt_convs.insert(t.conversation_id.as_str());
            had_earlier_prompt && t.cache_read_tokens == 0
        })
        .collect()
}

/// Aggregated token and optional cost totals for a time window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeriodSummary {
    /// Total deduplicated turns/requests with non-zero token usage.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Prompt cache read tokens.
    pub cache_read_tokens: u64,
    /// Prompt cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens (includes thinking tokens when reported by AGY).
    pub output_tokens: u64,
    /// Thinking/reasoning output tokens (subset of `output_tokens`).
    pub thinking_tokens: u64,
    /// Total tokens (`input_tokens + cache_read_tokens + cache_write_tokens + output_tokens`).
    pub total_tokens: u64,
    /// Estimated cost in USD across priced turns (from native `model_cost` or `models.dev` rates).
    pub priced_usd: f64,
    /// Count of turns with a price entry or native cost.
    pub priced_requests: u64,
    /// Count of turns without a price entry or native cost.
    pub unpriced_requests: u64,
}

/// Standard fixed-window summaries (`today`, `last_7d`, `last_30d`, `all_time`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSummaries {
    /// Local today (`now_unix_secs` date in the active timezone).
    pub today: PeriodSummary,
    /// Last 7 local calendar days including today.
    pub last_7d: PeriodSummary,
    /// Last 30 local calendar days including today.
    pub last_30d: PeriodSummary,
    /// All recorded turns regardless of date.
    pub all_time: PeriodSummary,
}

/// Daily local-calendar token and optional cost aggregate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyUsage {
    /// Local calendar date in `YYYY-MM-DD` format.
    pub date: String,
    /// Turn/request count on this date.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + cache_write + output`).
    pub total_tokens: u64,
    /// Estimated cost in USD for priced turns.
    pub priced_usd: f64,
    /// Count of unpriced turns on this date.
    pub unpriced_requests: u64,
}

/// Monday-anchored local calendar week aggregate for the 20-week activity tile grid.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeeklyUsage {
    /// Monday start date in `YYYY-MM-DD` format.
    pub week_start: String,
    /// Sunday end date in `YYYY-MM-DD` format.
    pub week_end: String,
    /// Per-day token totals from Monday (`0`) through Sunday (`6`), `None` for future days after `today_local`.
    pub daily_tokens: [Option<u64>; 7],
    /// Total tokens across non-future days in this week.
    pub total_tokens: u64,
}

/// Per-model usage, optional cost, and speed/TTFT leaderboard entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelStats {
    /// Display model name derived dynamically from AGY metadata.
    pub model: String,
    /// Matched catalog model key (or `"native"`) when priced.
    pub pricing_key: Option<String>,
    /// Total turns attributed to this model.
    pub requests: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache read tokens.
    pub cache_read_tokens: u64,
    /// Cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Thinking tokens.
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + cache_write + output`).
    pub total_tokens: u64,
    /// Estimated cost in USD.
    pub priced_usd: f64,
    /// Count of unpriced turns in this model bucket.
    pub unpriced_requests: u64,
    /// Number of valid decode speed samples.
    pub speed_samples: usize,
    /// Median decode throughput in tokens/second, if speed samples exist.
    pub median_tok_per_sec: Option<f64>,
    /// Number of valid TTFT samples.
    pub ttft_samples: usize,
    /// Median time-to-first-token in seconds, if TTFT samples exist.
    pub median_ttft_secs: Option<f64>,
}

/// Aggregate speed and TTFT statistics from `conversations/*.db`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SqliteStats {
    /// Total valid UUID `.db` conversations scanned.
    pub db_files: u64,
    /// Completed turns decoded from `gen_metadata` passing speed sanity bounds.
    pub completed_turns: u64,
    /// Sum of output tokens across completed speed turns.
    pub total_output_tokens: u64,
    /// Median decode speed across completed turns (tokens/second).
    pub median_tok_per_sec: Option<f64>,
    /// Count of turns with valid TTFT (`chat.11`).
    pub ttft_samples: usize,
    /// Median time-to-first-token across decoded turns (seconds).
    pub median_ttft_secs: Option<f64>,
}

/// Complete usage report computed by `agy-statusline stats` and serialized by `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsReport {
    /// Reference local calendar date (`YYYY-MM-DD`) derived from `now_unix_secs` and `timezone`.
    pub today_local: String,
    /// Active timezone label (e.g., `"UTC+08:00"` or `"UTC"`).
    pub timezone: String,
    /// Reference UTC date (`YYYY-MM-DD`) derived from `now_unix_secs`.
    pub today_utc: String,
    /// Active `--days` filter (`None` for all-time).
    pub filter_days: Option<u32>,
    /// Earliest local calendar date observed across active usage events.
    pub first_active_date: Option<String>,
    /// Fixed comparison windows (`today`, `last_7d`, `last_30d`, `all_time`).
    pub windows: WindowSummaries,
    /// Active summary totals (filtered by `filter_days` when set).
    pub totals: PeriodSummary,
    /// Daily usage entries sorted chronologically (filtered by `filter_days` when set).
    pub daily: Vec<DailyUsage>,
    /// Unfiltered 20-week Monday-anchored calendar activity (oldest week first, ending with current week).
    pub weekly: Vec<WeeklyUsage>,
    /// Per-model breakdown sorted by `total_tokens` descending.
    pub models: Vec<ModelStats>,
    /// Aggregate statistics from `conversations/*.db` (`gen_metadata`).
    pub sqlite: SqliteStats,
    /// Optional live or cached multi-pool quota and AI credits summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<StatsQuotaReport>,
    /// Aggregated retry, error-step, and compaction-checkpoint diagnostics across scanned DBs.
    #[serde(default, skip_serializing_if = "ScanDiagnostics::is_empty")]
    pub diagnostics: ScanDiagnostics,
}

/// Per-turn usage, cache hit rate, speed, and cost entry for a single conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationTurnStats {
    /// 1-based chronological turn index across the conversation (and descendant subagents).
    pub index: usize,
    /// Owning conversation UUID (matches root or a descendant subagent UUID).
    pub conversation_id: String,
    /// Whether this turn came from a descendant subagent conversation.
    pub is_subagent: bool,
    /// Local timestamp (`YYYY-MM-DD HH:MM:SS`) when available.
    pub timestamp_local: Option<String>,
    /// Display model group name derived from AGY metadata.
    pub model: String,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Prompt cache read tokens.
    pub cache_read_tokens: u64,
    /// Prompt cache write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens (includes thinking tokens when reported by AGY).
    pub output_tokens: u64,
    /// Thinking/reasoning output tokens (subset of `output_tokens`).
    pub thinking_tokens: u64,
    /// Total tokens (`input + cache_read + cache_write + output`).
    pub total_tokens: u64,
    /// Prompt cache hit percentage (`0.0..=100.0`), or `None` when no prompt tokens exist.
    pub cache_hit_pct: Option<f64>,
    /// Estimated turn cost in USD (native or catalog rate), if priced.
    pub priced_usd: Option<f64>,
    /// Decode throughput in tokens/second, if valid.
    pub tok_per_sec: Option<f64>,
    /// Time-to-first-token in seconds, if valid.
    pub ttft_secs: Option<f64>,
}

/// Aggregated statistics for one descendant subagent conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentConversationStats {
    /// Canonical lowercase descendant conversation UUID.
    pub conversation_id: String,
    /// Subagent name from `conversation_summaries.db`, if recorded.
    pub agent_name: Option<String>,
    /// Conversation title from `conversation_summaries.db`, if recorded.
    pub title: Option<String>,
    /// Aggregated token, turn, and cost totals attributed to this subagent.
    pub summary: PeriodSummary,
    /// Median decode throughput in tokens/second across valid turns.
    pub median_tok_per_sec: Option<f64>,
    /// Median TTFT in seconds across valid turns.
    pub median_ttft_secs: Option<f64>,
}

/// Complete usage report for a single conversation (`agy-statusline stats <CONVERSATION_ID>`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConversationStatsReport {
    /// Canonical lowercase UUID of the inspected conversation.
    pub conversation_id: String,
    /// Conversation title from `conversation_summaries.db`, if recorded.
    pub title: Option<String>,
    /// Parent conversation UUID when inspecting a subagent conversation directly.
    pub parent_conversation_id: Option<String>,
    /// Agent name from `conversation_summaries.db`, if recorded.
    pub agent_name: Option<String>,
    /// Owning data root origin (`"cli"` or `"desktop"`).
    pub source: String,
    /// Active local timezone label (e.g. `"UTC+08:00"` or `"UTC"`).
    pub timezone: String,
    /// Earliest turn timestamp in local time (`YYYY-MM-DD HH:MM:SS`), if available.
    pub first_turn_local: Option<String>,
    /// Latest turn timestamp in local time (`YYYY-MM-DD HH:MM:SS`), if available.
    pub last_turn_local: Option<String>,
    /// Elapsed seconds between the first and last turn timestamps, if available.
    pub duration_secs: Option<u64>,
    /// Peak single-turn prompt context size (`input + cache_read + cache_write`), if any.
    pub peak_context_tokens: Option<u64>,
    /// Count of turns with zero cache read after the first prompt turn of the same conversation.
    pub mid_session_misses: u64,
    /// Estimated USD cost spent on `mid_session_misses` turns.
    pub mid_session_miss_usd: f64,
    /// Combined deduplicated totals across the main conversation and descendant subagents.
    pub totals: PeriodSummary,
    /// Totals attributed to the target conversation itself.
    pub main: PeriodSummary,
    /// Combined totals attributed to descendant subagents.
    pub subagents_summary: PeriodSummary,
    /// Median decode throughput for main conversation turns.
    pub main_median_tok_per_sec: Option<f64>,
    /// Median TTFT for main conversation turns.
    pub main_median_ttft_secs: Option<f64>,
    /// Median decode throughput across descendant subagent turns.
    pub subagents_median_tok_per_sec: Option<f64>,
    /// Median TTFT across descendant subagent turns.
    pub subagents_median_ttft_secs: Option<f64>,
    /// Per-model breakdown across the conversation (and descendant subagents).
    pub models: Vec<ModelStats>,
    /// Per-subagent breakdown for each descendant conversation database found.
    pub subagents: Vec<SubagentConversationStats>,
    /// Chronological per-turn timeline with per-turn cache hit rate, speed, TTFT, and cost.
    pub turns: Vec<ConversationTurnStats>,
    /// Aggregate SQLite speed/TTFT stats across scanned conversation DBs.
    pub sqlite: SqliteStats,
    /// Aggregated retry, error-step, and compaction diagnostics across main + subagents.
    #[serde(default, skip_serializing_if = "ConversationDiagnostics::is_empty")]
    pub diagnostics: ConversationDiagnostics,
}

/// Lightweight historical conversation metadata row (`agy-statusline conversations`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationListItem {
    /// Canonical lowercase conversation UUID.
    pub conversation_id: String,
    /// Owning data root origin (`"cli"` or `"desktop"`).
    pub source: String,
    /// Conversation title (or sanitized first-line preview fallback) from `conversation_summaries.db`.
    pub title: Option<String>,
    /// Basename of the primary workspace directory, if recorded.
    pub workspace: Option<String>,
    /// Agent name from `conversation_summaries.db`, if recorded.
    pub agent_name: Option<String>,
    /// Parent conversation UUID when this conversation is a descendant subagent.
    pub parent_conversation_id: Option<String>,
    /// Count of descendant subagent conversation databases linked to this conversation.
    pub subagent_count: usize,
    /// Recorded step count from `conversation_summaries.db`, if available.
    pub step_count: Option<u64>,
    /// Last modification Unix timestamp in seconds.
    pub last_modified_secs: i64,
    /// Last modification timestamp formatted in host local time (`YYYY-MM-DD HH:MM`).
    pub last_modified_local: String,
    /// Conversation SQLite file size in bytes.
    pub db_size_bytes: u64,
}
