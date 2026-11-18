//! Read-only SQLite `gen_metadata` and `transcript.jsonl` scanners.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use super::pricing::{round_usd_6, PricingCatalog};
use super::tz::TimeZoneSpec;
use super::{compute_median, SqliteStats};
use crate::db::is_valid_uuid;
use crate::protobuf::{
    extract_gen_metadata_record, extract_step_created_at_secs, MAX_TTFT_SECS, MAX_TURN_SPEED_TPS,
    MIN_TURN_DECODE_SECS,
};
use crate::timeutil::parse_rfc3339_unix_secs;

const MAX_TRANSCRIPT_SCAN_DEPTH: usize = 4;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 25;

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
pub fn derive_model_name(
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

fn has_non_null_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter()
        .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
}

pub(super) fn parse_duration_secs(v: Option<&Value>) -> Option<f64> {
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

pub(super) fn extract_canonical_event_key(
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

#[derive(Debug, Clone)]
pub(super) struct UsageEvent {
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

fn is_skipped_dir_name(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
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
        if is_skipped_dir_name(&name_str) {
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
        if has_non_null_field(
            &obj,
            &["input_tokens", "output_tokens", "cache_read_tokens"],
        ) {
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

    let (pricing_key, cost_usd) = if let Some(usd) = native_cost {
        (Some("native".to_string()), Some(round_usd_6(usd)))
    } else {
        let mut candidates = Vec::with_capacity(3);
        if let Some(r) = resp {
            candidates.push(r);
        }
        if let Some(d) = disp {
            candidates.push(d);
        }
        candidates.push(model_group.as_str());
        match pricing.estimate_cost(
            &candidates,
            input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
        ) {
            Some((k, usd)) => (Some(k), Some(usd)),
            None => (None, None),
        }
    };

    let streaming_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("streamingDuration")))
        .or_else(|| parse_duration_secs(obj.get("streamingSec")));
    let decode_speed = streaming_secs.and_then(|secs| {
        if secs < MIN_TURN_DECODE_SECS || output_tokens == 0 {
            return None;
        }
        let speed = (output_tokens as f64) / secs;
        (speed.is_finite() && speed > 0.0 && speed < MAX_TURN_SPEED_TPS).then_some(speed)
    });

    let ttft_secs = cm_obj
        .and_then(|cm| parse_duration_secs(cm.get("timeToFirstToken")))
        .or_else(|| parse_duration_secs(obj.get("ttftSec")))
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);

    let event_key = extract_canonical_event_key(&obj, job.source, &job.cascade_hint, line_idx);

    Some((
        event_key,
        UsageEvent {
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

struct DbTarget {
    source: &'static str,
    uuid: String,
    path: PathBuf,
}

struct SingleDbScan {
    source: &'static str,
    uuid: String,
    completed_turns: u64,
    total_output_tokens: u64,
    speed_samples: Vec<f64>,
    ttft_samples: Vec<f64>,
    events: Vec<UsageEvent>,
}

fn scan_single_sqlite_db(
    target: &DbTarget,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> Option<SingleDbScan> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db_mtime_secs = fs::metadata(&target.path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    let conn = Connection::open_with_flags(&target.path, flags).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS));

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

    let mut out = SingleDbScan {
        source: target.source,
        uuid: target.uuid.clone(),
        completed_turns: 0,
        total_output_tokens: 0,
        speed_samples: Vec::new(),
        ttft_samples: Vec::new(),
        events: Vec::new(),
    };

    while let Ok(Some(row)) = rows.next() {
        let Ok(blob) = row.get::<_, Vec<u8>>(0) else {
            continue;
        };
        let Some(rec) = extract_gen_metadata_record(&blob) else {
            continue;
        };

        let decode_speed = rec.decode_speed_tps();
        if let Some(speed) = decode_speed {
            out.completed_turns += 1;
            out.total_output_tokens = out
                .total_output_tokens
                .saturating_add(rec.output_tokens);
            out.speed_samples.push(speed);
            if let Some(ttft) = rec.ttft_secs {
                if ttft.is_finite() && ttft > 0.0 {
                    out.ttft_samples.push(ttft);
                }
            }
        }

        let total_tokens = rec
            .input_tokens
            .saturating_add(rec.cache_read_tokens)
            .saturating_add(rec.cache_write_tokens)
            .saturating_add(rec.output_tokens);
        if total_tokens == 0 {
            continue;
        }

        let Some(ts_secs) = rec
            .first_step_index
            .and_then(|step_idx| step_timestamps.get(&step_idx).copied())
            .or(rec.chat_start_created_at_secs)
            .or(db_mtime_secs)
        else {
            continue;
        };

        let (local_day, local_date) = tz.local_day_and_date(ts_secs);
        let model_group =
            derive_model_name(rec.model_display_name, rec.response_model, rec.model_enum);

        let (pricing_key, cost_usd) = if let Some(usd) = rec.model_cost_usd {
            (Some("native".to_string()), Some(round_usd_6(usd)))
        } else {
            let mut candidates = Vec::with_capacity(3);
            if let Some(r) = rec.response_model {
                candidates.push(r);
            }
            if let Some(d) = rec.model_display_name {
                candidates.push(d);
            }
            candidates.push(model_group.as_str());
            match pricing.estimate_cost(
                &candidates,
                rec.input_tokens,
                rec.cache_read_tokens,
                rec.cache_write_tokens,
                rec.output_tokens,
            ) {
                Some((k, usd)) => (Some(k), Some(usd)),
                None => (None, None),
            }
        };

        out.events.push(UsageEvent {
            local_day,
            local_date,
            model_group,
            pricing_key,
            input_tokens: rec.input_tokens,
            cache_read_tokens: rec.cache_read_tokens,
            cache_write_tokens: rec.cache_write_tokens,
            output_tokens: rec.output_tokens,
            thinking_tokens: rec.thinking_output_tokens,
            total_tokens,
            cost_usd,
            decode_speed,
            ttft_secs: rec.ttft_secs,
        });
    }

    Some(out)
}

pub(super) fn scan_sqlite_conversations(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
) -> (
    SqliteStats,
    Vec<UsageEvent>,
    HashSet<(&'static str, String)>,
) {
    let mut seen_dbs = HashSet::new();
    let mut targets = Vec::new();
    let mut covered = HashSet::new();

    for (source, root) in roots {
        let conv_dir = root.join("conversations");
        let Ok(entries) = fs::read_dir(&conv_dir) else {
            continue;
        };
        let mut sorted: Vec<_> = entries.filter_map(Result::ok).collect();
        sorted.sort_by_key(|e| e.file_name());
        for entry in sorted {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.ends_with("-wal")
                || name_str.ends_with("-shm")
                || name_str.ends_with("-journal")
            {
                continue;
            }
            let Some(stem) = name_str.strip_suffix(".db") else {
                continue;
            };
            if !is_valid_uuid(stem) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let uuid_lower = stem.to_ascii_lowercase();
            let real = canonical_or_self(&path);
            if seen_dbs.insert(real) {
                targets.push(DbTarget {
                    source,
                    uuid: uuid_lower,
                    path,
                });
            }
        }
    }

    let mut stats = SqliteStats::default();
    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut events = Vec::new();

    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 8);
    let results: Vec<SingleDbScan> = if targets.len() <= 8 || workers == 1 {
        targets
            .iter()
            .filter_map(|t| scan_single_sqlite_db(t, tz, pricing))
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
                            .filter_map(|t| scan_single_sqlite_db(t, tz, pricing))
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
    };

    for res in results {
        covered.insert((res.source, res.uuid));
        stats.db_files += 1;
        stats.completed_turns += res.completed_turns;
        stats.total_output_tokens = stats
            .total_output_tokens
            .saturating_add(res.total_output_tokens);
        speed_samples.extend(res.speed_samples);
        ttft_samples.extend(res.ttft_samples);
        events.extend(res.events);
    }

    stats.ttft_samples = ttft_samples.len();
    stats.median_tok_per_sec = compute_median(&mut speed_samples);
    stats.median_ttft_secs = compute_median(&mut ttft_samples);
    (stats, events, covered)
}
