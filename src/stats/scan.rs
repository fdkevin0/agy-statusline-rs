//! Read-only SQLite `gen_metadata` and `transcript.jsonl` / `transcript_full.jsonl` scanners.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::pricing::{normalize_model_key, round_usd_6, PricingCatalog, PricingFileFingerprint};
use super::tz::TimeZoneSpec;
use super::SqliteStats;
use crate::db::{
    compute_median, is_absent_or_real_file, is_real_dir, is_safe_sqlite_file, is_valid_uuid,
    open_readonly_db, resolve_conversation_db_path, sidecar_path,
};
use crate::fsutil::atomic_write;
use crate::protobuf::{
    extract_gen_metadata_record, extract_step_metadata_record, extract_trajectory_created_at_secs,
    is_sane_decode_speed, MAX_TTFT_SECS, MAX_TURN_SPEED_TPS,
};
use crate::timeutil::parse_rfc3339_unix_secs;

const MAX_TRANSCRIPT_SCAN_DEPTH: usize = 4;
const COMPACT_TRANSCRIPT_FILENAME: &str = "transcript.jsonl";
const FULL_TRANSCRIPT_FILENAME: &str = "transcript_full.jsonl";

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

fn extract_explicit_event_id(obj: &serde_json::Map<String, Value>) -> Option<&str> {
    let usage_obj = obj
        .get("chatModel")
        .and_then(|c| c.get("usage"))
        .and_then(Value::as_object)
        .or_else(|| obj.get("usage").and_then(Value::as_object));

    usage_obj
        .and_then(|u| {
            non_empty_str(u.get("responseId"))
                .or_else(|| non_empty_str(u.get("response_id")))
                .or_else(|| non_empty_str(u.get("messageId")))
                .or_else(|| non_empty_str(u.get("message_id")))
                .or_else(|| non_empty_str(u.get("callId")))
                .or_else(|| non_empty_str(u.get("call_id")))
        })
        .or_else(|| non_empty_str(obj.get("responseId")))
        .or_else(|| non_empty_str(obj.get("response_id")))
        .or_else(|| non_empty_str(obj.get("messageId")))
        .or_else(|| non_empty_str(obj.get("message_id")))
        .or_else(|| non_empty_str(obj.get("callId")))
        .or_else(|| non_empty_str(obj.get("call_id")))
}

