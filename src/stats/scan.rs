//! Read-only SQLite `gen_metadata` and `steps.metadata` scanner.

mod conversation;
mod cost_cache;

pub(in crate::stats) use conversation::{
    collect_conversation_stats_report, filter_and_rank_conversations, list_conversations,
};
pub(crate) use cost_cache::{
    estimate_conversation_cost_usd_with_parent_conn, lookup_cached_conversation_cost,
};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::pricing::{normalize_model_key, round_usd_6, PricingCatalog};
use super::tz::TimeZoneSpec;
use super::SqliteStats;
use crate::agy::db::{
    compute_median, is_real_dir, is_safe_sqlite_file, is_valid_uuid, open_readonly_db,
    safe_sqlite_metadata,
};
use crate::agy::protobuf::{
    extract_gen_metadata_record, extract_step_metadata_record, extract_trajectory_created_at_secs,
    MAX_TTFT_SECS, MAX_TURN_SPEED_TPS,
};
use crate::sys::fs::modified_timestamp;

pub(super) fn is_effort_token(word: &str) -> bool {
    matches!(
        word,
        "xlow"
            | "extralow"
            | "low"
            | "medium"
            | "med"
            | "high"
            | "xhigh"
            | "extrahigh"
            | "extra"
            | "minimal"
            | "max"
            | "none"
            | "off"
    )
}

fn is_effort_qualifier(tag: &str) -> bool {
    let norm: String = tag
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    is_effort_token(&norm)
}

/// Derives a clean display model group name strictly from AGY metadata without hardcoded models.
///
/// Strips trailing parenthetical effort tiers (e.g. `(Low)`, `(Medium)`, `(High)`) while
/// preserving non-effort qualifiers (e.g. `(Orchestrator)` or tier/backend labels).
fn derive_model_name(
    model_display_name: Option<&str>,
    response_model: Option<&str>,
    model_enum: u64,
) -> String {
    if let Some(disp) = model_display_name.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(open_idx) = disp.rfind(" (") {
            if let Some(inner) = disp[open_idx + 2..].strip_suffix(')') {
                if is_effort_qualifier(inner) {
                    let base = disp[..open_idx].trim();
                    if !base.is_empty() {
                        return base.to_string();
                    }
                }
            }
        }
        return disp.to_string();
    }
    if let Some(resp) = response_model.map(str::trim).filter(|s| !s.is_empty()) {
        return resp.to_string();
    }
    if model_enum > 0 {
        return format!("model#{model_enum}");
    }
    "unknown".to_string()
}

/// Origin of a scanned usage event (`antigravity-cli` vs desktop `antigravity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    Cli,
    Desktop,
}

impl UsageSource {
    fn from_root_label(label: &str) -> Self {
        if label == "desktop" {
            Self::Desktop
        } else {
            Self::Cli
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct UsageEvent {
    pub(super) source: UsageSource,
    pub(super) local_day: i64,
    pub(super) local_date: String,
    pub(super) model_group: String,
    pub(super) pricing_key: Option<String>,
    pub(super) input_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) cache_write_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) thinking_tokens: u64,
    pub(super) total_tokens: u64,
    pub(super) cost_usd: Option<f64>,
    pub(super) decode_speed: Option<f64>,
    pub(super) ttft_secs: Option<f64>,
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn root_path_key(root: &Path) -> String {
    canonical_or_self(root).to_string_lossy().into_owned()
}

#[derive(Debug, Clone)]
struct DbTarget {
    source: &'static str,
    root: PathBuf,
    conversation_id: String,
    path: PathBuf,
    db_mtime_secs: Option<i64>,
    db_size_bytes: u64,
}

const TS_QUALITY_NONE: u8 = 0;
const TS_QUALITY_DB_MTIME: u8 = 1;
const TS_QUALITY_TRAJECTORY: u8 = 2;
const TS_QUALITY_SECONDARY: u8 = 3;
const TS_QUALITY_STEP_PRIMARY: u8 = 4;

/// Timezone- and pricing-independent turn record extracted from a SQLite conversation DB.
#[derive(Debug, Clone, PartialEq)]
struct RawTurn {
    ts_secs: Option<i64>,
    ts_quality: u8,
    model_enum: u64,
    response_model: Option<String>,
    model_display_name: Option<String>,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    thinking_tokens: u64,
    native_cost_usd: Option<f64>,
    decode_speed: Option<f64>,
    ttft_secs: Option<f64>,
    id_11: Option<String>,
    id_12: Option<String>,
    id_7: Option<String>,
}

impl RawTurn {
    fn has_billable_usage(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.cache_read_tokens > 0
            || self.cache_write_tokens > 0
            || self.native_cost_usd.is_some()
    }

    fn has_signal(&self) -> bool {
        self.decode_speed.is_some() || self.ttft_secs.is_some() || self.has_billable_usage()
    }

    fn has_any_identity(&self) -> bool {
        self.id_11.is_some() || self.id_12.is_some() || self.id_7.is_some()
    }

    fn identity_keys(&self) -> [(u8, Option<&str>); 3] {
        [
            (11, self.id_11.as_deref()),
            (12, self.id_12.as_deref()),
            (7, self.id_7.as_deref()),
        ]
    }
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            let p = self.parent[x];
            self.parent[x] = self.parent[p];
            x = p;
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            // Attach larger root to smaller root so component order stays deterministic.
            if ra < rb {
                self.parent[rb] = ra;
            } else {
                self.parent[ra] = rb;
            }
        }
    }
}

fn display_name_rank(disp: Option<&str>) -> u8 {
    match disp.map(str::trim).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) if PricingCatalog::has_specific_model_qualifier(None, Some(s)) => 2,
        Some(_) => 1,
    }
}

fn response_model_rank(resp: Option<&str>) -> u8 {
    match resp.map(str::trim).filter(|s| !s.is_empty()) {
        None => 0,
        Some(s) if PricingCatalog::has_specific_model_qualifier(Some(s), None) => 2,
        Some(_) => 1,
    }
}

fn valid_decode_speed(speed: Option<f64>) -> Option<f64> {
    speed.filter(|s| s.is_finite() && *s > 0.0 && *s < MAX_TURN_SPEED_TPS)
}

fn valid_ttft_secs(ttft: Option<f64>) -> Option<f64> {
    ttft.filter(|t| t.is_finite() && *t > 0.0 && *t < MAX_TTFT_SECS)
}

fn valid_native_cost(cost: Option<f64>) -> Option<f64> {
    cost.filter(|c| c.is_finite() && *c > 0.0)
}

