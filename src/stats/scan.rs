//! Read-only SQLite `gen_metadata` and `transcript.jsonl` scanners.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use super::pricing::{round_usd_6, PricingCatalog};
use super::tz::TimeZoneSpec;
use super::SqliteStats;
use crate::db::{compute_median, is_valid_uuid, open_readonly_db, resolve_conversation_db_path};
use crate::protobuf::{
    extract_gen_metadata_record, extract_step_created_at_secs, is_sane_decode_speed,
    GenMetadataRecord, MAX_TTFT_SECS,
};
use crate::timeutil::parse_rfc3339_unix_secs;

const MAX_TRANSCRIPT_SCAN_DEPTH: usize = 4;

const SKIP_DIRS: &[&str] = &[
    "steps",
    "tasks",
    "scratch",
    "media",
    "artifacts",
    "node_modules",
    ".git",
    "__pycache__",
];

fn is_effort_qualifier(tag: &str) -> bool {
    let norm: String = tag
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .collect();
    matches!(
        norm.as_str(),
        "low"
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

fn value_as_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                u
            } else if let Some(i) = n.as_i64() {
                i.max(0) as u64
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() && f >= 0.0 {
                    f as u64
                } else {
                    0
                }
            } else {
                0
            }
        }
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    }
}

fn value_as_positive_f64(v: Option<&Value>) -> Option<f64> {
    let f = v.and_then(Value::as_f64)?;
    (f.is_finite() && f > 0.0).then_some(f)
}