fn extract_canonical_event_key(
    obj: &serde_json::Map<String, Value>,
    explicit_id: Option<&str>,
    root_source: &str,
    default_cascade: &str,
    line_idx: usize,
) -> String {
    if let Some(id) = explicit_id {
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

/// Conversations and explicit response/call IDs already covered by SQLite usage records.
#[derive(Debug, Default)]
pub(super) struct SqliteCoverage {
    pub(super) conversations: HashSet<(&'static str, String)>,
    pub(super) response_ids: HashSet<String>,
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

fn is_non_empty_file(path: &Path) -> bool {
    // Follow symlinks via `fs::metadata` so symlinked transcripts remain discoverable and
    // deduplicated by `canonical_or_self`.
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() > 0)
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
                let mut push_if_file = |candidate: PathBuf| -> bool {
                    if !is_non_empty_file(&candidate) {
                        return false;
                    }
                    let real = canonical_or_self(&candidate);
                    if seen_files.insert(real) {
                        jobs.push(TranscriptJob {
                            cascade_hint: name_str.clone().into_owned(),
                            path: candidate,
                            source,
                        });
                    }
                    true
                };
                let std_dir = path.join(".system_generated").join("logs");
                let mut matched_fast = false;
                for candidate in [
                    std_dir.join(COMPACT_TRANSCRIPT_FILENAME),
                    path.join(COMPACT_TRANSCRIPT_FILENAME),
                ] {
                    if push_if_file(candidate) {
                        matched_fast = true;
                        break;
                    }
                }
                if !matched_fast {
                    for candidate in [
                        std_dir.join(FULL_TRANSCRIPT_FILENAME),
                        path.join(FULL_TRANSCRIPT_FILENAME),
                    ] {
                        if push_if_file(candidate) {
                            matched_fast = true;
                            break;
                        }
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
        } else if meta.is_file()
            && meta.len() > 0
            && (name_str == COMPACT_TRANSCRIPT_FILENAME
                || (name_str == FULL_TRANSCRIPT_FILENAME
                    && !is_non_empty_file(&dir.join(COMPACT_TRANSCRIPT_FILENAME))))
        {
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
    covered: &SqliteCoverage,
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
            && covered
                .conversations
                .contains(&(job.source, cid.to_ascii_lowercase()))
        {
            return None;
        }
    }

    let explicit_id = extract_explicit_event_id(&obj);
    if explicit_id.is_some_and(|id| covered.response_ids.contains(id)) {
        return None;
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

    let event_key =
        extract_canonical_event_key(&obj, explicit_id, job.source, &job.cascade_hint, line_idx);

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
    covered: &SqliteCoverage,
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
            && covered
                .conversations
                .contains(&(job.source, job.cascade_hint.to_ascii_lowercase()))
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
                    parse_transcript_line(line, job, line_idx, tz, pricing, covered)
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DbFileFingerprint {
    pub(super) db_mtime_secs: i64,
    pub(super) db_mtime_nanos: u32,
    pub(super) db_ctime_secs: i64,
    pub(super) db_ctime_nanos: u32,
    pub(super) db_size: u64,
    /// Normalized to `0` when `wal_size == 0` so AGY startup touching idle 0-byte
    /// `-wal` files does not trigger redundant re-scans.
    pub(super) wal_mtime_secs: i64,
    pub(super) wal_mtime_nanos: u32,
    pub(super) wal_ctime_secs: i64,
    pub(super) wal_ctime_nanos: u32,
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

#[cfg(unix)]
fn status_change_timestamp(meta: &fs::Metadata) -> (i64, u32) {
    use std::os::unix::fs::MetadataExt;
    let secs = meta.ctime();
    let nanos = meta.ctime_nsec().clamp(0, 999_999_999) as u32;
    (secs, nanos)
}

#[cfg(not(unix))]
fn status_change_timestamp(_meta: &fs::Metadata) -> (i64, u32) {
    (0, 0)
}

pub(super) fn db_file_fingerprint(db_path: &Path) -> Option<DbFileFingerprint> {
    let db_meta = fs::symlink_metadata(db_path).ok()?;
    let db_ft = db_meta.file_type();
    if db_ft.is_symlink() || !db_ft.is_file() {
        return None;
    }
    if !is_absent_or_real_file(&sidecar_path(db_path, "-shm")) {
        return None;
    }
    let (db_mtime_secs, db_mtime_nanos) = modified_timestamp(&db_meta);
    let (db_ctime_secs, db_ctime_nanos) = status_change_timestamp(&db_meta);
    let db_size = db_meta.len();

    let (wal_mtime_secs, wal_mtime_nanos, wal_ctime_secs, wal_ctime_nanos, wal_size) =
        match fs::symlink_metadata(sidecar_path(db_path, "-wal")) {
            Ok(wal_meta) => {
                let wal_ft = wal_meta.file_type();
                if wal_ft.is_symlink() || !wal_ft.is_file() {
                    return None;
                }
                if wal_meta.len() > 0 {
                    let (mtime_secs, mtime_nanos) = modified_timestamp(&wal_meta);
                    let (ctime_secs, ctime_nanos) = status_change_timestamp(&wal_meta);
                    (
                        mtime_secs,
                        mtime_nanos,
                        ctime_secs,
                        ctime_nanos,
                        wal_meta.len(),
                    )
                } else {
                    (0, 0, 0, 0, 0)
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (0, 0, 0, 0, 0),
            Err(_) => return None,
        };

    Some(DbFileFingerprint {
        db_mtime_secs,
        db_mtime_nanos,
        db_ctime_secs,
        db_ctime_nanos,
        db_size,
        wal_mtime_secs,
        wal_mtime_nanos,
        wal_ctime_secs,
        wal_ctime_nanos,
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

const TS_QUALITY_NONE: u8 = 0;
const TS_QUALITY_DB_MTIME: u8 = 1;
const TS_QUALITY_TRAJECTORY: u8 = 2;
const TS_QUALITY_SECONDARY: u8 = 3;
const TS_QUALITY_STEP_PRIMARY: u8 = 4;

/// Timezone- and pricing-independent turn record extracted from a SQLite conversation DB.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RawTurn {
    pub(super) ts_secs: Option<i64>,
    pub(super) ts_quality: u8,
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
    pub(super) id_11: Option<String>,
    pub(super) id_12: Option<String>,
    pub(super) id_7: Option<String>,
}

impl RawTurn {
    pub(super) fn has_billable_usage(&self) -> bool {
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

#[derive(Debug, Clone)]
pub(super) struct RawConversationScan {
    pub(super) source: &'static str,
    pub(super) root_key: String,
    pub(super) uuid: String,
    pub(super) fingerprint: DbFileFingerprint,
    pub(super) turns: Vec<RawTurn>,
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
    dst.decode_speed = valid_decode_speed(dst.decode_speed).or_else(|| valid_decode_speed(other.decode_speed));
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

fn build_turn_union_find<'a>(turns: impl IntoIterator<Item = &'a RawTurn>, len: usize) -> UnionFind {
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

pub(super) fn discover_sqlite_db_targets(roots: &[(&'static str, PathBuf)]) -> Vec<DbTarget> {
    let mut seen_dbs = HashSet::new();
    let mut targets = Vec::new();

    for (source, root) in roots {
        let root_key = root_path_key(root);
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

fn scan_single_sqlite_db_raw(target: &DbTarget) -> Option<RawConversationScan> {
    let db_mtime_secs =
        (target.fingerprint.db_mtime_secs > 0).then_some(target.fingerprint.db_mtime_secs);
    let conn = open_readonly_db(&target.path).ok()?;
    let turns = extract_sqlite_db_turns(&conn, db_mtime_secs)?;

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
    while words.len() > 1 && matches!(words.last().copied(), Some("trial" | "preview" | "latest"))
    {
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
                            if disp_rank >= display_name_rank(*model_display_name)
                                && disp.is_some()
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

        let (eff_resp, eff_disp) = if raw_resp.is_none() && raw_disp.is_none() && turn.model_enum > 0
        {
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
                        true,
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

struct SourcedTurn<'a> {
    source: UsageSource,
    conv_source: &'static str,
    conv_uuid: &'a str,
    turn: RawTurn,
}

struct MergedSourcedTurn<'a> {
    source: UsageSource,
    turn: RawTurn,
    contributors: Vec<(&'static str, &'a str)>,
}

fn deduplicate_sourced_turns<'a>(items: Vec<SourcedTurn<'a>>) -> Vec<MergedSourcedTurn<'a>> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut uf = build_turn_union_find(items.iter().map(|i| &i.turn), items.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut merged: Vec<MergedSourcedTurn<'a>> = Vec::with_capacity(items.len());

    for (idx, item) in items.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            let entry = &mut merged[slot];
            entry.contributors.push((item.conv_source, item.conv_uuid));
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
            merged.push(MergedSourcedTurn {
                source: item.source,
                turn: item.turn,
                contributors: vec![(item.conv_source, item.conv_uuid)],
            });
        }
    }

    merged
}

pub(super) fn project_sqlite_conversations<'a>(
    conversations: impl IntoIterator<Item = (&'static str, &'a str, &'a [RawTurn])>,
    archived_db_files: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (SqliteStats, Vec<UsageEvent>, SqliteCoverage) {
    let mut stats = SqliteStats {
        archived_db_files,
        ..SqliteStats::default()
    };
    let mut covered = SqliteCoverage::default();
    let mut raw_items: Vec<SourcedTurn<'a>> = Vec::new();

    for (source, uuid, turns) in conversations {
        stats.db_files += 1;
        let usage_source = UsageSource::from_root_label(source);
        for turn in turns {
            for (_, val_opt) in turn.identity_keys() {
                if let Some(val) = val_opt.map(str::trim).filter(|s| !s.is_empty()) {
                    covered.response_ids.insert(val.to_string());
                }
            }
            raw_items.push(SourcedTurn {
                source: usage_source,
                conv_source: source,
                conv_uuid: uuid,
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
            stats.total_output_tokens = stats
                .total_output_tokens
                .saturating_add(turn.output_tokens);
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

        for &(conv_source, conv_uuid) in &entry.contributors {
            covered
                .conversations
                .insert((conv_source, conv_uuid.to_owned()));
        }

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
    (stats, events, covered)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ConversationCost {
    pub(crate) usd: Option<f64>,
    pub(crate) subagent_usd: Option<f64>,
    pub(crate) unpriced_turns: u64,
}

const CONV_COST_CACHE_FILE: &str = "conv_cost_cache.json";
const CONV_COST_CACHE_SCHEMA_VERSION: u32 = 1;
const CONV_COST_CACHE_MAX_ENTRIES: usize = 32;
const MAX_CONV_COST_CACHE_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CachedConversationCost {
    usd: Option<f64>,
    subagent_usd: Option<f64>,
    unpriced_turns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ConvCostCacheEntry {
    cli_dir: String,
    conversation_id: String,
    parent_fp: Option<DbFileFingerprint>,
    summaries_fp: Option<DbFileFingerprint>,
    descendants: Vec<(String, Option<DbFileFingerprint>)>,
    pricing_fp: PricingFileFingerprint,
    cost: CachedConversationCost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ConvCostCacheFile {
    schema_version: u32,
    #[serde(default)]
    entries: Vec<ConvCostCacheEntry>,
}

fn conversation_db_fingerprint(cli_dir: &Path, conversation_id: &str) -> Option<DbFileFingerprint> {
    let db_path = resolve_conversation_db_path(cli_dir, conversation_id)?;
    db_file_fingerprint(&db_path)
}

fn summaries_db_fingerprint(cli_dir: &Path) -> Option<DbFileFingerprint> {
    db_file_fingerprint(&cli_dir.join("conversation_summaries.db"))
}

fn read_conv_cost_cache_file(cache_dir: &Path) -> Option<ConvCostCacheFile> {
    let path = cache_dir.join(CONV_COST_CACHE_FILE);
    let meta = fs::symlink_metadata(&path).ok()?;
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() || meta.len() > MAX_CONV_COST_CACHE_BYTES {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let parsed = serde_json::from_slice::<ConvCostCacheFile>(&bytes).ok()?;
    (parsed.schema_version == CONV_COST_CACHE_SCHEMA_VERSION).then_some(parsed)
}

fn write_conv_cost_cache_entry_with_limit(
    cache_dir: &Path,
    entry: ConvCostCacheEntry,
    max_entries: usize,
) {
    let mut file = read_conv_cost_cache_file(cache_dir).unwrap_or(ConvCostCacheFile {
        schema_version: CONV_COST_CACHE_SCHEMA_VERSION,
        entries: Vec::new(),
    });
    file.entries
        .retain(|e| !(e.cli_dir == entry.cli_dir && e.conversation_id == entry.conversation_id));
    file.entries.insert(0, entry);
    file.entries.truncate(max_entries);
    if let Ok(bytes) = serde_json::to_vec(&file) {
        let _ = atomic_write(&cache_dir.join(CONV_COST_CACHE_FILE), &bytes);
    }
}

/// Fast `lstat`-only lookup in `conv_cost_cache.json` before parsing `models_dev_pricing.json`
/// or opening any SQLite DB for cost scanning.
pub(crate) fn lookup_cached_conversation_cost(
    cache_dir: &Path,
    cli_dir: &Path,
    conversation_id: &str,
    pricing_fp: PricingFileFingerprint,
) -> Option<ConversationCost> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }
    let parent_fp = conversation_db_fingerprint(cli_dir, conversation_id);
    let summaries_fp = summaries_db_fingerprint(cli_dir);
    if parent_fp.is_none() && summaries_fp.is_none() {
        return None;
    }
    let cache = read_conv_cost_cache_file(cache_dir)?;
    let cli_dir_key = root_path_key(cli_dir);
    let conv_id_lower = conversation_id.to_ascii_lowercase();
    let entry = cache
        .entries
        .iter()
        .find(|e| e.cli_dir == cli_dir_key && e.conversation_id == conv_id_lower)?;
    if entry.parent_fp != parent_fp
        || entry.summaries_fp != summaries_fp
        || entry.pricing_fp != pricing_fp
    {
        return None;
    }
    let descendants_match = entry
        .descendants
        .iter()
        .all(|(desc_id, desc_fp)| conversation_db_fingerprint(cli_dir, desc_id) == *desc_fp);
    if !descendants_match {
        return None;
    }
    if !entry.cost.usd.is_none_or(|v| v.is_finite() && v >= 0.0)
        || !entry
            .cost
            .subagent_usd
            .is_none_or(|v| v.is_finite() && v >= 0.0)
    {
        return None;
    }
    Some(ConversationCost {
        usd: entry.cost.usd,
        subagent_usd: entry.cost.subagent_usd,
        unpriced_turns: entry.cost.unpriced_turns,
    })
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

fn compute_conversation_cost_from_turns(
    tagged_turns: Vec<(bool, RawTurn)>,
    pricing: &PricingCatalog,
) -> Option<ConversationCost> {
    if tagged_turns.is_empty() {
        return None;
    }

    // Deduplicate across parent + descendant DBs (preferring parent attribution if shared).
    let mut uf = build_turn_union_find(tagged_turns.iter().map(|(_, t)| t), tagged_turns.len());
    let mut root_to_slot: HashMap<usize, usize> = HashMap::new();
    let mut deduped: Vec<(bool, RawTurn)> = Vec::with_capacity(tagged_turns.len());
    for (idx, (is_sub, turn)) in tagged_turns.into_iter().enumerate() {
        let root = uf.find(idx);
        if let Some(&slot) = root_to_slot.get(&root) {
            let (slot_sub, slot_turn) = &mut deduped[slot];
            if *slot_sub && !is_sub {
                let prev = std::mem::replace(slot_turn, turn);
                *slot_sub = false;
                merge_raw_turn(slot_turn, prev);
            } else {
                merge_raw_turn(slot_turn, turn);
            }
        } else {
            root_to_slot.insert(root, deduped.len());
            deduped.push((is_sub, turn));
        }
    }

    let pricer = TurnPricer::build(pricing, deduped.iter().map(|(_, t)| t));
    let mut parent_usd = 0.0_f64;
    let mut parent_priced = 0_u64;
    let mut parent_unpriced = 0_u64;
    let mut sub_usd = 0.0_f64;
    let mut sub_priced = 0_u64;
    let mut sub_unpriced = 0_u64;

    for (is_sub, turn) in &deduped {
        if !turn.has_billable_usage() {
            continue;
        }
        let resolved = pricer.price_turn(turn);
        match (is_sub, resolved.cost_usd) {
            (false, Some(usd)) => {
                parent_usd += usd;
                parent_priced += 1;
            }
            (false, None) => parent_unpriced += 1,
            (true, Some(usd)) => {
                sub_usd += usd;
                sub_priced += 1;
            }
            (true, None) => sub_unpriced += 1,
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

/// Estimates the subtotal for priced usage in a SQLite conversation and any descendant subagents,
/// optionally reusing an already-open read-only parent connection and persisting the result into
/// the bounded LRU `conv_cost_cache.json` cache.
pub(crate) fn estimate_conversation_cost_usd_with_parent_conn(
    cli_dir: &Path,
    conversation_id: &str,
    pricing: &PricingCatalog,
    parent_conn: Option<&rusqlite::Connection>,
    cache_target: Option<(&Path, PricingFileFingerprint)>,
) -> Option<ConversationCost> {
    if !is_valid_uuid(conversation_id) {
        return None;
    }

    let parent_db_path = resolve_conversation_db_path(cli_dir, conversation_id);
    // Stat before reading so concurrent SQLite commits invalidate on the next tick.
    let parent_fp = parent_db_path.as_deref().and_then(db_file_fingerprint);

    let mut tagged_turns: Vec<(bool, RawTurn)> = Vec::new();
    if let Some(conn) = parent_conn {
        for turn in extract_sqlite_db_turns(conn, None)? {
            tagged_turns.push((false, turn));
        }
    } else if let Some(db_path) = parent_db_path.as_deref() {
        for turn in read_single_db_turns(db_path)? {
            tagged_turns.push((false, turn));
        }
    }

    let summaries_fp = summaries_db_fingerprint(cli_dir);
    let desc_ids = resolve_descendant_conversation_ids(cli_dir, conversation_id);
    let mut descendants = Vec::with_capacity(desc_ids.len());
    for desc_id in desc_ids {
        let desc_db_path = resolve_conversation_db_path(cli_dir, &desc_id);
        let desc_fp = desc_db_path.as_deref().and_then(db_file_fingerprint);
        descendants.push((desc_id.to_ascii_lowercase(), desc_fp));
        if let Some(turns) = desc_db_path.as_deref().and_then(read_single_db_turns) {
            for turn in turns {
                tagged_turns.push((true, turn));
            }
        }
    }

    let cost = compute_conversation_cost_from_turns(tagged_turns, pricing)?;
    if let Some((cache_dir, pricing_fp)) = cache_target {
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            ConvCostCacheEntry {
                cli_dir: root_path_key(cli_dir),
                conversation_id: conversation_id.to_ascii_lowercase(),
                parent_fp,
                summaries_fp,
                descendants,
                pricing_fp,
                cost: CachedConversationCost {
                    usd: cost.usd,
                    subagent_usd: cost.subagent_usd,
                    unpriced_turns: cost.unpriced_turns,
                },
            },
            CONV_COST_CACHE_MAX_ENTRIES,
        );
    }
    Some(cost)
}

/// Estimates the subtotal for priced usage in a SQLite conversation and any descendant subagents.
#[cfg(test)]
fn estimate_conversation_cost_usd(
    cli_dir: &Path,
    conversation_id: &str,
    pricing: &PricingCatalog,
) -> Option<ConversationCost> {
    estimate_conversation_cost_usd_with_parent_conn(cli_dir, conversation_id, pricing, None, None)
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

    #[test]
    fn transcript_full_fallback_counts_usage_when_compact_transcript_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Standard depth-0 layout: .system_generated/logs/transcript_full.jsonl only.
        let std_full = home.join(
            ".gemini/antigravity-cli/brain/aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa/.system_generated/logs/transcript_full.jsonl",
        );
        fs::create_dir_all(std_full.parent().unwrap()).unwrap();
        fs::write(
            &std_full,
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":1,\"model\":\"full-std\",\"input_tokens\":1000,\"output_tokens\":200}\n",
        )
        .unwrap();

        // Direct depth-0 layout: transcript_full.jsonl only.
        let direct_full = home.join(
            ".gemini/antigravity/brain/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb/transcript_full.jsonl",
        );
        fs::create_dir_all(direct_full.parent().unwrap()).unwrap();
        fs::write(
            &direct_full,
            "{\"created_at\":\"2026-11-18T10:30:00Z\",\"step_index\":1,\"model\":\"full-direct\",\"input_tokens\":500,\"output_tokens\":50}\n",
        )
        .unwrap();

        // Recursive nested layout: nested/sub/transcript_full.jsonl only.
        let nested_full =
            home.join(".gemini/antigravity-cli/brain/group/sub/transcript_full.jsonl");
        fs::create_dir_all(nested_full.parent().unwrap()).unwrap();
        fs::write(
            &nested_full,
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":1,\"model\":\"full-nested\",\"input_tokens\":300,\"output_tokens\":30}\n",
        )
        .unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 3);
        assert_eq!(report.totals.input_tokens, 1800);
        assert_eq!(report.totals.output_tokens, 280);
        assert_eq!(report.totals.total_tokens, 2080);
    }

    #[test]
    fn transcript_full_is_skipped_when_compact_transcript_exists() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Depth-0 fast path: both transcript.jsonl and transcript_full.jsonl exist.
        // Distinct step_index and token counts ensure scanning transcript_full.jsonl would inflate totals.
        let std_dir = home.join(
            ".gemini/antigravity-cli/brain/cccccccc-cccc-4ccc-8ccc-cccccccccccc/.system_generated/logs",
        );
        fs::create_dir_all(&std_dir).unwrap();
        fs::write(
            std_dir.join("transcript.jsonl"),
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":1,\"model\":\"compact-std\",\"input_tokens\":400,\"output_tokens\":40}\n",
        )
        .unwrap();
        fs::write(
            std_dir.join("transcript_full.jsonl"),
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":99,\"model\":\"compact-std\",\"input_tokens\":9000,\"output_tokens\":900}\n",
        )
        .unwrap();

        // Recursive nested path: both transcript.jsonl and transcript_full.jsonl in same parent dir.
        let nested_dir = home.join(".gemini/antigravity/brain/group/nested");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(
            nested_dir.join("transcript.jsonl"),
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":2,\"model\":\"compact-nested\",\"input_tokens\":600,\"output_tokens\":60}\n",
        )
        .unwrap();
        fs::write(
            nested_dir.join("transcript_full.jsonl"),
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":88,\"model\":\"compact-nested\",\"input_tokens\":8000,\"output_tokens\":800}\n",
        )
        .unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.input_tokens, 1000);
        assert_eq!(report.totals.output_tokens, 100);
        assert_eq!(report.totals.total_tokens, 1100);
    }

    #[test]
    fn empty_compact_transcript_does_not_shadow_populated_transcript_full() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();

        // Depth-0 fast path: 0-byte transcript.jsonl alongside populated transcript_full.jsonl.
        let std_dir = home.join(
            ".gemini/antigravity-cli/brain/dddddddd-dddd-4ddd-8ddd-dddddddddddd/.system_generated/logs",
        );
        fs::create_dir_all(&std_dir).unwrap();
        fs::write(std_dir.join("transcript.jsonl"), b"").unwrap();
        fs::write(
            std_dir.join("transcript_full.jsonl"),
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"step_index\":1,\"model\":\"full-after-empty-std\",\"input_tokens\":400,\"output_tokens\":40}\n",
        )
        .unwrap();

        // Recursive nested path: 0-byte transcript.jsonl alongside populated transcript_full.jsonl.
        let nested_dir = home.join(".gemini/antigravity/brain/group/nested");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(nested_dir.join("transcript.jsonl"), b"").unwrap();
        fs::write(
            nested_dir.join("transcript_full.jsonl"),
            "{\"created_at\":\"2026-11-18T11:00:00Z\",\"step_index\":2,\"model\":\"full-after-empty-nested\",\"input_tokens\":600,\"output_tokens\":60}\n",
        )
        .unwrap();

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.input_tokens, 1000);
        assert_eq!(report.totals.output_tokens, 100);
        assert_eq!(report.totals.total_tokens, 1100);
    }

    #[test]
    fn depth0_fast_path_queues_only_first_non_empty_transcript_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let brain = home.join(".gemini/antigravity-cli/brain");

        // Session with both `.system_generated/logs/transcript.jsonl` and `<session>/transcript.jsonl`.
        // Lines omit `responseId` and the second file has an extra line so double-queuing would inflate totals.
        let compact_session = brain.join("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee");
        let std_compact = compact_session.join(".system_generated/logs/transcript.jsonl");
        fs::create_dir_all(std_compact.parent().unwrap()).unwrap();
        fs::write(
            &std_compact,
            "{\"created_at\":\"2026-11-18T10:00:00Z\",\"model\":\"compact-primary\",\"input_tokens\":400,\"output_tokens\":40}\n",
        )
        .unwrap();
        fs::write(
            compact_session.join("transcript.jsonl"),
            concat!(
                "{\"created_at\":\"2026-11-18T10:00:00Z\",\"model\":\"compact-dup\",\"input_tokens\":400,\"output_tokens\":40}\n",
                "{\"created_at\":\"2026-11-18T10:01:00Z\",\"model\":\"compact-dup\",\"input_tokens\":9000,\"output_tokens\":900}\n",
            ),
        )
        .unwrap();

        // Session with both `.system_generated/logs/transcript_full.jsonl` and `<session>/transcript_full.jsonl`.
        let full_session = brain.join("ffffffff-ffff-4fff-8fff-ffffffffffff");
        let std_full = full_session.join(".system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(std_full.parent().unwrap()).unwrap();
        fs::write(
            &std_full,
            "{\"created_at\":\"2026-11-18T10:30:00Z\",\"model\":\"full-primary\",\"input_tokens\":300,\"output_tokens\":30}\n",
        )
        .unwrap();
        fs::write(
            full_session.join("transcript_full.jsonl"),
            concat!(
                "{\"created_at\":\"2026-11-18T10:30:00Z\",\"model\":\"full-dup\",\"input_tokens\":300,\"output_tokens\":30}\n",
                "{\"created_at\":\"2026-11-18T10:31:00Z\",\"model\":\"full-dup\",\"input_tokens\":8000,\"output_tokens\":800}\n",
            ),
        )
        .unwrap();

        let mut visited_dirs = HashSet::new();
        let mut seen_files = HashSet::new();
        let mut jobs = Vec::new();
        collect_transcript_jobs_in_brain(
            &brain,
            "cli",
            0,
            &mut visited_dirs,
            &mut seen_files,
            &mut jobs,
        );
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].path, std_compact);
        assert_eq!(jobs[1].path, std_full);

        let now_secs = parse_rfc3339_unix_secs("2026-11-18T12:00:00Z").unwrap() as u64;
        let report = collect_stats_with_tz(home, None, now_secs, &TimeZoneSpec::utc());
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.totals.input_tokens, 700);
        assert_eq!(report.totals.output_tokens, 70);
        assert_eq!(report.totals.total_tokens, 770);
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

    #[test]
    fn project_sqlite_infers_unambiguous_response_model_without_crossing_ambiguous_or_explicit_tiers()
    {
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
            mk_turn(
                "shared-worker-id",
                Some("Gemini 4 Hydrogen (Orchestrator)"),
            ),
            mk_turn("shared-worker-id", Some("Gemini 5 Plasma (Orchestrator)")),
            mk_turn("shared-worker-id", Some("Gemini Flash")),
        ];

        let (_, events, _) = project_sqlite_conversations(
            [("cli", "11111111-1111-4111-8111-111111111111", turns.as_slice())],
            0,
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

    fn test_pricing_catalog(cache_dir: &Path) -> PricingCatalog {
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
        use crate::protobuf::{
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
    fn project_sqlite_union_find_transitively_dedups_three_records_across_roots_and_blocks_transcript(
    ) {
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

        let (stats, events, covered) = project_sqlite_conversations(
            [
                ("desktop", "11111111-1111-4111-8111-111111111111", desk_a.as_slice()),
                ("cli", "22222222-2222-4222-8222-222222222222", cli_b.as_slice()),
                ("desktop", "33333333-3333-4333-8333-333333333333", desk_c.as_slice()),
            ],
            0,
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
        assert!(covered.response_ids.contains("r-shared"));
        assert!(covered.response_ids.contains("c-shared"));
        assert!(covered.response_ids.contains("m-shared"));

        // Transcript fallback in an unrelated session skips any explicit ID already covered by SQLite.
        let cli_root = dir.path().join("cli");
        let transcript = cli_root.join("brain/uncovered-session/transcript.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(
            &transcript,
            concat!(
                r#"{"created_at":"2026-11-18T10:00:00Z","model":"m1","input_tokens":9000,"output_tokens":90,"responseId":"r-shared"}"#,
                "\n",
                r#"{"created_at":"2026-11-18T10:01:00Z","model":"m1","input_tokens":8000,"output_tokens":80,"callId":"c-shared"}"#,
                "\n",
                r#"{"created_at":"2026-11-18T10:02:00Z","model":"m1","input_tokens":7000,"output_tokens":70,"messageId":"m-shared"}"#,
                "\n",
                r#"{"created_at":"2026-11-18T10:03:00Z","model":"m-kept","input_tokens":300,"output_tokens":30,"responseId":"r-unique"}"#,
                "\n",
            ),
        )
        .unwrap();
        let fallback = scan_transcripts(
            &[("cli", cli_root)],
            &TimeZoneSpec::utc(),
            &pricing,
            &covered,
        );
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].model_group, "m-kept");
        assert_eq!(fallback[0].input_tokens, 300);
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

        let (_, events, _) = project_sqlite_conversations(
            [("cli", "11111111-1111-4111-8111-111111111111", turns.as_slice())],
            0,
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

    #[test]
    fn estimate_conversation_cost_uses_turn_pricer_for_compaction_and_worker_inference() {
        use crate::protobuf::{encode_gen_blob, encode_step_blob, GenBlobSpec, StepBlobSpec, UsageSpec};

        let dir = tempfile::tempdir().unwrap();
        let cli_dir = dir.path().join(".gemini/antigravity-cli");
        let pricing = test_pricing_catalog(&dir.path().join("cache"));
        let parent = "11111111-1111-4111-8111-111111111111";
        let child = "22222222-2222-4222-8222-222222222222";

        // Parent DB: 1M Hydrogen turn in gen_metadata + 1M bare compaction turn in steps.metadata.f9.
        let parent_db = cli_dir.join("conversations").join(format!("{parent}.db"));
        fs::create_dir_all(parent_db.parent().unwrap()).unwrap();
        let pconn = Connection::open(&parent_db).unwrap();
        pconn
            .execute_batch(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
                 CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);",
            )
            .unwrap();
        pconn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1669,
                    resp_model: Some("gemini-flash-worker-agy"),
                    disp_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
                    input_tok: 1_000_000,
                    ..Default::default()
                })],
            )
            .unwrap();
        pconn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                [encode_step_blob(StepBlobSpec {
                    created_at_secs: Some(1_795_000_000),
                    usage: Some(UsageSpec {
                        model_enum: 1669,
                        input_tok: 1_000_000,
                        ..Default::default()
                    }),
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(pconn);

        // Child DB: 1M turn with legacy generic display name "Gemini Flash" on same worker routing ID.
        seed_cost_db(
            &cli_dir,
            child,
            1,
            encode_gen_blob(GenBlobSpec {
                step_indices: vec![1],
                model_enum: 0,
                resp_model: Some("gemini-flash-worker-agy"),
                disp_name: Some("Gemini Flash"),
                input_tok: 1_000_000,
                ..Default::default()
            }),
        );

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
        drop(summaries);

        let est = estimate_conversation_cost_usd(&cli_dir, parent, &pricing).unwrap();
        // Parent: $1.50 (main) + $1.50 (compaction) = $3.00; child: $1.50 -> total $4.50, sub $1.50.
        assert_eq!(est.usd, Some(4.5));
        assert_eq!(est.subagent_usd, Some(1.5));
        assert_eq!(est.unpriced_turns, 0);
    }

    #[cfg(unix)]
    #[test]
    fn db_file_fingerprint_tracks_ctime_and_zeros_idle_wal_ctime() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.db-wal");
        fs::write(&db_path, b"sqlite-bytes").unwrap();

        let fp_no_wal = db_file_fingerprint(&db_path).unwrap();
        assert!(fp_no_wal.db_ctime_secs > 0);
        assert_eq!(fp_no_wal.wal_ctime_secs, 0);
        assert_eq!(fp_no_wal.wal_ctime_nanos, 0);

        // 0-byte WAL must keep wal_ctime zeroed.
        fs::write(&wal_path, b"").unwrap();
        let fp_empty_wal = db_file_fingerprint(&db_path).unwrap();
        assert_eq!(fp_empty_wal, fp_no_wal);

        // Non-empty WAL records non-zero wal_ctime and changes the fingerprint.
        fs::write(&wal_path, b"wal-frame").unwrap();
        let fp_active_wal = db_file_fingerprint(&db_path).unwrap();
        assert!(fp_active_wal.wal_ctime_secs > 0);
        assert_ne!(fp_active_wal, fp_no_wal);

        // A ctime-only difference changes DbFileFingerprint equality.
        let mut altered = fp_no_wal;
        altered.db_ctime_secs = altered.db_ctime_secs.saturating_sub(1);
        assert_ne!(altered, fp_no_wal);
    }

    #[test]
    fn conv_cost_cache_bounds_lru_entries_and_promotes_updated_key() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path();
        let mk_entry = |id: &str, usd: f64| ConvCostCacheEntry {
            cli_dir: "/isolated/cli".to_string(),
            conversation_id: id.to_string(),
            parent_fp: None,
            summaries_fp: None,
            descendants: Vec::new(),
            pricing_fp: PricingFileFingerprint::default(),
            cost: CachedConversationCost {
                usd: Some(usd),
                subagent_usd: None,
                unpriced_turns: 0,
            },
        };

        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("11111111-1111-4111-8111-111111111111", 1.0),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("22222222-2222-4222-8222-222222222222", 2.0),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("33333333-3333-4333-8333-333333333333", 3.0),
            2,
        );

        let after_evict = read_conv_cost_cache_file(cache_dir).unwrap();
        assert_eq!(after_evict.entries.len(), 2);
        assert_eq!(
            after_evict.entries[0].conversation_id,
            "33333333-3333-4333-8333-333333333333"
        );
        assert_eq!(
            after_evict.entries[1].conversation_id,
            "22222222-2222-4222-8222-222222222222"
        );

        // Updating entry 2 promotes it to MRU so entry 3 is evicted next.
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("22222222-2222-4222-8222-222222222222", 2.5),
            2,
        );
        write_conv_cost_cache_entry_with_limit(
            cache_dir,
            mk_entry("44444444-4444-4444-8444-444444444444", 4.0),
            2,
        );

        let after_promote = read_conv_cost_cache_file(cache_dir).unwrap();
        assert_eq!(after_promote.entries.len(), 2);
        assert_eq!(
            after_promote.entries[0].conversation_id,
            "44444444-4444-4444-8444-444444444444"
        );
        assert_eq!(
            after_promote.entries[1].conversation_id,
            "22222222-2222-4222-8222-222222222222"
        );
        assert_eq!(after_promote.entries[1].cost.usd, Some(2.5));
    }
}