fn merge_raw_turn(dst: &mut RawTurn, other: RawTurn) {
    dst.input_tokens = dst.input_tokens.max(other.input_tokens);
    dst.cache_read_tokens = dst.cache_read_tokens.max(other.cache_read_tokens);
    dst.cache_write_tokens = dst.cache_write_tokens.max(other.cache_write_tokens);
    dst.output_tokens = dst.output_tokens.max(other.output_tokens);
    dst.thinking_tokens = dst
        .output_tokens
        .min(dst.thinking_tokens.max(other.thinking_tokens));

    if response_model_rank(other.response_model.as_deref())
        > response_model_rank(dst.response_model.as_deref())
    {
        dst.response_model = other.response_model;
    }
    if display_name_rank(other.model_display_name.as_deref())
        > display_name_rank(dst.model_display_name.as_deref())
    {
        dst.model_display_name = other.model_display_name;
    }
    if dst.model_enum == 0 && other.model_enum != 0 {
        dst.model_enum = other.model_enum;
    }

    match (dst.ts_secs, other.ts_secs) {
        (None, Some(ts)) => {
            dst.ts_secs = Some(ts);
            dst.ts_quality = other.ts_quality;
        }
        (Some(dst_ts), Some(other_ts))
            if other.ts_quality > dst.ts_quality
                || (other.ts_quality == dst.ts_quality && other_ts < dst_ts) =>
        {
            dst.ts_secs = Some(other_ts);
            dst.ts_quality = other.ts_quality;
        }
        _ => {}
    }

    dst.native_cost_usd = match (
        valid_native_cost(dst.native_cost_usd),
        valid_native_cost(other.native_cost_usd),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    dst.decode_speed =
        valid_decode_speed(dst.decode_speed).or_else(|| valid_decode_speed(other.decode_speed));
    dst.ttft_secs = valid_ttft_secs(dst.ttft_secs).or_else(|| valid_ttft_secs(other.ttft_secs));

    if dst.id_11.is_none() {
        dst.id_11 = other.id_11;
    }
    if dst.id_12.is_none() {
        dst.id_12 = other.id_12;
    }
    if dst.id_7.is_none() {
        dst.id_7 = other.id_7;
    }
}

fn build_turn_union_find<'a>(
    turns: impl IntoIterator<Item = &'a RawTurn>,
    len: usize,
) -> UnionFind {
    let mut uf = UnionFind::new(len);
    let mut key_owner: HashMap<(u8, &'a str), usize> = HashMap::new();
    for (idx, turn) in turns.into_iter().enumerate() {
        for (tag, val_opt) in turn.identity_keys() {
            let Some(val) = val_opt.map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            if let Some(&prev) = key_owner.get(&(tag, val)) {
                uf.union(prev, idx);
            } else {
                key_owner.insert((tag, val), idx);
            }
        }
    }
    uf
}

fn deduplicate_raw_turns(turns: Vec<RawTurn>) -> Vec<RawTurn> {
    if turns.len() <= 1 {
        return turns;
    }
    let mut uf = build_turn_union_find(turns.iter(), turns.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut merged: Vec<RawTurn> = Vec::with_capacity(turns.len());
    for (idx, turn) in turns.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            merge_raw_turn(&mut merged[slot], turn);
        } else {
            root_to_slot.insert(root, merged.len());
            merged.push(turn);
        }
    }
    merged
}

fn discover_sqlite_db_targets(roots: &[(&'static str, PathBuf)]) -> Vec<DbTarget> {
    let mut seen_dbs = HashSet::new();
    let mut seen_ids = HashSet::new();
    let mut targets = Vec::new();

    for (source, root) in roots {
        let conv_dir = root.join("conversations");
        if !is_real_dir(&conv_dir) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&conv_dir) else {
            continue;
        };
        let mut sorted: Vec<_> = entries.filter_map(Result::ok).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let Some(stem) = name_str.strip_suffix(".db") else {
                continue;
            };
            if !is_valid_uuid(stem) {
                continue;
            }
            let path = entry.path();
            let Some(meta) = safe_sqlite_metadata(&path) else {
                continue;
            };
            let real = canonical_or_self(&path);
            let conversation_id = stem.to_ascii_lowercase();
            if !seen_dbs.insert(real) || !seen_ids.insert(conversation_id.clone()) {
                continue;
            }
            let db_mtime_secs = Some(modified_timestamp(&meta).0).filter(|&secs| secs > 0);
            targets.push(DbTarget {
                source,
                root: root.clone(),
                conversation_id,
                path,
                db_mtime_secs,
                db_size_bytes: meta.len(),
            });
        }
    }

    targets
}

fn fallback_timestamp(trajectory_ts: Option<i64>, db_mtime_secs: Option<i64>) -> (Option<i64>, u8) {
    if let Some(ts) = trajectory_ts {
        (Some(ts), TS_QUALITY_TRAJECTORY)
    } else if let Some(ts) = db_mtime_secs {
        (Some(ts), TS_QUALITY_DB_MTIME)
    } else {
        (None, TS_QUALITY_NONE)
    }
}