fn parse_duration_secs(v: Option<&Value>) -> Option<f64> {
    let val = v?;
    match val {
        Value::Number(n) => {
            let f = n.as_f64()?;
            (f.is_finite() && f >= 0.0).then_some(f)
        }
        Value::String(raw) => {
            let s = raw.trim();
            if s.is_empty() {
                return None;
            }
            let (num_str, scale) =
                if let Some(prefix) = s.strip_suffix("ms").or_else(|| s.strip_suffix("MS")) {
                    (prefix.trim(), 0.001)
                } else if let Some(prefix) = s.strip_suffix('s').or_else(|| s.strip_suffix('S')) {
                    (prefix.trim(), 1.0)
                } else {
                    (s, 1.0)
                };
            let parsed = num_str.parse::<f64>().ok()?;
            let secs = parsed * scale;
            (secs.is_finite() && secs >= 0.0).then_some(secs)
        }
        _ => None,
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn extract_canonical_event_key(
    obj: &serde_json::Map<String, Value>,
    root_source: &str,
    default_cascade: &str,
    line_idx: usize,
) -> String {
    let usage_obj = obj
        .get("chatModel")
        .and_then(|c| c.get("usage"))
        .and_then(Value::as_object)
        .or_else(|| obj.get("usage").and_then(Value::as_object));

    let explicit = usage_obj
        .and_then(|u| {
            non_empty_str(u.get("responseId"))
                .or_else(|| non_empty_str(u.get("response_id")))
                .or_else(|| non_empty_str(u.get("messageId")))
                .or_else(|| non_empty_str(u.get("message_id")))
        })
        .or_else(|| non_empty_str(obj.get("responseId")))
        .or_else(|| non_empty_str(obj.get("response_id")))
        .or_else(|| non_empty_str(obj.get("messageId")))
        .or_else(|| non_empty_str(obj.get("message_id")))
        .or_else(|| non_empty_str(obj.get("callId")))
        .or_else(|| non_empty_str(obj.get("call_id")));

    if let Some(id) = explicit {
        return format!("id:{id}");
    }

    let source = non_empty_str(obj.get("source")).unwrap_or(root_source);
    let cascade = non_empty_str(obj.get("cascadeId"))
        .or_else(|| non_empty_str(obj.get("cascade_id")))
        .unwrap_or(default_cascade);

    let step_idx = obj
        .get("stepIndices")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_u64)
        .or_else(|| obj.get("stepIndex").and_then(Value::as_u64))
        .or_else(|| obj.get("step_index").and_then(Value::as_u64))
        .unwrap_or(line_idx as u64);

    format!("{source}:{cascade}#{step_idx}")
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

#[derive(Debug, Clone)]
struct TranscriptJob {
    path: PathBuf,
    source: &'static str,
    cascade_hint: String,
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn cascade_hint_from_transcript_path(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for i in 0..components.len() {
        if components[i].eq_ignore_ascii_case("brain") {
            if let Some(next) = components.get(i + 1) {
                return next.clone();
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "session".to_string())
}

fn collect_transcript_jobs_in_brain(
    dir: &Path,
    source: &'static str,
    depth: usize,
    visited_dirs: &mut HashSet<PathBuf>,
    seen_files: &mut HashSet<PathBuf>,
    jobs: &mut Vec<TranscriptJob>,
) {
    if depth > MAX_TRANSCRIPT_SCAN_DEPTH || !dir.is_dir() {
        return;
    }
    let real_dir = canonical_or_self(dir);
    if !visited_dirs.insert(real_dir) {
        return;
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut sorted_entries: Vec<_> = entries.filter_map(Result::ok).collect();
    sorted_entries.sort_by_key(|e| e.file_name());

    for entry in sorted_entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if SKIP_DIRS.contains(&name_str.as_ref()) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };

        if meta.is_dir() {
            if depth == 0 {
                let std_log = path
                    .join(".system_generated")
                    .join("logs")
                    .join("transcript.jsonl");
                let direct_log = path.join("transcript.jsonl");
                let mut matched_fast = false;
                for candidate in [std_log, direct_log] {
                    if candidate.is_file() {
                        let real = canonical_or_self(&candidate);
                        if seen_files.insert(real) {
                            jobs.push(TranscriptJob {
                                cascade_hint: name_str.clone().into_owned(),
                                path: candidate,
                                source,
                            });
                        }
                        matched_fast = true;
                    }
                }
                if matched_fast {
                    continue;
                }
            }
            collect_transcript_jobs_in_brain(
                &path,
                source,
                depth + 1,
                visited_dirs,
                seen_files,
                jobs,
            );
        } else if meta.is_file() && name_str == "transcript.jsonl" {
            let real = canonical_or_self(&path);
            if seen_files.insert(real) {
                jobs.push(TranscriptJob {
                    cascade_hint: cascade_hint_from_transcript_path(&path),
                    path,
                    source,
                });
            }
        }
    }
}

fn resolve_turn_cost(
    pricing: &PricingCatalog,
    native_cost_usd: Option<f64>,
    candidates: &[Option<&str>],
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
) -> (Option<String>, Option<f64>) {
    if let Some(usd) = native_cost_usd {
        return (Some("native".to_string()), Some(round_usd_6(usd)));
    }
    let active: Vec<&str> = candidates.iter().copied().flatten().collect();
    pricing
        .estimate_cost(
            &active,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        )
        .unzip()
}

fn parse_transcript_line(
    line: &str,
    job: &TranscriptJob,
    line_idx: usize,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    covered_conversations: &HashSet<(&'static str, String)>,
) -> Option<(String, UsageEvent)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Value::Object(obj) = serde_json::from_str::<Value>(trimmed).ok()? else {
        return None;
    };

    if let Some(cid) =
        non_empty_str(obj.get("cascadeId")).or_else(|| non_empty_str(obj.get("cascade_id")))
    {
        if is_valid_uuid(cid)
            && covered_conversations.contains(&(job.source, cid.to_ascii_lowercase()))
        {
            return None;
        }
    }

    let created_at = non_empty_str(obj.get("created_at"))?;
    let unix_secs = parse_rfc3339_unix_secs(created_at)?;
    let (local_day, local_date) = tz.local_day_and_date(unix_secs);

    let cm_obj = obj.get("chatModel").and_then(Value::as_object);
    let usage_obj = cm_obj
        .and_then(|cm| cm.get("usage"))
        .and_then(Value::as_object);

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, thinking_tokens) =
        if ["input_tokens", "output_tokens", "cache_read_tokens"]
            .iter()
            .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
        {
            (
                value_as_u64(obj.get("input_tokens")),
                value_as_u64(obj.get("output_tokens")),
                value_as_u64(obj.get("cache_read_tokens")),
                value_as_u64(obj.get("cache_write_tokens")),
                value_as_u64(obj.get("thinking_output_tokens")),
            )
        } else if let Some(u) = usage_obj {
            (
                value_as_u64(u.get("inputTokens").or_else(|| u.get("input_tokens"))),
                value_as_u64(u.get("outputTokens").or_else(|| u.get("output_tokens"))),
                value_as_u64(
                    u.get("cacheReadTokens")
                        .or_else(|| u.get("cache_read_tokens")),
                ),
                value_as_u64(
                    u.get("cacheWriteTokens")
                        .or_else(|| u.get("cache_write_tokens")),
                ),
                value_as_u64(
                    u.get("thinkingOutputTokens")
                        .or_else(|| u.get("thinking_output_tokens")),
                ),
            )
        } else {
            (0, 0, 0, 0, 0)
        };

    let total_tokens = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens)
        .saturating_add(output_tokens);
    if total_tokens == 0 {
        return None;
    }

    let disp = cm_obj
        .and_then(|m| non_empty_str(m.get("modelDisplayName")))
        .or_else(|| non_empty_str(obj.get("modelDisplayName")));
    let resp = cm_obj
        .and_then(|m| {
            non_empty_str(m.get("responseModel"))
                .or_else(|| non_empty_str(m.get("model")))
                .or_else(|| non_empty_str(m.get("modelId")))
        })
        .or_else(|| non_empty_str(obj.get("responseModel")))
        .or_else(|| non_empty_str(obj.get("model")))
        .or_else(|| non_empty_str(obj.get("modelId")));
    let model_group = derive_model_name(disp, resp, 0);

    let native_cost = cm_obj
        .and_then(|cm| value_as_positive_f64(cm.get("modelCost")))
        .or_else(|| value_as_positive_f64(obj.get("cost_usd")))
        .or_else(|| value_as_positive_f64(obj.get("cost")));

    let (pricing_key, cost_usd) = resolve_turn_cost(
        pricing,
        native_cost,
        &[resp, disp, Some(model_group.as_str())],
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        output_tokens,
    );

    let streaming_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("streamingDuration")))
        .or_else(|| parse_duration_secs(obj.get("streamingSec")));
    let decode_speed = streaming_secs
        .filter(|&secs| is_sane_decode_speed(output_tokens, secs))
        .map(|secs| (output_tokens as f64) / secs);

    let ttft_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("timeToFirstToken")))
        .or_else(|| parse_duration_secs(obj.get("ttftSec")))
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);

    let event_key = extract_canonical_event_key(&obj, job.source, &job.cascade_hint, line_idx);

    Some((
        event_key,
        UsageEvent {
            source: UsageSource::from_root_label(job.source),
            local_day,
            local_date,
            model_group,
            pricing_key,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            thinking_tokens,
            total_tokens,
            cost_usd,
            decode_speed,
            ttft_secs,
        },
    ))
}