/// Extracts and intra-DB deduplicates all turns from one SQLite conversation DB across
/// `gen_metadata` (main + `f17` retries), `steps.metadata` (`f1`/`f8` timestamps, `f9` usage,
/// `f24` model info, `f28` retries), and `trajectory_metadata_blob` (`f2` fallback timestamp).
///
/// Returns `None` only when neither `gen_metadata` nor `steps` exists in the database.
fn extract_sqlite_db_turns(
    conn: &rusqlite::Connection,
    db_mtime_secs: Option<i64>,
) -> Option<Vec<RawTurn>> {
    let trajectory_ts = conn
        .prepare("SELECT data FROM trajectory_metadata_blob WHERE data IS NOT NULL LIMIT 1")
        .ok()
        .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, Vec<u8>>(0)).ok())
        .and_then(|blob| extract_trajectory_created_at_secs(&blob));

    let mut has_steps_table = false;
    let mut step_timestamps: HashMap<u64, (i64, u8)> = HashMap::new();
    let mut step_candidates: Vec<(u64, RawTurn)> = Vec::new();
    if let Ok(mut step_stmt) =
        conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx ASC")
    {
        if let Ok(mut step_rows) = step_stmt.query([]) {
            has_steps_table = true;
            while let Ok(Some(row)) = step_rows.next() {
                let Ok(idx) = row.get::<_, i64>(0) else {
                    continue;
                };
                let Ok(meta_blob) = row.get::<_, Vec<u8>>(1) else {
                    continue;
                };
                if idx < 0 {
                    continue;
                }
                let step_idx = idx as u64;
                let Some(step_rec) = extract_step_metadata_record(&meta_blob) else {
                    continue;
                };

                let step_ts = if let Some(ts) = step_rec.created_at_secs {
                    step_timestamps.insert(step_idx, (ts, TS_QUALITY_STEP_PRIMARY));
                    (Some(ts), TS_QUALITY_STEP_PRIMARY)
                } else if let Some(ts) = step_rec.secondary_created_at_secs {
                    step_timestamps.insert(step_idx, (ts, TS_QUALITY_SECONDARY));
                    (Some(ts), TS_QUALITY_SECONDARY)
                } else {
                    fallback_timestamp(trajectory_ts, db_mtime_secs)
                };

                if let Some(usage) = step_rec.usage {
                    let inherits_info = usage.model_enum == 0
                        || step_rec.model_enum == 0
                        || usage.model_enum == step_rec.model_enum;
                    let (response_model, model_display_name) = if inherits_info {
                        (
                            step_rec.response_model.clone(),
                            step_rec.model_display_name.clone(),
                        )
                    } else {
                        (None, None)
                    };
                    let model_enum = if usage.model_enum != 0 {
                        usage.model_enum
                    } else {
                        step_rec.model_enum
                    };
                    let turn = RawTurn {
                        ts_secs: step_ts.0,
                        ts_quality: step_ts.1,
                        model_enum,
                        response_model,
                        model_display_name,
                        input_tokens: usage.input_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_write_tokens: usage.cache_write_tokens,
                        output_tokens: usage.output_tokens,
                        thinking_tokens: usage.thinking_output_tokens,
                        native_cost_usd: None,
                        decode_speed: None,
                        ttft_secs: None,
                        id_11: usage.identity.id_11,
                        id_12: usage.identity.id_12,
                        id_7: usage.identity.id_7,
                    };
                    if turn.has_signal() {
                        step_candidates.push((step_idx, turn));
                    }
                }

                for retry in step_rec.retry_usages {
                    let inherits_info =
                        retry.model_enum == 0 || retry.model_enum == step_rec.model_enum;
                    let (response_model, model_display_name) = if inherits_info {
                        (
                            step_rec.response_model.clone(),
                            step_rec.model_display_name.clone(),
                        )
                    } else {
                        (None, None)
                    };
                    let model_enum = if retry.model_enum != 0 {
                        retry.model_enum
                    } else {
                        step_rec.model_enum
                    };
                    let turn = RawTurn {
                        ts_secs: step_ts.0,
                        ts_quality: step_ts.1,
                        model_enum,
                        response_model,
                        model_display_name,
                        input_tokens: retry.input_tokens,
                        cache_read_tokens: retry.cache_read_tokens,
                        cache_write_tokens: retry.cache_write_tokens,
                        output_tokens: retry.output_tokens,
                        thinking_tokens: retry.thinking_output_tokens,
                        native_cost_usd: None,
                        decode_speed: None,
                        ttft_secs: None,
                        id_11: retry.identity.id_11,
                        id_12: retry.identity.id_12,
                        id_7: retry.identity.id_7,
                    };
                    if turn.has_signal() {
                        step_candidates.push((step_idx, turn));
                    }
                }
            }
        }
    }

    let mut has_gen_table = false;
    let mut covered_step_indices: HashSet<u64> = HashSet::new();
    let mut turns = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx ASC")
    {
        if let Ok(mut rows) = stmt.query([]) {
            has_gen_table = true;
            while let Ok(Some(row)) = rows.next() {
                let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
                    continue;
                };
                let Some(rec) = extract_gen_metadata_record(&blob) else {
                    continue;
                };
                covered_step_indices.extend(rec.step_indices.iter().copied());

                let (ts_secs, ts_quality) = rec
                    .step_indices
                    .iter()
                    .find_map(|idx| step_timestamps.get(idx).copied())
                    .map(|(ts, q)| (Some(ts), q))
                    .or_else(|| {
                        rec.chat_start_created_at_secs
                            .map(|ts| (Some(ts), TS_QUALITY_SECONDARY))
                    })
                    .unwrap_or_else(|| fallback_timestamp(trajectory_ts, db_mtime_secs));

                let decode_speed = rec.decode_speed_tps();
                let main_turn = RawTurn {
                    ts_secs,
                    ts_quality,
                    model_enum: rec.model_enum,
                    response_model: rec.response_model.clone(),
                    model_display_name: rec.model_display_name.clone(),
                    input_tokens: rec.input_tokens,
                    cache_read_tokens: rec.cache_read_tokens,
                    cache_write_tokens: rec.cache_write_tokens,
                    output_tokens: rec.output_tokens,
                    thinking_tokens: rec.thinking_output_tokens,
                    native_cost_usd: rec.model_cost_usd,
                    decode_speed,
                    ttft_secs: rec.ttft_secs,
                    id_11: rec.identity.id_11,
                    id_12: rec.identity.id_12,
                    id_7: rec.identity.id_7,
                };
                if main_turn.has_signal() {
                    turns.push(main_turn);
                }

                for retry in rec.retry_usages {
                    // When a retry attempt ran on a different model_enum than the outer chat,
                    // omit outer strings so the scan-time model_enum learning table resolves it.
                    let inherits_outer =
                        retry.model_enum == 0 || retry.model_enum == rec.model_enum;
                    let (response_model, model_display_name) = if inherits_outer {
                        (rec.response_model.clone(), rec.model_display_name.clone())
                    } else {
                        (None, None)
                    };
                    let model_enum = if retry.model_enum != 0 {
                        retry.model_enum
                    } else {
                        rec.model_enum
                    };
                    let retry_turn = RawTurn {
                        ts_secs,
                        ts_quality,
                        model_enum,
                        response_model,
                        model_display_name,
                        input_tokens: retry.input_tokens,
                        cache_read_tokens: retry.cache_read_tokens,
                        cache_write_tokens: retry.cache_write_tokens,
                        output_tokens: retry.output_tokens,
                        thinking_tokens: retry.thinking_output_tokens,
                        native_cost_usd: None,
                        decode_speed: None,
                        ttft_secs: None,
                        id_11: retry.identity.id_11,
                        id_12: retry.identity.id_12,
                        id_7: retry.identity.id_7,
                    };
                    if retry_turn.has_signal() {
                        turns.push(retry_turn);
                    }
                }
            }
        }
    }

    if !has_gen_table && !has_steps_table {
        return None;
    }

    for (step_idx, turn) in step_candidates {
        if !turn.has_any_identity() && covered_step_indices.contains(&step_idx) {
            continue;
        }
        turns.push(turn);
    }

    Some(deduplicate_raw_turns(turns))
}

fn scan_single_sqlite_db_raw(target: &DbTarget) -> Option<(&'static str, Vec<RawTurn>)> {
    let conn = open_readonly_db(&target.path).ok()?;
    let turns = extract_sqlite_db_turns(&conn, target.db_mtime_secs)?;
    Some((target.source, turns))
}

fn scan_sqlite_targets_raw(targets: &[DbTarget]) -> Vec<(&'static str, Vec<RawTurn>)> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    if targets.len() <= 8 || workers == 1 {
        targets
            .iter()
            .filter_map(scan_single_sqlite_db_raw)
            .collect()
    } else {
        let chunk_size = targets.len().div_ceil(workers);
        std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .filter_map(scan_single_sqlite_db_raw)
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut merged = Vec::new();
            for h in handles {
                if let Ok(part) = h.join() {
                    merged.extend(part);
                }
            }
            merged
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelEnumIdentity {
    CatalogEntry(usize),
    NormalizedName(String),
}

enum LearnedModelEnum<'b> {
    Generic {
        response_model: Option<&'b str>,
        model_display_name: Option<&'b str>,
        quality: (u8, u8),
    },
    Concrete {
        identity: ModelEnumIdentity,
        response_model: Option<&'b str>,
        model_display_name: Option<&'b str>,
        quality: (u8, u8),
    },
    Poisoned,
}