pub(super) fn scan_transcripts(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    covered_conversations: &HashSet<(&'static str, String)>,
) -> Vec<UsageEvent> {
    let mut visited_dirs = HashSet::new();
    let mut seen_files = HashSet::new();
    let mut jobs = Vec::new();

    for (source, root) in roots {
        let brain_dir = root.join("brain");
        collect_transcript_jobs_in_brain(
            &brain_dir,
            source,
            0,
            &mut visited_dirs,
            &mut seen_files,
            &mut jobs,
        );
    }

    let mut seen_events = HashSet::new();
    let mut events = Vec::new();
    let mut byte_buf = Vec::with_capacity(4096);

    for job in &jobs {
        if is_valid_uuid(&job.cascade_hint)
            && covered_conversations.contains(&(job.source, job.cascade_hint.to_ascii_lowercase()))
        {
            continue;
        }
        let Ok(file) = File::open(&job.path) else {
            continue;
        };
        let mut reader = BufReader::new(file);
        let mut line_idx = 0usize;

        loop {
            byte_buf.clear();
            let Ok(read_bytes) = reader.read_until(b'\n', &mut byte_buf) else {
                break;
            };
            if read_bytes == 0 {
                break;
            }
            if let Ok(line) = std::str::from_utf8(&byte_buf) {
                if let Some((event_key, event)) =
                    parse_transcript_line(line, job, line_idx, tz, pricing, covered_conversations)
                {
                    if seen_events.insert(event_key) {
                        events.push(event);
                    }
                }
            }
            line_idx += 1;
        }
    }

    events
}

/// File-level change fingerprint for a SQLite conversation DB and its `-wal` sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DbFileFingerprint {
    pub(super) db_mtime_secs: i64,
    pub(super) db_mtime_nanos: u32,
    pub(super) db_size: u64,
    /// Normalized to `0` when `wal_size == 0` so AGY startup touching idle 0-byte
    /// `-wal` files does not trigger redundant re-scans.
    pub(super) wal_mtime_secs: i64,
    pub(super) wal_mtime_nanos: u32,
    pub(super) wal_size: u64,
}