fn normalized_fallback_model_identity(group: &str) -> String {
    let norm = normalize_model_key(group);
    let mut words: Vec<&str> = norm.split_whitespace().collect();
    while words.len() > 1 && matches!(words.last().copied(), Some("trial" | "preview" | "latest")) {
        words.pop();
    }
    words.join(" ")
}

struct ResolvedTurnPricing {
    model_group: String,
    pricing_key: Option<String>,
    cost_usd: Option<f64>,
}

type ModelLookupKey<'b> = (Option<&'b str>, Option<&'b str>, u64);
type ResolvedModelInfo = (String, Option<usize>, bool);

/// Shared scan-time model-enum learner, memoized catalog resolver, and dynamic
/// `response_model` pricing inferrer used by both `stats` projection and statusline cost.
struct TurnPricer<'a, 'b> {
    pricing: &'a PricingCatalog,
    learned_enums: HashMap<u64, (Option<&'b str>, Option<&'b str>)>,
    resolution_cache: HashMap<ModelLookupKey<'b>, ResolvedModelInfo>,
    inferred_response_models: HashMap<&'b str, Option<usize>>,
}

impl<'a, 'b> TurnPricer<'a, 'b> {
    fn resolve_uncached(
        pricing: &PricingCatalog,
        resp: Option<&str>,
        disp: Option<&str>,
        model_enum: u64,
    ) -> ResolvedModelInfo {
        let group = derive_model_name(disp, resp, model_enum);
        let active: Vec<&str> = [resp, disp, Some(group.as_str())]
            .into_iter()
            .flatten()
            .collect();
        let entry_idx = pricing.resolve_entry_idx(&active);
        let specific = PricingCatalog::has_specific_model_qualifier(resp, disp);
        (group, entry_idx, specific)
    }

    fn build(pricing: &'a PricingCatalog, turns: impl IntoIterator<Item = &'b RawTurn>) -> Self {
        let mut resolution_cache: HashMap<ModelLookupKey<'b>, ResolvedModelInfo> = HashMap::new();
        let mut enum_states: HashMap<u64, LearnedModelEnum<'b>> = HashMap::new();

        for turn in turns {
            let resp = turn
                .response_model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let disp = turn
                .model_display_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());

            let (group, entry_idx, _) = resolution_cache
                .entry((resp, disp, turn.model_enum))
                .or_insert_with(|| Self::resolve_uncached(pricing, resp, disp, turn.model_enum));

            if turn.model_enum == 0 || (resp.is_none() && disp.is_none()) {
                continue;
            }

            let disp_rank = display_name_rank(disp);
            let resp_rank = response_model_rank(resp);
            let quality = (disp_rank, resp_rank);
            let is_concrete = entry_idx.is_some()
                || disp_rank == 2
                || PricingCatalog::has_specific_model_qualifier(None, resp);

            let new_identity = if is_concrete {
                Some(match *entry_idx {
                    Some(idx) => ModelEnumIdentity::CatalogEntry(idx),
                    None => {
                        ModelEnumIdentity::NormalizedName(normalized_fallback_model_identity(group))
                    }
                })
            } else {
                None
            };

            match enum_states.get_mut(&turn.model_enum) {
                None => {
                    let state = match new_identity {
                        Some(identity) => LearnedModelEnum::Concrete {
                            identity,
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        },
                        None => LearnedModelEnum::Generic {
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        },
                    };
                    enum_states.insert(turn.model_enum, state);
                }
                Some(LearnedModelEnum::Poisoned) => {}
                Some(slot @ LearnedModelEnum::Generic { .. }) => {
                    if let Some(identity) = new_identity {
                        *slot = LearnedModelEnum::Concrete {
                            identity,
                            response_model: resp,
                            model_display_name: disp,
                            quality,
                        };
                    } else if let LearnedModelEnum::Generic {
                        response_model,
                        model_display_name,
                        quality: prev_q,
                    } = slot
                    {
                        if quality > *prev_q {
                            *response_model = resp;
                            *model_display_name = disp;
                            *prev_q = quality;
                        }
                    }
                }
                Some(slot @ LearnedModelEnum::Concrete { .. }) => {
                    // Generic-only observations (e.g. legacy "Gemini Flash") never poison a concrete enum.
                    let Some(identity) = new_identity else {
                        continue;
                    };
                    let same_identity = matches!(
                        slot,
                        LearnedModelEnum::Concrete {
                            identity: prev_id,
                            ..
                        } if *prev_id == identity
                    );
                    if !same_identity {
                        *slot = LearnedModelEnum::Poisoned;
                    } else if let LearnedModelEnum::Concrete {
                        response_model,
                        model_display_name,
                        quality: prev_q,
                        ..
                    } = slot
                    {
                        if quality > *prev_q {
                            if resp.is_some() {
                                *response_model = resp;
                            }
                            if disp_rank >= display_name_rank(*model_display_name) && disp.is_some()
                            {
                                *model_display_name = disp;
                            }
                            *prev_q = (
                                display_name_rank(*model_display_name),
                                response_model_rank(*response_model),
                            );
                        }
                    }
                }
            }
        }

        let mut learned_enums = HashMap::new();
        for (model_enum, state) in enum_states {
            match state {
                LearnedModelEnum::Concrete {
                    response_model,
                    model_display_name,
                    ..
                }
                | LearnedModelEnum::Generic {
                    response_model,
                    model_display_name,
                    ..
                } => {
                    learned_enums.insert(model_enum, (response_model, model_display_name));
                    resolution_cache
                        .entry((response_model, model_display_name, model_enum))
                        .or_insert_with(|| {
                            Self::resolve_uncached(
                                pricing,
                                response_model,
                                model_display_name,
                                model_enum,
                            )
                        });
                }
                LearnedModelEnum::Poisoned => {}
            }
        }

        // Round 1 dynamic inference: map response_models that uniquely resolved to one
        // catalog entry elsewhere in the dataset, poisoning ambiguous or explicitly-qualified
        // unpriced variants.
        let mut inferred_response_models: HashMap<&'b str, Option<usize>> = HashMap::new();
        for ((resp_opt, _, _), (_, entry_idx, has_specific_qualifier)) in &resolution_cache {
            let Some(resp) = *resp_opt else {
                continue;
            };
            match (*entry_idx, *has_specific_qualifier) {
                (Some(idx), _) => {
                    inferred_response_models
                        .entry(resp)
                        .and_modify(|slot| {
                            if *slot != Some(idx) {
                                *slot = None;
                            }
                        })
                        .or_insert(Some(idx));
                }
                (None, true) => {
                    inferred_response_models.insert(resp, None);
                }
                (None, false) => {}
            }
        }

        Self {
            pricing,
            learned_enums,
            resolution_cache,
            inferred_response_models,
        }
    }

    fn price_turn(&self, turn: &RawTurn) -> ResolvedTurnPricing {
        let raw_resp = turn
            .response_model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let raw_disp = turn
            .model_display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let (eff_resp, eff_disp) =
            if raw_resp.is_none() && raw_disp.is_none() && turn.model_enum > 0 {
                self.learned_enums
                    .get(&turn.model_enum)
                    .copied()
                    .unwrap_or((None, None))
            } else {
                (raw_resp, raw_disp)
            };

        let cached = self
            .resolution_cache
            .get(&(eff_resp, eff_disp, turn.model_enum));
        let owned_fallback;
        let (model_group, direct_entry_idx, has_specific_qualifier) = match cached {
            Some(entry) => (&entry.0, entry.1, entry.2),
            None => {
                owned_fallback =
                    Self::resolve_uncached(self.pricing, eff_resp, eff_disp, turn.model_enum);
                (&owned_fallback.0, owned_fallback.1, owned_fallback.2)
            }
        };

        let (pricing_key, cost_usd) = if let Some(usd) = valid_native_cost(turn.native_cost_usd) {
            (Some("native".to_string()), Some(round_usd_6(usd)))
        } else {
            let resolved_idx = direct_entry_idx.or_else(|| {
                (!has_specific_qualifier)
                    .then(|| {
                        eff_resp
                            .and_then(|resp| self.inferred_response_models.get(resp).copied())
                            .flatten()
                    })
                    .flatten()
            });
            resolved_idx
                .and_then(|idx| {
                    self.pricing.estimate_cost_for_entry(
                        idx,
                        turn.input_tokens,
                        turn.cache_read_tokens,
                        turn.cache_write_tokens,
                        turn.output_tokens,
                    )
                })
                .unzip()
        };

        ResolvedTurnPricing {
            model_group: model_group.clone(),
            pricing_key,
            cost_usd,
        }
    }
}

struct SourcedTurn {
    source: UsageSource,
    turn: RawTurn,
}

fn deduplicate_sourced_turns(items: Vec<SourcedTurn>) -> Vec<SourcedTurn> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut uf = build_turn_union_find(items.iter().map(|i| &i.turn), items.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut merged: Vec<SourcedTurn> = Vec::with_capacity(items.len());

    for (idx, item) in items.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            let entry = &mut merged[slot];
            // Enforce deterministic UsageSource priority (Cli > Desktop).
            if entry.source == UsageSource::Desktop && item.source == UsageSource::Cli {
                let prev = std::mem::replace(&mut entry.turn, item.turn);
                entry.source = UsageSource::Cli;
                merge_raw_turn(&mut entry.turn, prev);
            } else {
                merge_raw_turn(&mut entry.turn, item.turn);
            }
        } else {
            root_to_slot.insert(root, merged.len());
            merged.push(item);
        }
    }

    merged
}

fn project_sqlite_conversations<'a>(
    conversations: impl IntoIterator<Item = (&'static str, &'a [RawTurn])>,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>) {
    let mut stats = SqliteStats::default();
    let mut raw_items: Vec<SourcedTurn> = Vec::new();

    for (source, turns) in conversations {
        stats.db_files += 1;
        let usage_source = UsageSource::from_root_label(source);
        for turn in turns {
            raw_items.push(SourcedTurn {
                source: usage_source,
                turn: turn.clone(),
            });
        }
    }

    let merged_turns = deduplicate_sourced_turns(raw_items);
    let pricer = TurnPricer::build(pricing, merged_turns.iter().map(|m| &m.turn));

    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut events = Vec::new();

    for entry in &merged_turns {
        let turn = &entry.turn;
        if let Some(speed) = valid_decode_speed(turn.decode_speed) {
            stats.completed_turns += 1;
            stats.total_output_tokens =
                stats.total_output_tokens.saturating_add(turn.output_tokens);
            speed_samples.push(speed);
        }
        if let Some(ttft) = valid_ttft_secs(turn.ttft_secs) {
            ttft_samples.push(ttft);
        }
        if !turn.has_billable_usage() {
            continue;
        }
        let Some(ts_secs) = turn.ts_secs else {
            continue;
        };

        let total_tokens = turn
            .input_tokens
            .saturating_add(turn.cache_read_tokens)
            .saturating_add(turn.cache_write_tokens)
            .saturating_add(turn.output_tokens);
        let (local_day, local_date) = tz.local_day_and_date(ts_secs);
        let resolved = pricer.price_turn(turn);

        events.push(UsageEvent {
            source: entry.source,
            local_day,
            local_date,
            model_group: resolved.model_group,
            pricing_key: resolved.pricing_key,
            input_tokens: turn.input_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_write_tokens: turn.cache_write_tokens,
            output_tokens: turn.output_tokens,
            thinking_tokens: turn.thinking_tokens,
            total_tokens,
            cost_usd: resolved.cost_usd,
            decode_speed: valid_decode_speed(turn.decode_speed),
            ttft_secs: valid_ttft_secs(turn.ttft_secs),
        });
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&speed_samples);
    stats.median_ttft_secs = compute_median(&ttft_samples);
    (stats, events)
}

pub(super) fn scan_sqlite_conversations(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>) {
    let targets = discover_sqlite_db_targets(roots);
    let scanned = scan_sqlite_targets_raw(&targets);
    project_sqlite_conversations(
        scanned
            .iter()
            .map(|(source, turns)| (*source, turns.as_slice())),
        tz,
        pricing,
    )
}

fn resolve_descendant_conversation_ids(cli_dir: &Path, root_id: &str) -> Vec<String> {
    if !is_valid_uuid(root_id) {
        return Vec::new();
    }
    let summaries_path = cli_dir.join("conversation_summaries.db");
    if !is_safe_sqlite_file(&summaries_path) {
        return Vec::new();
    }
    let Ok(conn) = open_readonly_db(&summaries_path) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "WITH RECURSIVE descendants(conversation_id, depth) AS (
             SELECT conversation_id, 1
             FROM conversation_summaries
             WHERE parent_conversation_id = ?1 COLLATE NOCASE
             UNION
             SELECT cs.conversation_id, d.depth + 1
             FROM conversation_summaries cs
             JOIN descendants d ON cs.parent_conversation_id = d.conversation_id COLLATE NOCASE
             WHERE d.depth < 8
         )
         SELECT DISTINCT conversation_id FROM descendants",
    ) else {
        return Vec::new();
    };
    let Ok(mut rows) = stmt.query([root_id]) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(id) = row.get::<_, String>(0) {
            if is_valid_uuid(&id)
                && !id.eq_ignore_ascii_case(root_id)
                && seen.insert(id.to_ascii_lowercase())
            {
                ids.push(id);
            }
        }
    }
    ids
}