fn modified_timestamp(meta: &fs::Metadata) -> (i64, u32) {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                (-(d.as_secs() as i64), d.subsec_nanos())
            }
        },
        Err(_) => (0, 0),
    }
}

fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

pub(super) fn db_file_fingerprint(db_path: &Path) -> Option<DbFileFingerprint> {
    let db_meta = fs::metadata(db_path).ok()?;
    if !db_meta.is_file() {
        return None;
    }
    let (db_mtime_secs, db_mtime_nanos) = modified_timestamp(&db_meta);
    let db_size = db_meta.len();

    let (wal_mtime_secs, wal_mtime_nanos, wal_size) = match fs::metadata(wal_sidecar_path(db_path))
    {
        Ok(wal_meta) if wal_meta.is_file() && wal_meta.len() > 0 => {
            let (secs, nanos) = modified_timestamp(&wal_meta);
            (secs, nanos, wal_meta.len())
        }
        _ => (0, 0, 0),
    };

    Some(DbFileFingerprint {
        db_mtime_secs,
        db_mtime_nanos,
        db_size,
        wal_mtime_secs,
        wal_mtime_nanos,
        wal_size,
    })
}

pub(super) fn root_path_key(root: &Path) -> String {
    canonical_or_self(root).to_string_lossy().into_owned()
}

#[derive(Debug, Clone)]
pub(super) struct DbTarget {
    pub(super) source: &'static str,
    pub(super) root_key: String,
    pub(super) uuid: String,
    pub(super) path: PathBuf,
    pub(super) fingerprint: DbFileFingerprint,
}

/// Timezone- and pricing-independent turn record extracted from a SQLite conversation DB.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RawTurn {
    pub(super) ts_secs: Option<i64>,
    pub(super) model_enum: u64,
    pub(super) response_model: Option<String>,
    pub(super) model_display_name: Option<String>,
    pub(super) input_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) cache_write_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) thinking_tokens: u64,
    pub(super) native_cost_usd: Option<f64>,
    pub(super) decode_speed: Option<f64>,
    pub(super) ttft_secs: Option<f64>,
}

impl RawTurn {
    pub(super) fn has_billable_usage(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.cache_read_tokens > 0
            || self.cache_write_tokens > 0
            || self.native_cost_usd.is_some()
    }
}

#[derive(Debug, Clone)]
pub(super) struct RawConversationScan {
    pub(super) source: &'static str,
    pub(super) root_key: String,
    pub(super) uuid: String,
    pub(super) fingerprint: DbFileFingerprint,
    pub(super) turns: Vec<RawTurn>,
}

pub(super) fn discover_sqlite_db_targets(roots: &[(&'static str, PathBuf)]) -> Vec<DbTarget> {
    let mut seen_dbs = HashSet::new();
    let mut targets = Vec::new();

    for (source, root) in roots {
        let root_key = root_path_key(root);
        let conv_dir = root.join("conversations");
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
            let Some(fingerprint) = db_file_fingerprint(&path) else {
                continue;
            };
            let uuid_lower = stem.to_ascii_lowercase();
            let real = canonical_or_self(&path);
            if seen_dbs.insert(real) {
                targets.push(DbTarget {
                    source,
                    root_key: root_key.clone(),
                    uuid: uuid_lower,
                    path,
                    fingerprint,
                });
            }
        }
    }

    targets
}

fn scan_single_sqlite_db_raw(target: &DbTarget) -> Option<RawConversationScan> {
    let db_mtime_secs = (target.fingerprint.db_mtime_secs > 0).then_some(target.fingerprint.db_mtime_secs);
    let conn = open_readonly_db(&target.path).ok()?;

    let mut step_timestamps: HashMap<u64, i64> = HashMap::new();
    if let Ok(mut step_stmt) =
        conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL")
    {
        if let Ok(mut step_rows) = step_stmt.query([]) {
            while let Ok(Some(row)) = step_rows.next() {
                if let (Ok(idx), Ok(meta_blob)) =
                    (row.get::<_, i64>(0), row.get::<_, Vec<u8>>(1))
                {
                    if idx >= 0 {
                        if let Some(secs) = extract_step_created_at_secs(&meta_blob) {
                            step_timestamps.insert(idx as u64, secs);
                        }
                    }
                }
            }
        }
    }

    let mut stmt = conn
        .prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx ASC")
        .ok()?;
    let mut rows = stmt.query([]).ok()?;

    let mut turns = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
            continue;
        };
        let Some(rec) = extract_gen_metadata_record(&blob) else {
            continue;
        };

        let decode_speed = rec.decode_speed_tps();
        let ttft_secs = rec.ttft_secs;
        let ts_secs = rec
            .first_step_index
            .and_then(|step_idx| step_timestamps.get(&step_idx).copied())
            .or(rec.chat_start_created_at_secs)
            .or(db_mtime_secs);

        let turn = RawTurn {
            ts_secs,
            model_enum: rec.model_enum,
            response_model: rec.response_model,
            model_display_name: rec.model_display_name,
            input_tokens: rec.input_tokens,
            cache_read_tokens: rec.cache_read_tokens,
            cache_write_tokens: rec.cache_write_tokens,
            output_tokens: rec.output_tokens,
            thinking_tokens: rec.thinking_output_tokens,
            native_cost_usd: rec.model_cost_usd,
            decode_speed,
            ttft_secs,
        };
        if turn.decode_speed.is_none() && turn.ttft_secs.is_none() && !turn.has_billable_usage() {
            continue;
        }
        turns.push(turn);
    }

    Some(RawConversationScan {
        source: target.source,
        root_key: target.root_key.clone(),
        uuid: target.uuid.clone(),
        fingerprint: target.fingerprint,
        turns,
    })
}