fn read_single_db_turns(db_path: &Path) -> Option<Vec<RawTurn>> {
    let conn = open_readonly_db(db_path).ok()?;
    extract_sqlite_db_turns(&conn, None)
}

/// Deduplicates tagged turns across parent + descendant DBs via Union-Find, preferring
/// parent (`is_subagent == false`) attribution when the same turn appears in both.
fn deduplicate_parent_priority_turns<T>(
    items: Vec<T>,
    turn_ref: impl Fn(&T) -> &RawTurn,
    is_sub: impl Fn(&T) -> bool,
    mut merge_into: impl FnMut(&mut T, T),
) -> Vec<T> {
    if items.len() <= 1 {
        return items;
    }
    let mut uf = build_turn_union_find(items.iter().map(&turn_ref), items.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut deduped: Vec<T> = Vec::with_capacity(items.len());
    for (idx, item) in items.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            let existing = &mut deduped[slot];
            if is_sub(existing) && !is_sub(&item) {
                let prev = std::mem::replace(existing, item);
                merge_into(existing, prev);
            } else {
                merge_into(existing, item);
            }
        } else {
            root_to_slot.insert(root, deduped.len());
            deduped.push(item);
        }
    }
    deduped
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::stats::collect_stats_with_tz;
    use crate::stats::metric::StatsMetric;
    use crate::stats::render::render_stats_report_with_metric;
    use crate::stats::test_helpers::build_gen_blob;
    use crate::sys::time::parse_rfc3339_unix_secs;

    #[test]
    fn derive_model_name_is_dynamic_and_strips_only_effort_suffixes() {
        assert_eq!(
            derive_model_name(Some("Model Alpha (Medium)"), Some("model-alpha"), 1526),
            "Model Alpha"
        );
        assert_eq!(
            derive_model_name(Some("Model Alpha (High)"), Some("model-alpha"), 1525),
            "Model Alpha"
        );
        assert_eq!(
            derive_model_name(
                Some("Gemini 3.5 Flash (Extra Low)"),
                Some("gemini-3.5-flash-extra-low"),
                1527
            ),
            "Gemini 3.5 Flash"
        );
        assert_eq!(
            derive_model_name(
                Some("Gemini 3.5 Flash (xLow)"),
                Some("gemini-3.5-flash-xlow"),
                1528
            ),
            "Gemini 3.5 Flash"
        );
        assert!(is_effort_token("extralow"));
        assert!(is_effort_token("xlow"));
        assert_eq!(
            derive_model_name(Some("Orchestrator (TierA)"), Some("pro-orchestrator"), 1667),
            "Orchestrator (TierA)"
        );
        assert_eq!(
            derive_model_name(
                Some("Orchestrator (TierB)"),
                Some("pro-orchestrator-b"),
                1681
            ),
            "Orchestrator (TierB)"
        );
        assert_eq!(
            derive_model_name(
                Some("Worker Alpha (Orchestrator)"),
                Some("worker-agy"),
                1669
            ),
            "Worker Alpha (Orchestrator)"
        );
        assert_eq!(
            derive_model_name(None, Some("worker-tiered"), 1322),
            "worker-tiered"
        );
        assert_eq!(derive_model_name(None, None, 1999), "model#1999");
        assert_eq!(derive_model_name(None, None, 0), "unknown");
    }

    #[test]
    fn scans_empty_home_and_falls_back_to_db_mtime_when_step_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;

        let empty = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(empty.totals.requests, 0);
        assert_eq!(empty.sqlite.db_files, 0);
        let mut empty_out = Vec::new();
        render_stats_report_with_metric(
            &empty,
            StatsMetric::Total,
            None,
            true,
            true,
            &mut empty_out,
        )
        .unwrap();
        let empty_text = String::from_utf8(empty_out).unwrap();
        assert!(!empty_text.contains('\x1b'));
        assert!(
            empty_text.contains("Activity (20w): no activity | Less .. == -- ## ** More"),
            "unexpected text:\n{empty_text}"
        );
        assert!(
            empty_text.contains("       Jul         Aug            Sep         Oct         Nov")
        );
        assert!(empty_text
            .contains("  Mon  .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .."));
        assert!(
            empty_text.contains("  Sun  .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. .. ..")
        );
        assert!(empty_text.contains("Models: none recorded"));

        // Minimal gen_metadata-only DB without steps table falls back to DB mtime.
        let conv_dir = home.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&conv_dir).unwrap();
        let db_path = conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let blob_50 = build_gen_blob(None, 10, "tiny-model", "", 100, 200, 100, 40, 2, 1000, None);
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [blob_50],
        )
        .unwrap();
        drop(conn);
        fs::write(
            conv_dir.join("8d0371e1-5a05-423b-a8bc-ed75995f9a8b.db-wal"),
            b"junk",
        )
        .unwrap();
        fs::write(conv_dir.join("not-a-uuid.db"), b"junk").unwrap();

        let populated = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(populated.sqlite.db_files, 1);
        assert_eq!(populated.sqlite.completed_turns, 1);
        assert_eq!(populated.windows.all_time.requests, 1);
        assert_eq!(populated.models[0].model, "tiny-model");
    }

    #[test]
    fn project_sqlite_infers_unambiguous_response_model_without_crossing_ambiguous_or_explicit_tiers(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("cache");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(
            cache_dir.join("models_dev_pricing.json"),
            r#"{
              "fetched_at": 1000,
              "models": {
                "gemini-4-hydrogen": { "input_per_mtok": 1.5, "output_per_mtok": 7.5 },
                "gemini-4-carbon": { "input_per_mtok": 2.0, "output_per_mtok": 10.0 },
                "gemini-4-argon": { "input_per_mtok": 4.0, "output_per_mtok": 20.0 }
              }
            }"#,
        )
        .unwrap();
        let pricing = PricingCatalog::load_effective_with(
            Some(&cache_dir),
            crate::stats::SyncMode::Offline,
            1000,
            86_400,
            |_| unreachable!(),
        );

        let ts = Some(1_700_000_000_i64);
        let mk_turn = |resp: &str, disp: Option<&str>| RawTurn {
            ts_secs: ts,
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 0,
            response_model: Some(resp.to_string()),
            model_display_name: disp.map(str::to_string),
            input_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: None,
            id_12: None,
            id_7: None,
        };

        let turns = vec![
            // Unambiguous worker routing ID: turn 0 has full display name, turn 1 has legacy generic name.
            mk_turn(
                "gemini-flash-worker-agy",
                Some("Gemini 4 Hydrogen (Orchestrator)"),
            ),
            mk_turn("gemini-flash-worker-agy", Some("Gemini Flash")),
            // Ambiguous routing ID observed with two distinct models: bare turn 4 must stay unpriced.
            mk_turn("gemini-pro-orchestrator", Some("Orchestrator (Carbon)")),
            mk_turn("gemini-pro-orchestrator", Some("Orchestrator (Argon)")),
            mk_turn("gemini-pro-orchestrator", None),
            // Routing ID with an explicit unpriced codename: turn 6 and bare turn 7 must stay unpriced.
            mk_turn("custom-orchestrator", Some("Orchestrator (Carbon)")),
            mk_turn("custom-orchestrator", Some("Orchestrator (Xenon)")),
            mk_turn("custom-orchestrator", None),
            // Shared worker routing ID upgraded to a newer unpriced model whose name precedes
            // `(Orchestrator)`: turn 9 and generic turn 10 must stay unpriced (never mispriced as Hydrogen).
            mk_turn("shared-worker-id", Some("Gemini 4 Hydrogen (Orchestrator)")),
            mk_turn("shared-worker-id", Some("Gemini 5 Plasma (Orchestrator)")),
            mk_turn("shared-worker-id", Some("Gemini Flash")),
        ];

        let (_, events) = project_sqlite_conversations(
            [("cli", turns.as_slice())],
            &TimeZoneSpec::utc(),
            &pricing,
        );
        assert_eq!(events.len(), 11);
        assert_eq!(events[0].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[0].cost_usd, Some(1.5));
        assert_eq!(events[1].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[1].cost_usd, Some(1.5));

        assert_eq!(events[2].pricing_key.as_deref(), Some("gemini-4-carbon"));
        assert_eq!(events[3].pricing_key.as_deref(), Some("gemini-4-argon"));
        assert_eq!(events[4].pricing_key, None);
        assert_eq!(events[4].cost_usd, None);

        assert_eq!(events[5].pricing_key.as_deref(), Some("gemini-4-carbon"));
        assert_eq!(events[6].pricing_key, None);
        assert_eq!(events[6].cost_usd, None);
        assert_eq!(events[7].pricing_key, None);
        assert_eq!(events[7].cost_usd, None);

        assert_eq!(events[8].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[9].pricing_key, None);
        assert_eq!(events[9].cost_usd, None);
        assert_eq!(events[10].pricing_key, None);
        assert_eq!(events[10].cost_usd, None);
    }

    pub(super) fn test_pricing_catalog(cache_dir: &Path) -> PricingCatalog {
        fs::create_dir_all(cache_dir).unwrap();
        fs::write(
            cache_dir.join("models_dev_pricing.json"),
            r#"{
              "fetched_at": 1000,
              "models": {
                "gemini-4-hydrogen": { "input_per_mtok": 1.5, "output_per_mtok": 7.5 },
                "gemini-4-carbon": { "input_per_mtok": 2.0, "output_per_mtok": 10.0 },
                "gemini-4-argon": { "input_per_mtok": 4.0, "output_per_mtok": 20.0 }
              }
            }"#,
        )
        .unwrap();
        PricingCatalog::load_effective_with(
            Some(cache_dir),
            crate::stats::SyncMode::Offline,
            1000,
            86_400,
            |_| unreachable!(),
        )
    }

    #[test]
    fn extract_sqlite_db_turns_merges_steps_f9_f28_f17_and_suppresses_keyless_covered_steps() {
        use crate::agy::protobuf::{
            encode_gen_blob, encode_step_blob, encode_trajectory_meta_blob, GenBlobSpec,
            StepBlobSpec, UsageSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("conv.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE trajectory_metadata_blob (data BLOB);
             CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO trajectory_metadata_blob (data) VALUES (?1)",
            [encode_trajectory_meta_blob(1_795_000_000)],
        )
        .unwrap();

        // Step 1 is covered by gen_metadata(1): keyless f9 must be suppressed, while f28 retry
        // sharing id_11="r1" merges into gen_metadata(1) and lifts output_tokens to 300.
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
            [encode_step_blob(StepBlobSpec {
                created_at_secs: Some(1_795_000_100),
                usage: Some(UsageSpec {
                    model_enum: 1669,
                    input_tok: 99_999,
                    output_tok: 999,
                    ..Default::default()
                }),
                retries: vec![UsageSpec {
                    model_enum: 1669,
                    input_tok: 1_000,
                    output_tok: 300,
                    id_11: Some("r1"),
                    id_7: Some("c1"),
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        // Step 2 is NOT covered by gen_metadata (e.g. step_type = 23 compaction turn) with f8 ts.
        conn.execute(
            "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
            [encode_step_blob(StepBlobSpec {
                secondary_created_at_secs: Some(1_795_000_200),
                usage: Some(UsageSpec {
                    model_enum: 1669,
                    input_tok: 4_000,
                    output_tok: 80,
                    ..Default::default()
                }),
                ..Default::default()
            })],
        )
        .unwrap();

        // gen_metadata(1): covers step 1; has main turn id_11="r1", plus f17 retry on different
        // model_enum=1418 (must NOT inherit outer response_model/display_name).
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                model_enum: 1669,
                resp_model: Some("gemini-flash-worker-agy"),
                disp_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
                input_tok: 1_200,
                output_tok: 200,
                id_11: Some("r1"),
                dur_secs: 2,
                ttft_nanos: 500_000_000,
                retries: vec![UsageSpec {
                    model_enum: 1418,
                    input_tok: 600,
                    output_tok: 40,
                    id_11: Some("r-retry-other-model"),
                    ..Default::default()
                }],
                ..Default::default()
            })],
        )
        .unwrap();

        // gen_metadata(2): no step timestamp and no chat_start -> falls back to trajectory_metadata_blob.f2.
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
            [encode_gen_blob(GenBlobSpec {
                step_indices: vec![99],
                model_enum: 1418,
                resp_model: Some("gemini-4-argon"),
                disp_name: Some("Gemini 4 Argon"),
                input_tok: 500,
                output_tok: 50,
                id_11: Some("r-traj-fallback"),
                ..Default::default()
            })],
        )
        .unwrap();

        let turns = extract_sqlite_db_turns(&conn, Some(1_795_000_999)).unwrap();
        assert_eq!(turns.len(), 4);

        // Turn 0: merged gen_metadata(1) + steps(1).f28 retry via id_11="r1".
        assert_eq!(turns[0].ts_secs, Some(1_795_000_100));
        assert_eq!(turns[0].ts_quality, TS_QUALITY_STEP_PRIMARY);
        assert_eq!(turns[0].input_tokens, 1_200);
        assert_eq!(turns[0].output_tokens, 300);
        assert_eq!(turns[0].id_11.as_deref(), Some("r1"));
        assert_eq!(turns[0].id_7.as_deref(), Some("c1"));
        assert_eq!(
            turns[0].model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen (Orchestrator)")
        );
        assert_eq!(turns[0].decode_speed, Some(100.0));
        assert_eq!(turns[0].ttft_secs, Some(0.5));

        // Turn 1: f17 retry on model_enum=1418 did NOT inherit outer strings.
        assert_eq!(turns[1].model_enum, 1418);
        assert_eq!(turns[1].response_model, None);
        assert_eq!(turns[1].model_display_name, None);
        assert_eq!(turns[1].input_tokens, 600);

        // Turn 2: gen_metadata(2) fell back to trajectory_metadata_blob.f2 (q=2) before db_mtime.
        assert_eq!(turns[2].ts_secs, Some(1_795_000_000));
        assert_eq!(turns[2].ts_quality, TS_QUALITY_TRAJECTORY);

        // Turn 3: uncovered compaction step(2).f9 with f8 timestamp (q=3).
        assert_eq!(turns[3].ts_secs, Some(1_795_000_200));
        assert_eq!(turns[3].ts_quality, TS_QUALITY_SECONDARY);
        assert_eq!(turns[3].model_enum, 1669);
        assert_eq!(turns[3].input_tokens, 4_000);
        assert_eq!(turns[3].output_tokens, 80);
    }

    #[test]
    fn project_sqlite_union_find_transitively_dedups_three_records_across_roots() {
        let dir = tempfile::tempdir().unwrap();
        let pricing = test_pricing_catalog(&dir.path().join("cache"));

        let desk_a = vec![RawTurn {
            ts_secs: Some(1_795_002_000),
            ts_quality: TS_QUALITY_DB_MTIME,
            model_enum: 1669,
            response_model: Some("gemini-flash-worker-agy".to_string()),
            model_display_name: Some("Gemini Flash".to_string()),
            input_tokens: 1_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 100,
            thinking_tokens: 20,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: Some("r-shared".to_string()),
            id_12: None,
            id_7: None,
        }];
        let cli_b = vec![RawTurn {
            ts_secs: Some(1_795_001_500),
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 1669,
            response_model: Some("gemini-flash-worker-agy".to_string()),
            model_display_name: Some("Gemini 4 Hydrogen (Orchestrator)".to_string()),
            input_tokens: 2_000,
            cache_read_tokens: 500,
            cache_write_tokens: 0,
            output_tokens: 200,
            thinking_tokens: 50,
            native_cost_usd: None,
            decode_speed: Some(100.0),
            ttft_secs: Some(0.5),
            id_11: Some("r-shared".to_string()),
            id_12: None,
            id_7: Some("c-shared".to_string()),
        }];
        let desk_c = vec![RawTurn {
            ts_secs: Some(1_795_001_400),
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum: 0,
            response_model: None,
            model_display_name: None,
            input_tokens: 1_500,
            cache_read_tokens: 0,
            cache_write_tokens: 100,
            output_tokens: 500,
            thinking_tokens: 120,
            native_cost_usd: None,
            decode_speed: Some(80.0),
            ttft_secs: Some(0.9),
            id_11: None,
            id_12: Some("m-shared".to_string()),
            id_7: Some("c-shared".to_string()),
        }];

        let (stats, events) = project_sqlite_conversations(
            [
                ("desktop", desk_a.as_slice()),
                ("cli", cli_b.as_slice()),
                ("desktop", desk_c.as_slice()),
            ],
            &TimeZoneSpec::utc(),
            &pricing,
        );

        assert_eq!(stats.db_files, 3);
        assert_eq!(stats.completed_turns, 1);
        assert_eq!(stats.ttft_samples, 1);
        assert_eq!(stats.total_output_tokens, 500);
        assert_eq!(stats.median_tok_per_sec, Some(100.0));
        assert_eq!(stats.median_ttft_secs, Some(0.5));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, UsageSource::Cli);
        assert_eq!(events[0].input_tokens, 2_000);
        assert_eq!(events[0].cache_read_tokens, 500);
        assert_eq!(events[0].cache_write_tokens, 100);
        assert_eq!(events[0].output_tokens, 500);
        assert_eq!(events[0].thinking_tokens, 120);
        assert_eq!(events[0].model_group, "Gemini 4 Hydrogen (Orchestrator)");
        assert_eq!(events[0].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
    }

    #[test]
    fn turn_pricer_learns_model_enum_without_generic_poisoning_and_poisons_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let pricing = test_pricing_catalog(&dir.path().join("cache"));
        let ts = Some(1_795_000_000_i64);
        let mk = |model_enum: u64, resp: Option<&str>, disp: Option<&str>| RawTurn {
            ts_secs: ts,
            ts_quality: TS_QUALITY_STEP_PRIMARY,
            model_enum,
            response_model: resp.map(str::to_string),
            model_display_name: disp.map(str::to_string),
            input_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            thinking_tokens: 0,
            native_cost_usd: None,
            decode_speed: None,
            ttft_secs: None,
            id_11: None,
            id_12: None,
            id_7: None,
        };

        let turns = vec![
            // Enum 1669: generic "Gemini Flash" + concrete Hydrogen must NOT poison 1669.
            mk(1669, Some("gemini-flash-worker-agy"), Some("Gemini Flash")),
            mk(
                1669,
                Some("gemini-flash-worker-agy"),
                Some("Gemini 4 Hydrogen (Orchestrator)"),
            ),
            mk(1669, None, None),
            // Enum 1418: "gemini-4-argon" + "gemini-4-argon-trial" resolve identically -> NOT poisoned.
            mk(1418, Some("gemini-4-argon"), Some("Gemini 4 Argon (High)")),
            mk(1418, Some("gemini-4-argon-trial"), None),
            mk(1418, None, None),
            // Enum 9999: genuine conflict Carbon vs Argon -> POISONED.
            mk(9999, Some("gemini-4-carbon"), Some("Gemini 4 Carbon")),
            mk(9999, Some("gemini-4-argon"), Some("Gemini 4 Argon")),
            mk(9999, None, None),
        ];

        let (_, events) = project_sqlite_conversations(
            [("cli", turns.as_slice())],
            &TimeZoneSpec::utc(),
            &pricing,
        );
        assert_eq!(events.len(), 9);

        // Bare enum 1669 compaction turn learns Hydrogen.
        assert_eq!(events[2].model_group, "Gemini 4 Hydrogen (Orchestrator)");
        assert_eq!(events[2].pricing_key.as_deref(), Some("gemini-4-hydrogen"));
        assert_eq!(events[2].cost_usd, Some(1.5));

        // Bare enum 1418 turn learns Argon.
        assert_eq!(events[5].model_group, "Gemini 4 Argon");
        assert_eq!(events[5].pricing_key.as_deref(), Some("gemini-4-argon"));
        assert_eq!(events[5].cost_usd, Some(4.0));

        // Poisoned enum 9999 bare turn remains unresolved and unpriced.
        assert_eq!(events[8].model_group, "model#9999");
        assert_eq!(events[8].pricing_key, None);
        assert_eq!(events[8].cost_usd, None);
    }
}