pub(super) fn scan_sqlite_targets_raw(targets: &[DbTarget]) -> Vec<RawConversationScan> {
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

pub(super) fn project_sqlite_conversations<'a>(
    conversations: impl IntoIterator<Item = (&'static str, &'a str, &'a [RawTurn])>,
    archived_db_files: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (
    SqliteStats,
    Vec<UsageEvent>,
    HashSet<(&'static str, String)>,
) {
    let mut stats = SqliteStats {
        archived_db_files,
        ..SqliteStats::default()
    };
    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut events = Vec::new();
    let mut covered = HashSet::new();
    // Memoize model-group derivation and catalog entry resolution across turns so large
    // multi-month archives avoid repeated O(catalog_size) substring scans per turn.
    type ModelLookupKey<'b> = (Option<&'b str>, Option<&'b str>, u64);
    let mut model_resolution_cache: HashMap<ModelLookupKey<'_>, (String, Option<usize>)> =
        HashMap::new();

    for (source, uuid, turns) in conversations {
        stats.db_files += 1;
        let usage_source = UsageSource::from_root_label(source);
        let mut has_events = false;

        for turn in turns {
            if let Some(speed) = turn.decode_speed {
                stats.completed_turns += 1;
                stats.total_output_tokens = stats
                    .total_output_tokens
                    .saturating_add(turn.output_tokens);
                speed_samples.push(speed);
            }
            if let Some(ttft) = turn.ttft_secs {
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
            let cache_key = (
                turn.response_model.as_deref(),
                turn.model_display_name.as_deref(),
                turn.model_enum,
            );
            let (model_group, entry_idx) = model_resolution_cache
                .entry(cache_key)
                .or_insert_with(|| {
                    let group = derive_model_name(
                        turn.model_display_name.as_deref(),
                        turn.response_model.as_deref(),
                        turn.model_enum,
                    );
                    let active: Vec<&str> = [
                        turn.response_model.as_deref(),
                        turn.model_display_name.as_deref(),
                        Some(group.as_str()),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let idx = pricing.resolve_entry_idx(&active);
                    (group, idx)
                });
            let (pricing_key, cost_usd) = if let Some(usd) = turn.native_cost_usd {
                (Some("native".to_string()), Some(round_usd_6(usd)))
            } else {
                (*entry_idx)
                    .and_then(|idx| {
                        pricing.estimate_cost_for_entry(
                            idx,
                            turn.input_tokens,
                            turn.cache_read_tokens,
                            turn.cache_write_tokens,
                            turn.output_tokens,
                            true,
                        )
                    })
                    .unzip()
            };

            has_events = true;
            events.push(UsageEvent {
                source: usage_source,
                local_day,
                local_date,
                model_group: model_group.clone(),
                pricing_key,
                input_tokens: turn.input_tokens,
                cache_read_tokens: turn.cache_read_tokens,
                cache_write_tokens: turn.cache_write_tokens,
                output_tokens: turn.output_tokens,
                thinking_tokens: turn.thinking_tokens,
                total_tokens,
                cost_usd,
                decode_speed: turn.decode_speed,
                ttft_secs: turn.ttft_secs,
            });
        }

        if has_events {
            covered.insert((source, uuid.to_owned()));
        }
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&speed_samples);
    stats.median_ttft_secs = compute_median(&ttft_samples);
    (stats, events, covered)
}

fn has_billable_usage(rec: &GenMetadataRecord) -> bool {
    rec.input_tokens > 0
        || rec.output_tokens > 0
        || rec.cache_read_tokens > 0
        || rec.cache_write_tokens > 0
        || rec.model_cost_usd.is_some()
}

pub(crate) struct ConversationCost {
    pub(crate) usd: Option<f64>,
    pub(crate) subagent_usd: Option<f64>,
    pub(crate) unpriced_turns: u64,
}

fn resolve_descendant_conversation_ids(cli_dir: &Path, root_id: &str) -> Vec<String> {
    if !is_valid_uuid(root_id) {
        return Vec::new();
    }
    let summaries_path = cli_dir.join("conversation_summaries.db");
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

fn accumulate_single_db_cost(
    db_path: &Path,
    pricing: &PricingCatalog,
) -> Option<(f64, u64, u64)> {
    let conn = open_readonly_db(db_path).ok()?;
    let mut stmt = conn
        .prepare("SELECT data FROM gen_metadata WHERE data IS NOT NULL")
        .ok()?;
    let mut rows = stmt.query([]).ok()?;

    let mut total_usd = 0.0_f64;
    let mut priced_turns = 0_u64;
    let mut unpriced_turns = 0_u64;
    while let Some(row) = rows.next().ok()? {
        let Some(blob) = row.get_ref(0).ok().and_then(|value| value.as_blob().ok()) else {
            continue;
        };
        let Some(rec) = extract_gen_metadata_record(blob) else {
            continue;
        };
        if !has_billable_usage(&rec) {
            continue;
        }
        let model_group = derive_model_name(
            rec.model_display_name.as_deref(),
            rec.response_model.as_deref(),
            rec.model_enum,
        );
        let (_, turn_usd) = resolve_turn_cost(
            pricing,
            rec.model_cost_usd,
            &[
                rec.response_model.as_deref(),
                rec.model_display_name.as_deref(),
                Some(model_group.as_str()),
            ],
            rec.input_tokens,
            rec.cache_read_tokens,
            rec.cache_write_tokens,
            rec.output_tokens,
        );
        if let Some(usd) = turn_usd {
            total_usd += usd;
            priced_turns += 1;
        } else {
            unpriced_turns += 1;
        }
    }
    Some((total_usd, priced_turns, unpriced_turns))
}

/// Estimates the subtotal for priced usage in a SQLite conversation and any descendant subagents.
pub(crate) fn estimate_conversation_cost_usd(
    cli_dir: &Path,
    conversation_id: &str,
    pricing: &PricingCatalog,
) -> Option<ConversationCost> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let (parent_usd, parent_priced, parent_unpriced) =
        match resolve_conversation_db_path(cli_dir, conversation_id) {
            Some(db_path) => accumulate_single_db_cost(&db_path, pricing)?,
            None => (0.0, 0, 0),
        };

    let mut sub_usd = 0.0_f64;
    let mut sub_priced = 0_u64;
    let mut sub_unpriced = 0_u64;
    for desc_id in resolve_descendant_conversation_ids(cli_dir, conversation_id) {
        let Some(db_path) = resolve_conversation_db_path(cli_dir, &desc_id) else {
            continue;
        };
        if let Some((usd, priced, unpriced)) = accumulate_single_db_cost(&db_path, pricing) {
            sub_usd += usd;
            sub_priced += priced;
            sub_unpriced += unpriced;
        }
    }

    let total_priced = parent_priced + sub_priced;
    let total_unpriced = parent_unpriced + sub_unpriced;
    let total_usd = parent_usd + sub_usd;
    (total_priced > 0 || total_unpriced > 0).then(|| ConversationCost {
        usd: (total_priced > 0 && total_usd.is_finite()).then(|| round_usd_6(total_usd)),
        subagent_usd: (sub_priced > 0 && sub_usd.is_finite()).then(|| round_usd_6(sub_usd)),
        unpriced_turns: total_unpriced,
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::stats::render::render_stats_report;
    use crate::stats::test_helpers::build_gen_blob;
    use crate::stats::collect_stats_with_tz;

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

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_and_duplicate_links_do_not_loop_or_double_count() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let brain = home.join(".gemini/antigravity-cli/brain");
        let real_log = brain.join("session-a/transcript.jsonl");
        fs::create_dir_all(real_log.parent().unwrap()).unwrap();
        let ev = r#"{"created_at":"2026-11-18T10:00:00Z","step_index":0,"chatModel":{"model":"local-model-v1","usage":{"inputTokens":1000,"cacheReadTokens":0,"outputTokens":100,"responseId":"sym-1"}}}"#;
        fs::write(&real_log, format!("{ev}\n")).unwrap();

        std::os::unix::fs::symlink(&brain, brain.join("cycle")).unwrap();
        let desk_session = home.join(".gemini/antigravity/brain/session-a");
        fs::create_dir_all(&desk_session).unwrap();
        std::os::unix::fs::symlink(&real_log, desk_session.join("transcript.jsonl")).unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 1);
        assert_eq!(report.totals.total_tokens, 1100);
        assert_eq!(report.models[0].model, "local-model-v1");
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
        render_stats_report(&empty, true, true, &mut empty_out).unwrap();
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
    fn empty_or_schemaless_db_falls_back_to_transcript_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uuid_empty = "55555555-5555-4555-8555-555555555555";
        let uuid_schemaless = "66666666-6666-4666-8666-666666666666";
        let conv_dir = home.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        // 0-byte .db file.
        fs::write(conv_dir.join(format!("{uuid_empty}.db")), b"").unwrap();
        let t1 = home.join(format!(
            ".gemini/antigravity-cli/brain/{uuid_empty}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(t1.parent().unwrap()).unwrap();
        fs::write(
            &t1,
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":1,\"model\":\"fallback-a\",\"input_tokens\":500,\"output_tokens\":50}\n",
        )
        .unwrap();

        // Schema-less SQLite .db missing gen_metadata.
        let conn = Connection::open(conv_dir.join(format!("{uuid_schemaless}.db"))).unwrap();
        conn.execute("CREATE TABLE unrelated (id INTEGER PRIMARY KEY)", [])
            .unwrap();
        drop(conn);
        let t2 = home.join(format!(
            ".gemini/antigravity-cli/brain/{uuid_schemaless}/.system_generated/logs/transcript.jsonl"
        ));
        fs::create_dir_all(t2.parent().unwrap()).unwrap();
        fs::write(
            &t2,
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":1,\"model\":\"fallback-b\",\"input_tokens\":700,\"output_tokens\":100}\n",
        )
        .unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.sqlite.db_files, 0);
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.total_tokens, 1350);

        let db = Connection::open(conv_dir.join(format!("{uuid_empty}.db"))).unwrap();
        db.execute(
            "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.sqlite.db_files, 1);
        assert_eq!(report.totals.total_tokens, 1350);
    }

    fn seed_cost_db(cli_dir: &Path, uuid: &str, idx: i64, blob: Vec<u8>) {
        let db_path = cli_dir.join("conversations").join(format!("{uuid}.db"));
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data) VALUES (?1, ?2)",
            rusqlite::params![idx, blob],
        )
        .unwrap();
    }

    #[test]
    fn estimate_conversation_cost_aggregates_descendant_subagents_and_guards_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";
        let grandchild = "33333333-3333-4333-8333-333333333333";
        let missing_child = "44444444-4444-4444-8444-444444444444";
        let unrelated = "55555555-5555-4555-8555-555555555555";
        let pricing = PricingCatalog::default();

        seed_cost_db(
            &cli_dir,
            parent,
            1,
            build_gen_blob(None, 10, "m-parent", "", 100, 0, 50, 0, 1, 100, Some(0.10)),
        );
        seed_cost_db(
            &cli_dir,
            child,
            1,
            build_gen_blob(None, 10, "m-child", "", 100, 0, 50, 0, 1, 100, Some(0.20)),
        );
        seed_cost_db(
            &cli_dir,
            grandchild,
            1,
            build_gen_blob(None, 10, "m-grand", "", 100, 0, 50, 0, 1, 100, Some(0.15)),
        );
        seed_cost_db(
            &cli_dir,
            grandchild,
            2,
            build_gen_blob(None, 10, "m-unpriced", "", 100, 0, 50, 0, 1, 100, None),
        );
        seed_cost_db(
            &cli_dir,
            unrelated,
            1,
            build_gen_blob(None, 10, "m-other", "", 100, 0, 50, 0, 1, 100, Some(9.99)),
        );

        // Without conversation_summaries.db, only parent is attributed.
        let parent_only = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        assert_eq!(parent_only.usd, Some(0.1));
        assert_eq!(parent_only.subagent_usd, None);
        assert_eq!(parent_only.unpriced_turns, 0);

        let summaries = Connection::open(cli_dir.join("conversation_summaries.db")).unwrap();
        summaries
            .execute(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT,
                    parent_conversation_id TEXT,
                    nesting_depth INTEGER
                )",
                [],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [child, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 2)",
                [grandchild, child],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [missing_child, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 1)",
                [parent, parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, 3)",
                [parent, grandchild],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES ('not-a-uuid', ?1, 1)",
                [parent],
            )
            .unwrap();
        summaries
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, NULL, 0)",
                [unrelated],
            )
            .unwrap();
        drop(summaries);

        let combined = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        assert_eq!(combined.usd, Some(0.45));
        assert_eq!(combined.subagent_usd, Some(0.35));
        assert_eq!(combined.unpriced_turns, 1);
    }
}
