//! Single-conversation stats reporting and historical conversation catalog.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::{
    db_file_fingerprint, deduplicate_parent_priority_turns, discover_sqlite_db_targets,
    extract_sqlite_db_turns, merge_raw_turn, valid_decode_speed, valid_ttft_secs,
    DbFileFingerprint, RawTurn, TurnPricer,
};
use crate::agy::db::{
    compute_median, is_safe_sqlite_file, is_valid_uuid, open_readonly_db,
    resolve_conversation_db_path, safe_sqlite_metadata,
};
use crate::stats::metric::{cache_hit_pct, StatsMetric};
use crate::stats::pricing::{round_usd_6, PricingCatalog};
use crate::stats::{
    finalize_models, ConversationListItem, ConversationStatsReport, ConversationTurnStats,
    ModelAccumulator, PeriodSummary, SqliteStats, SubagentConversationStats, UsageDelta,
};
use crate::sys::fmt::sanitize_inline_text;
use crate::sys::fs::modified_timestamp;
use crate::sys::time::TimeZoneSpec;

/// Lightweight metadata-only fingerprint for live-watching a single conversation and its subagents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::stats) struct ConversationWatchFingerprint {
    root_idx: Option<usize>,
    conv_dir_mtime: Option<(i64, u32)>,
    summaries_fp: Option<DbFileFingerprint>,
    main_fp: Option<DbFileFingerprint>,
    subagent_fps: Vec<(String, Option<DbFileFingerprint>)>,
}

/// Computes a zero-SQLite-open change fingerprint for a conversation, its `conversation_summaries.db`,
/// its `conversations/` directory, and all currently known descendant subagent DBs.
pub(in crate::stats) fn conversation_watch_fingerprint(
    roots: &[(&'static str, PathBuf)],
    conv_id: &str,
    known_subagent_ids: &[String],
) -> ConversationWatchFingerprint {
    for (idx, (_, root)) in roots.iter().enumerate() {
        let Some(db_path) = resolve_conversation_db_path(root, conv_id) else {
            continue;
        };
        let conv_dir_mtime = fs::symlink_metadata(root.join("conversations"))
            .ok()
            .map(|m| modified_timestamp(&m));
        let summaries_fp = db_file_fingerprint(&root.join("conversation_summaries.db"));
        let main_fp = db_file_fingerprint(&db_path);
        let mut subagent_fps: Vec<(String, Option<DbFileFingerprint>)> = known_subagent_ids
            .iter()
            .map(|sub_id| {
                let fp = resolve_conversation_db_path(root, sub_id)
                    .and_then(|p| db_file_fingerprint(&p));
                (sub_id.clone(), fp)
            })
            .collect();
        subagent_fps.sort_by(|a, b| a.0.cmp(&b.0));
        return ConversationWatchFingerprint {
            root_idx: Some(idx),
            conv_dir_mtime,
            summaries_fp,
            main_fp,
            subagent_fps,
        };
    }

    ConversationWatchFingerprint {
        root_idx: None,
        conv_dir_mtime: None,
        summaries_fp: None,
        main_fp: None,
        subagent_fps: Vec::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootWatchFingerprint {
    dir_mtime: Option<(i64, u32)>,
    summaries_fp: Option<DbFileFingerprint>,
}

/// Lightweight metadata-only fingerprint for live-watching the historical conversations list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::stats) struct ConversationsListFingerprint {
    roots: Vec<RootWatchFingerprint>,
    dbs: Vec<(String, Option<i64>, u64)>,
}

/// Computes a zero-SQLite-open change fingerprint across all AGY roots' conversation lists.
pub(in crate::stats) fn conversations_list_fingerprint(
    roots: &[(&'static str, PathBuf)],
) -> ConversationsListFingerprint {
    let root_fps = roots
        .iter()
        .map(|(_, root)| {
            let dir_mtime = fs::symlink_metadata(root.join("conversations"))
                .ok()
                .map(|m| modified_timestamp(&m));
            let summaries_fp = db_file_fingerprint(&root.join("conversation_summaries.db"));
            RootWatchFingerprint {
                dir_mtime,
                summaries_fp,
            }
        })
        .collect();
    // `list_conversations` reads only `conversation_summaries.db` plus each DB's mtime & size.
    let mut dbs: Vec<(String, Option<i64>, u64)> = discover_sqlite_db_targets(roots)
        .into_iter()
        .map(|t| (t.conversation_id, t.db_mtime_secs, t.db_size_bytes))
        .collect();
    dbs.sort_by(|a, b| a.0.cmp(&b.0));
    ConversationsListFingerprint {
        roots: root_fps,
        dbs,
    }
}

#[derive(Debug, Clone)]
struct ResolvedConversationTarget {
    source: &'static str,
    root: PathBuf,
    conversation_id: String,
    db_path: PathBuf,
    db_mtime_secs: Option<i64>,
}

fn resolve_conversation_target(
    roots: &[(&'static str, PathBuf)],
    query: &str,
) -> io::Result<ResolvedConversationTarget> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "conversation ID cannot be empty",
        ));
    }
    if !trimmed.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid conversation ID '{trimmed}': expected a UUID or hex prefix"),
        ));
    }

    if is_valid_uuid(trimmed) {
        let canonical_id = trimmed.to_ascii_lowercase();
        for (source, root) in roots {
            if let Some(db_path) = resolve_conversation_db_path(root, trimmed) {
                let db_mtime_secs = safe_sqlite_metadata(&db_path)
                    .map(|m| modified_timestamp(&m).0)
                    .filter(|&secs| secs > 0);
                return Ok(ResolvedConversationTarget {
                    source,
                    root: root.clone(),
                    conversation_id: canonical_id,
                    db_path,
                    db_mtime_secs,
                });
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("conversation '{trimmed}' not found"),
        ));
    }

    if trimmed.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "conversation ID prefix '{trimmed}' is too short (use at least 4 hex characters or a full UUID)"
            ),
        ));
    }

    let prefix = trimmed.to_ascii_lowercase();
    let mut matches: Vec<ResolvedConversationTarget> = discover_sqlite_db_targets(roots)
        .into_iter()
        .filter(|t| t.conversation_id.starts_with(&prefix))
        .map(|t| ResolvedConversationTarget {
            source: t.source,
            root: t.root,
            conversation_id: t.conversation_id,
            db_path: t.path,
            db_mtime_secs: t.db_mtime_secs,
        })
        .collect();

    match matches.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("conversation '{trimmed}' not found"),
        )),
        1 => Ok(matches.pop().unwrap()),
        count => {
            matches.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
            let preview: Vec<&str> = matches
                .iter()
                .take(5)
                .map(|m| m.conversation_id.as_str())
                .collect();
            let suffix = if count > preview.len() { ", ..." } else { "" };
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "ambiguous conversation prefix '{trimmed}' matches {count} conversations: {}{suffix}",
                    preview.join(", ")
                ),
            ))
        }
    }
}

#[derive(Debug, Clone, Default)]
struct IndexedSummaryRow {
    title: Option<String>,
    preview: Option<String>,
    workspace: Option<String>,
    agent_name: Option<String>,
    parent_conversation_id: Option<String>,
    step_count: Option<u64>,
    last_modified_secs: Option<i64>,
}

impl IndexedSummaryRow {
    fn display_title(&self) -> Option<String> {
        self.title.clone().or_else(|| self.preview.clone())
    }
}

fn parse_summary_timestamp_secs(raw: Option<String>) -> Option<i64> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = if trimmed.as_bytes().get(10) == Some(&b' ') {
        format!("{}T{}", &trimmed[..10], &trimmed[11..])
    } else {
        trimmed.to_string()
    };
    normalized
        .parse::<jiff::Timestamp>()
        .ok()
        .map(jiff::Timestamp::as_second)
}

fn extract_workspace_basename(raw_uris: Option<String>) -> Option<String> {
    let raw = raw_uris?;
    let uris = serde_json::from_str::<Vec<String>>(&raw).ok()?;
    for uri in uris {
        let trimmed = uri.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        let path_part = trimmed.strip_prefix("file://").unwrap_or(trimmed);
        if let Some(name) = path_part.rsplit('/').find(|seg| !seg.is_empty()) {
            return sanitize_summary_text(Some(name.to_string()));
        }
    }
    None
}

fn load_conversation_summaries(
    root: &Path,
    only_ids: Option<&HashSet<String>>,
) -> HashMap<String, IndexedSummaryRow> {
    let mut out = HashMap::new();
    let summaries_path = root.join("conversation_summaries.db");
    if !is_safe_sqlite_file(&summaries_path) {
        return out;
    }
    let Ok(conn) = open_readonly_db(&summaries_path) else {
        return out;
    };

    // Probe schema columns so older/minimal conversation_summaries tables remain compatible.
    let mut cols = HashSet::new();
    if let Ok(mut pragma) = conn.prepare("PRAGMA table_info(conversation_summaries)") {
        if let Ok(mut rows) = pragma.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(name) = row.get::<_, String>(1) {
                    cols.insert(name);
                }
            }
        }
    }
    if !cols.contains("conversation_id") {
        return out;
    }

    let col_or = |name: &str, fallback: &str| -> String {
        if cols.contains(name) {
            name.to_string()
        } else {
            fallback.to_string()
        }
    };
    let sql = format!(
        "SELECT conversation_id, {}, {}, {}, {}, {}, {}, {} FROM conversation_summaries",
        col_or("title", "''"),
        col_or("preview", "''"),
        col_or("workspace_uris", "''"),
        col_or("agent_name", "''"),
        col_or("parent_conversation_id", "''"),
        col_or("step_count", "NULL"),
        col_or("last_modified_time", "''"),
    );

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return out;
    };
    while let Ok(Some(row)) = rows.next() {
        let Ok(id) = row.get::<_, String>(0) else {
            continue;
        };
        if !is_valid_uuid(&id) {
            continue;
        }
        let lower_id = id.to_ascii_lowercase();
        if only_ids.is_some_and(|wanted| !wanted.contains(&lower_id)) {
            continue;
        }
        let step_count = row
            .get::<_, Option<i64>>(6)
            .ok()
            .flatten()
            .filter(|&v| v > 0)
            .map(|v| v as u64);
        out.insert(
            lower_id,
            IndexedSummaryRow {
                title: sanitize_summary_text(row.get::<_, Option<String>>(1).ok().flatten()),
                preview: sanitize_summary_text(row.get::<_, Option<String>>(2).ok().flatten()),
                workspace: extract_workspace_basename(
                    row.get::<_, Option<String>>(3).ok().flatten(),
                ),
                agent_name: sanitize_summary_text(row.get::<_, Option<String>>(4).ok().flatten()),
                parent_conversation_id: sanitize_summary_text(
                    row.get::<_, Option<String>>(5).ok().flatten(),
                )
                .filter(|p| is_valid_uuid(p))
                .map(|p| p.to_ascii_lowercase()),
                step_count,
                last_modified_secs: parse_summary_timestamp_secs(
                    row.get::<_, Option<String>>(7).ok().flatten(),
                ),
            },
        );
    }
    out
}

/// Lists all discoverable historical conversations across `roots`, enriched from
/// `conversation_summaries.db` and sorted newest first (without opening individual turn DBs).
pub(in crate::stats) fn list_conversations(
    roots: &[(&'static str, PathBuf)],
    tz: &TimeZoneSpec,
) -> Vec<ConversationListItem> {
    let mut summaries_by_root: HashMap<PathBuf, HashMap<String, IndexedSummaryRow>> =
        HashMap::new();
    let mut parent_links: HashMap<String, String> = HashMap::new();
    let mut items = Vec::new();

    for target in discover_sqlite_db_targets(roots) {
        let summaries = summaries_by_root
            .entry(target.root.clone())
            .or_insert_with(|| load_conversation_summaries(&target.root, None));
        let indexed = summaries.get(&target.conversation_id);
        let title = indexed.and_then(IndexedSummaryRow::display_title);
        let parent_id = indexed.and_then(|s| s.parent_conversation_id.clone());
        if let Some(parent) = parent_id.as_ref() {
            parent_links.insert(target.conversation_id.clone(), parent.clone());
        }
        let last_modified_secs = indexed
            .and_then(|s| s.last_modified_secs)
            .or(target.db_mtime_secs)
            .unwrap_or(0);
        let last_modified_local = if last_modified_secs > 0 {
            let dt = tz.local_datetime_string(last_modified_secs);
            dt.get(..16).unwrap_or(&dt).to_string()
        } else {
            String::new()
        };

        items.push(ConversationListItem {
            conversation_id: target.conversation_id,
            source: target.source.to_string(),
            title,
            workspace: indexed.and_then(|s| s.workspace.clone()),
            agent_name: indexed.and_then(|s| s.agent_name.clone()),
            parent_conversation_id: parent_id,
            subagent_count: 0,
            step_count: indexed.and_then(|s| s.step_count),
            last_modified_secs,
            last_modified_local,
            db_size_bytes: target.db_size_bytes,
        });
    }

    // Attribute each child DB to every ancestor (depth <= 8, cycle-safe) so root rows display total subagent count.
    let mut sub_counts: HashMap<String, usize> = HashMap::new();
    for item in &items {
        let mut visited: HashSet<&str> = HashSet::from([item.conversation_id.as_str()]);
        let mut cursor = item.parent_conversation_id.as_deref();
        let mut depth = 0usize;
        while let Some(parent) = cursor {
            if depth >= 8 || !visited.insert(parent) {
                break;
            }
            *sub_counts.entry(parent.to_string()).or_insert(0) += 1;
            cursor = parent_links.get(parent).map(String::as_str);
            depth += 1;
        }
    }
    for item in &mut items {
        if let Some(&cnt) = sub_counts.get(&item.conversation_id) {
            item.subagent_count = cnt;
        }
    }

    items.sort_by(|a, b| {
        b.last_modified_secs
            .cmp(&a.last_modified_secs)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });
    items
}

fn sanitize_summary_text(raw: Option<String>) -> Option<String> {
    let cleaned = sanitize_inline_text(&raw?);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Walks descendant conversation IDs in memory (BFS, depth <= 8, cycle-safe) from a
/// preloaded `conversation_summaries.db` map so callers never open the summary DB twice.
fn descendant_ids_from_summaries(
    summaries: &HashMap<String, IndexedSummaryRow>,
    root_id: &str,
) -> Vec<String> {
    let root_lower = root_id.to_ascii_lowercase();
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, row) in summaries {
        if let Some(parent) = row.parent_conversation_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(id.as_str());
        }
    }
    let mut seen: HashSet<&str> = HashSet::from([root_lower.as_str()]);
    let mut frontier: Vec<(&str, usize)> = vec![(root_lower.as_str(), 0)];
    let mut out = Vec::new();
    let mut idx = 0usize;
    while idx < frontier.len() {
        let (cur, depth) = frontier[idx];
        idx += 1;
        if depth >= 8 {
            continue;
        }
        if let Some(children) = children_by_parent.get(cur) {
            for &child in children {
                if seen.insert(child) {
                    out.push(child.to_string());
                    frontier.push((child, depth + 1));
                }
            }
        }
    }
    out.sort();
    out
}

struct ConversationTurnSlot {
    conversation_id: String,
    is_subagent: bool,
    order: usize,
    turn: RawTurn,
}

#[derive(Default)]
struct SubagentAccumulator {
    summary: PeriodSummary,
    speeds: Vec<f64>,
    ttfts: Vec<f64>,
}

fn load_conversation_turn_slots(
    target: &ResolvedConversationTarget,
    desc_ids: &[String],
) -> io::Result<(Vec<ConversationTurnSlot>, Vec<String>, u64)> {
    let mut tagged_turns: Vec<ConversationTurnSlot> = Vec::new();
    let mut db_files: u64 = 1;

    let parent_conn = open_readonly_db(&target.db_path).map_err(|err| {
        io::Error::other(format!(
            "failed to open conversation '{}': {err}",
            target.conversation_id
        ))
    })?;
    if let Some(turns) = extract_sqlite_db_turns(&parent_conn, target.db_mtime_secs) {
        for turn in turns {
            let order = tagged_turns.len();
            tagged_turns.push(ConversationTurnSlot {
                conversation_id: target.conversation_id.clone(),
                is_subagent: false,
                order,
                turn,
            });
        }
    }

    let mut active_subagent_ids: Vec<String> = Vec::new();
    for desc_id in desc_ids {
        let Some(desc_path) = resolve_conversation_db_path(&target.root, desc_id) else {
            continue;
        };
        let desc_mtime = safe_sqlite_metadata(&desc_path)
            .map(|m| modified_timestamp(&m).0)
            .filter(|&secs| secs > 0);
        let Ok(desc_conn) = open_readonly_db(&desc_path) else {
            continue;
        };
        let lower_desc_id = desc_id.to_ascii_lowercase();
        db_files += 1;
        active_subagent_ids.push(lower_desc_id.clone());
        if let Some(turns) = extract_sqlite_db_turns(&desc_conn, desc_mtime) {
            for turn in turns {
                let order = tagged_turns.len();
                tagged_turns.push(ConversationTurnSlot {
                    conversation_id: lower_desc_id.clone(),
                    is_subagent: true,
                    order,
                    turn,
                });
            }
        }
    }

    Ok((tagged_turns, active_subagent_ids, db_files))
}

/// Scans a single conversation (and any descendant subagent conversations) and produces
/// a complete [`ConversationStatsReport`].
pub(in crate::stats) fn collect_conversation_stats_report(
    roots: &[(&'static str, PathBuf)],
    conversation_query: &str,
    now_unix_secs: u64,
    tz: &TimeZoneSpec,
    pricing: &PricingCatalog,
    sort_metric: StatsMetric,
) -> io::Result<ConversationStatsReport> {
    let target = resolve_conversation_target(roots, conversation_query)?;
    let timezone = tz.format_offset_at(now_unix_secs as i64);

    // Open `conversation_summaries.db` once to resolve both descendants and display metadata.
    let summary_meta = load_conversation_summaries(&target.root, None);
    let desc_ids = descendant_ids_from_summaries(&summary_meta, &target.conversation_id);
    let (tagged_turns, active_subagent_ids, db_files) =
        load_conversation_turn_slots(&target, &desc_ids)?;

    let mut deduped = deduplicate_parent_priority_turns(
        tagged_turns,
        |slot| &slot.turn,
        |slot| slot.is_subagent,
        |existing, slot| {
            existing.order = existing.order.min(slot.order);
            merge_raw_turn(&mut existing.turn, slot.turn);
        },
    );

    deduped.sort_by_key(|slot| (slot.turn.ts_secs.unwrap_or(i64::MAX), slot.order));

    let pricer = TurnPricer::build(pricing, deduped.iter().map(|slot| &slot.turn));

    let mut sqlite = SqliteStats {
        db_files,
        ..Default::default()
    };
    let mut speed_samples = Vec::new();
    let mut ttft_samples = Vec::new();
    let mut main_speeds = Vec::new();
    let mut main_ttfts = Vec::new();
    let mut sub_speeds = Vec::new();
    let mut sub_ttfts = Vec::new();
    let mut totals = PeriodSummary::default();
    let mut main = PeriodSummary::default();
    let mut subagents_summary = PeriodSummary::default();
    let mut model_map: HashMap<String, ModelAccumulator> = HashMap::new();
    let mut subagent_map: HashMap<String, SubagentAccumulator> = active_subagent_ids
        .iter()
        .map(|id| (id.clone(), SubagentAccumulator::default()))
        .collect();
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut peak_context_tokens: Option<u64> = None;
    let mut seen_prompt_convs: HashSet<String> = HashSet::new();
    let mut mid_session_misses: u64 = 0;
    let mut mid_session_miss_usd: f64 = 0.0;
    let mut turns_out: Vec<ConversationTurnStats> = Vec::new();

    for slot in &deduped {
        let turn = &slot.turn;
        let decode_speed = valid_decode_speed(turn.decode_speed);
        let ttft_secs = valid_ttft_secs(turn.ttft_secs);

        if let Some(speed) = decode_speed {
            sqlite.completed_turns += 1;
            sqlite.total_output_tokens = sqlite
                .total_output_tokens
                .saturating_add(turn.output_tokens);
            speed_samples.push(speed);
        }
        if let Some(ttft) = ttft_secs {
            ttft_samples.push(ttft);
        }

        if !turn.has_billable_usage() {
            continue;
        }

        if let Some(ts) = turn.ts_secs {
            first_ts = Some(first_ts.map_or(ts, |prev| prev.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |prev| prev.max(ts)));
        }

        let prompt_tokens = turn
            .input_tokens
            .saturating_add(turn.cache_read_tokens)
            .saturating_add(turn.cache_write_tokens);
        if prompt_tokens > 0 {
            peak_context_tokens =
                Some(peak_context_tokens.map_or(prompt_tokens, |prev| prev.max(prompt_tokens)));
        }
        let total_tokens = prompt_tokens.saturating_add(turn.output_tokens);
        let resolved = pricer.price_turn(turn);

        if prompt_tokens > 0 {
            let had_earlier_prompt = !seen_prompt_convs.insert(slot.conversation_id.clone());
            if had_earlier_prompt && turn.cache_read_tokens == 0 {
                mid_session_misses += 1;
                if let Some(usd) = resolved.cost_usd {
                    mid_session_miss_usd += usd;
                }
            }
        }

        let delta = UsageDelta {
            input_tokens: turn.input_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_write_tokens: turn.cache_write_tokens,
            output_tokens: turn.output_tokens,
            thinking_tokens: turn.thinking_tokens,
            total_tokens,
            cost_usd: resolved.cost_usd,
        };

        totals.record_usage(delta);
        if slot.is_subagent {
            subagents_summary.record_usage(delta);
            let sub_acc = subagent_map
                .entry(slot.conversation_id.clone())
                .or_default();
            sub_acc.summary.record_usage(delta);
            if let Some(speed) = decode_speed {
                sub_speeds.push(speed);
                sub_acc.speeds.push(speed);
            }
            if let Some(ttft) = ttft_secs {
                sub_ttfts.push(ttft);
                sub_acc.ttfts.push(ttft);
            }
        } else {
            main.record_usage(delta);
            if let Some(speed) = decode_speed {
                main_speeds.push(speed);
            }
            if let Some(ttft) = ttft_secs {
                main_ttfts.push(ttft);
            }
        }
        let model_entry = model_map.entry(resolved.model_group.clone()).or_default();
        if model_entry.pricing_key.is_none() {
            model_entry.pricing_key = resolved.pricing_key;
        }
        model_entry.summary.record_usage(delta);
        if let Some(speed) = decode_speed {
            model_entry.speeds.push(speed);
        }
        if let Some(ttft) = ttft_secs {
            model_entry.ttfts.push(ttft);
        }

        turns_out.push(ConversationTurnStats {
            index: turns_out.len() + 1,
            conversation_id: slot.conversation_id.clone(),
            is_subagent: slot.is_subagent,
            timestamp_local: turn.ts_secs.map(|ts| tz.local_datetime_string(ts)),
            model: resolved.model_group,
            input_tokens: turn.input_tokens,
            cache_read_tokens: turn.cache_read_tokens,
            cache_write_tokens: turn.cache_write_tokens,
            output_tokens: turn.output_tokens,
            thinking_tokens: turn.thinking_tokens,
            total_tokens,
            cache_hit_pct: cache_hit_pct(
                turn.input_tokens,
                turn.cache_read_tokens,
                turn.cache_write_tokens,
            ),
            priced_usd: resolved.cost_usd,
            tok_per_sec: decode_speed,
            ttft_secs,
        });
    }

    totals.priced_usd = round_usd_6(totals.priced_usd);
    main.priced_usd = round_usd_6(main.priced_usd);
    subagents_summary.priced_usd = round_usd_6(subagents_summary.priced_usd);
    sqlite.ttft_samples = ttft_samples.len();
    sqlite.median_tok_per_sec = compute_median(&speed_samples);
    sqlite.median_ttft_secs = compute_median(&ttft_samples);

    let duration_secs = match (first_ts, last_ts) {
        (Some(a), Some(b)) if b >= a => Some((b - a) as u64),
        _ => None,
    };

    let models = finalize_models(model_map, sort_metric);
    let mut subagents: Vec<SubagentConversationStats> = subagent_map
        .into_iter()
        .map(|(conv_id, mut acc)| {
            acc.summary.priced_usd = round_usd_6(acc.summary.priced_usd);
            let meta = summary_meta.get(&conv_id);
            SubagentConversationStats {
                conversation_id: conv_id,
                agent_name: meta.and_then(|m| m.agent_name.clone()),
                title: meta.and_then(IndexedSummaryRow::display_title),
                summary: acc.summary,
                median_tok_per_sec: compute_median(&acc.speeds),
                median_ttft_secs: compute_median(&acc.ttfts),
            }
        })
        .collect();
    subagents.sort_by(|a, b| {
        sort_metric
            .extract(&a.summary)
            .cmp_desc(sort_metric.extract(&b.summary))
            .then_with(|| b.summary.total_tokens.cmp(&a.summary.total_tokens))
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
    });

    let root_meta = summary_meta.get(&target.conversation_id);
    Ok(ConversationStatsReport {
        conversation_id: target.conversation_id,
        title: root_meta.and_then(IndexedSummaryRow::display_title),
        parent_conversation_id: root_meta.and_then(|m| m.parent_conversation_id.clone()),
        agent_name: root_meta.and_then(|m| m.agent_name.clone()),
        source: target.source.to_string(),
        timezone,
        first_turn_local: first_ts.map(|ts| tz.local_datetime_string(ts)),
        last_turn_local: last_ts.map(|ts| tz.local_datetime_string(ts)),
        duration_secs,
        peak_context_tokens,
        mid_session_misses,
        mid_session_miss_usd: round_usd_6(mid_session_miss_usd),
        totals,
        main,
        subagents_summary,
        main_median_tok_per_sec: compute_median(&main_speeds),
        main_median_ttft_secs: compute_median(&main_ttfts),
        subagents_median_tok_per_sec: compute_median(&sub_speeds),
        subagents_median_ttft_secs: compute_median(&sub_ttfts),
        models,
        subagents,
        turns: turns_out,
        sqlite,
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use std::fs;

    use super::*;
    use crate::sys::time::parse_rfc3339_unix_secs;

    #[test]
    fn conversation_stats_dedups_parent_subagent_and_validates_prefixes() {
        use crate::agy::protobuf::{encode_gen_blob, encode_step_meta_blob, GenBlobSpec};
        use crate::stats::pricing::{PricingCatalog, SyncMode, DEFAULT_PRICING_TTL_SECS};

        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join(".gemini/antigravity-cli");
        let conv_dir = cli_root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        let parent_id = "aaaa1111-1111-4111-8111-111111111111";
        let child_id = "aaaa2222-2222-4222-8222-222222222222";
        let ts1 = parse_rfc3339_unix_secs("2026-11-24T08:00:00Z").unwrap();
        let ts2 = parse_rfc3339_unix_secs("2026-11-24T08:01:00Z").unwrap();

        // Parent DB with 1 turn carrying identity id_11="shared-turn".
        let parent_conn = Connection::open(conv_dir.join(format!("{parent_id}.db"))).unwrap();
        parent_conn
            .execute(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
                [],
            )
            .unwrap();
        parent_conn
            .execute(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
                [],
            )
            .unwrap();
        parent_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [encode_step_meta_blob(ts1)],
            )
            .unwrap();
        parent_conn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1532,
                    resp_model: Some("model-alpha"),
                    disp_name: Some("Model Alpha (High)"),
                    input_tok: 2_000,
                    cache_read_tok: 8_000,
                    output_tok: 400,
                    thinking_tok: 100,
                    id_11: Some("shared-turn"),
                    dur_secs: 2,
                    ttft_nanos: 500_000_000,
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(parent_conn);

        // Child DB repeats "shared-turn" and adds its own distinct turn.
        let child_conn = Connection::open(conv_dir.join(format!("{child_id}.db"))).unwrap();
        child_conn
            .execute(
                "CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB)",
                [],
            )
            .unwrap();
        child_conn
            .execute(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB)",
                [],
            )
            .unwrap();
        child_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (1, ?1)",
                [encode_step_meta_blob(ts1)],
            )
            .unwrap();
        child_conn
            .execute(
                "INSERT INTO steps (idx, metadata) VALUES (2, ?1)",
                [encode_step_meta_blob(ts2)],
            )
            .unwrap();
        child_conn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (1, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![1],
                    model_enum: 1532,
                    resp_model: Some("model-alpha"),
                    disp_name: Some("Model Alpha (High)"),
                    input_tok: 2_000,
                    cache_read_tok: 8_000,
                    output_tok: 400,
                    id_11: Some("shared-turn"),
                    dur_secs: 2,
                    ..Default::default()
                })],
            )
            .unwrap();
        child_conn
            .execute(
                "INSERT INTO gen_metadata (idx, data) VALUES (2, ?1)",
                [encode_gen_blob(GenBlobSpec {
                    step_indices: vec![2],
                    model_enum: 1532,
                    resp_model: Some("model-alpha"),
                    disp_name: Some("Model Alpha (High)"),
                    input_tok: 1_000,
                    cache_read_tok: 3_000,
                    output_tok: 200,
                    id_11: Some("child-only"),
                    dur_secs: 1,
                    ttft_nanos: 250_000_000,
                    ..Default::default()
                })],
            )
            .unwrap();
        drop(child_conn);

        let sum_conn = Connection::open(cli_root.join("conversation_summaries.db")).unwrap();
        sum_conn
            .execute(
                "CREATE TABLE conversation_summaries (\
                    conversation_id TEXT PRIMARY KEY,\
                    title TEXT NOT NULL DEFAULT '',\
                    parent_conversation_id TEXT NOT NULL DEFAULT '',\
                    agent_name TEXT NOT NULL DEFAULT ''\
                )",
                [],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Parent\nTitle', '', '')",
                [parent_id],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, '', ?2, 'explorer')",
                [child_id, parent_id],
            )
            .unwrap();
        drop(sum_conn);

        let roots = [("cli", cli_root)];
        let pricing = PricingCatalog::load_effective_with(
            None,
            SyncMode::Offline,
            ts2 as u64,
            DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        let tz = TimeZoneSpec::fixed_offset(8 * 3_600);

        // Unique prefix resolves parent and deduplicates "shared-turn" to parent attribution.
        let report = collect_conversation_stats_report(
            &roots,
            "aaaa1111",
            ts2 as u64,
            &tz,
            &pricing,
            StatsMetric::Total,
        )
        .unwrap();
        assert_eq!(report.conversation_id, parent_id);
        assert_eq!(report.title.as_deref(), Some("Parent Title"));
        assert_eq!(report.timezone, "UTC+08:00");
        assert_eq!(
            report.first_turn_local.as_deref(),
            Some("2026-11-24 16:00:00")
        );
        assert_eq!(
            report.last_turn_local.as_deref(),
            Some("2026-11-24 16:01:00")
        );
        assert_eq!(report.duration_secs, Some(60));
        assert_eq!(report.peak_context_tokens, Some(10_000));
        assert_eq!(report.mid_session_misses, 0);
        assert_eq!(report.totals.requests, 2);
        assert_eq!(report.main.requests, 1);
        assert_eq!(report.subagents_summary.requests, 1);
        assert_eq!(report.subagents.len(), 1);
        assert_eq!(report.subagents[0].agent_name.as_deref(), Some("explorer"));
        assert_eq!(report.subagents[0].summary.requests, 1);
        assert!((report.turns[0].cache_hit_pct.unwrap() - 80.0).abs() < 1e-6);
        assert!((report.turns[1].cache_hit_pct.unwrap() - 75.0).abs() < 1e-6);

        // Direct inspection of child conversation populates parent_conversation_id & agent_name.
        let child_report = collect_conversation_stats_report(
            &roots,
            "aaaa2222",
            ts2 as u64,
            &tz,
            &pricing,
            StatsMetric::Total,
        )
        .unwrap();
        assert_eq!(
            child_report.parent_conversation_id.as_deref(),
            Some(parent_id)
        );
        assert_eq!(child_report.agent_name.as_deref(), Some("explorer"));
        assert!(child_report.subagents.is_empty());

        // Ambiguous prefix, short prefix, invalid chars, and missing ID fail cleanly.
        let ambig_err = resolve_conversation_target(&roots, "aaaa").unwrap_err();
        assert_eq!(ambig_err.kind(), io::ErrorKind::InvalidInput);
        assert!(ambig_err.to_string().contains("ambiguous"), "{ambig_err}");

        let short_err = resolve_conversation_target(&roots, "aaa").unwrap_err();
        assert_eq!(short_err.kind(), io::ErrorKind::InvalidInput);

        let bad_err = resolve_conversation_target(&roots, "../etc").unwrap_err();
        assert_eq!(bad_err.kind(), io::ErrorKind::InvalidInput);

        let missing_err = resolve_conversation_target(&roots, "deadbeef").unwrap_err();
        assert_eq!(missing_err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn list_conversations_enriches_summaries_counts_subagents_and_handles_minimal_schema() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let conv_dir = cli_root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        let root_a = "11111111-1111-4111-8111-111111111111";
        let child_a = "22222222-2222-4222-8222-222222222222";
        let root_b = "33333333-3333-4333-8333-333333333333";

        let _ = Connection::open(conv_dir.join(format!("{root_a}.db"))).unwrap();
        let _ = Connection::open(conv_dir.join(format!("{child_a}.db"))).unwrap();
        let _ = Connection::open(conv_dir.join(format!("{root_b}.db"))).unwrap();

        let sum_conn = Connection::open(cli_root.join("conversation_summaries.db")).unwrap();
        sum_conn
            .execute(
                "CREATE TABLE conversation_summaries (\
                    conversation_id TEXT PRIMARY KEY,\
                    title TEXT NOT NULL DEFAULT '',\
                    preview TEXT NOT NULL DEFAULT '',\
                    step_count INTEGER NOT NULL DEFAULT 0,\
                    last_modified_time TEXT NOT NULL DEFAULT '',\
                    workspace_uris TEXT NOT NULL DEFAULT '',\
                    agent_name TEXT NOT NULL DEFAULT '',\
                    parent_conversation_id TEXT NOT NULL DEFAULT ''\
                )",
                [],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Cache Audit', 'ignored', 42, '2026-11-24 08:00:00+00:00', '[\"file:///home/dev/agy-statusline\"]', '', '')",
                [root_a],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, '', 'subagent preview', 5, '2026-11-24 08:01:00+00:00', '', 'research', ?2)",
                [child_a, root_a],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, '', 'Fallback\nPreview', 10, '2026-11-24 09:30:00+00:00', '[\"file:///work/demo/\"]', '', '')",
                [root_b],
            )
            .unwrap();
        drop(sum_conn);

        let roots = [("cli", cli_root)];
        let tz = TimeZoneSpec::fixed_offset(8 * 3_600);
        let items = list_conversations(&roots, &tz);

        assert_eq!(items.len(), 3);
        // Sorted newest first: root_b (09:30Z -> 17:30+08) > child_a (08:01Z) > root_a (08:00Z).
        assert_eq!(items[0].conversation_id, root_b);
        assert_eq!(items[0].title.as_deref(), Some("Fallback Preview"));
        assert_eq!(items[0].workspace.as_deref(), Some("demo"));
        assert_eq!(items[0].last_modified_local, "2026-11-24 17:30");
        assert_eq!(items[0].step_count, Some(10));
        assert_eq!(items[0].subagent_count, 0);

        assert_eq!(items[1].conversation_id, child_a);
        assert_eq!(items[1].parent_conversation_id.as_deref(), Some(root_a));
        assert_eq!(items[1].agent_name.as_deref(), Some("research"));

        assert_eq!(items[2].conversation_id, root_a);
        assert_eq!(items[2].title.as_deref(), Some("Cache Audit"));
        assert_eq!(items[2].workspace.as_deref(), Some("agy-statusline"));
        assert_eq!(items[2].subagent_count, 1);
        assert_eq!(items[2].step_count, Some(42));

        // Single-conversation report shares the same `title -> preview` fallback as list_conversations.
        let pricing = PricingCatalog::load_effective_with(
            None,
            crate::stats::pricing::SyncMode::Offline,
            0,
            crate::stats::pricing::DEFAULT_PRICING_TTL_SECS,
            |_| unreachable!(),
        );
        let root_b_report = collect_conversation_stats_report(
            &roots,
            "33333333",
            0,
            &tz,
            &pricing,
            StatsMetric::Total,
        )
        .unwrap();
        assert_eq!(root_b_report.title.as_deref(), Some("Fallback Preview"));

        // Minimal schema (only 4 columns) still succeeds without error.
        let min_dir = tempfile::tempdir().unwrap();
        let min_root = min_dir.path().join("cli");
        fs::create_dir_all(min_root.join("conversations")).unwrap();
        let _ =
            Connection::open(min_root.join("conversations").join(format!("{root_a}.db"))).unwrap();
        let min_sum = Connection::open(min_root.join("conversation_summaries.db")).unwrap();
        min_sum
            .execute(
                "CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, parent_conversation_id TEXT, agent_name TEXT)",
                [],
            )
            .unwrap();
        min_sum
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Minimal Row', '', '')",
                [root_a],
            )
            .unwrap();
        drop(min_sum);
        let min_items = list_conversations(&[("cli", min_root)], &tz);
        assert_eq!(min_items.len(), 1);
        assert_eq!(min_items[0].title.as_deref(), Some("Minimal Row"));
    }

    #[test]
    fn list_conversations_guards_against_parent_cycles_and_self_loops() {
        let dir = tempfile::tempdir().unwrap();
        let cli_root = dir.path().join("cli");
        let conv_dir = cli_root.join("conversations");
        fs::create_dir_all(&conv_dir).unwrap();

        let self_loop = "11111111-1111-4111-8111-111111111111";
        let cycle_a = "22222222-2222-4222-8222-222222222222";
        let cycle_b = "33333333-3333-4333-8333-333333333333";
        let _ = Connection::open(conv_dir.join(format!("{self_loop}.db"))).unwrap();
        let _ = Connection::open(conv_dir.join(format!("{cycle_a}.db"))).unwrap();
        let _ = Connection::open(conv_dir.join(format!("{cycle_b}.db"))).unwrap();

        let sum_conn = Connection::open(cli_root.join("conversation_summaries.db")).unwrap();
        sum_conn
            .execute(
                "CREATE TABLE conversation_summaries (\
                    conversation_id TEXT PRIMARY KEY,\
                    title TEXT NOT NULL DEFAULT '',\
                    parent_conversation_id TEXT NOT NULL DEFAULT '',\
                    agent_name TEXT NOT NULL DEFAULT ''\
                )",
                [],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Self', ?1, '')",
                [self_loop],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Cycle A', ?2, '')",
                [cycle_a, cycle_b],
            )
            .unwrap();
        sum_conn
            .execute(
                "INSERT INTO conversation_summaries VALUES (?1, 'Cycle B', ?2, '')",
                [cycle_b, cycle_a],
            )
            .unwrap();
        drop(sum_conn);

        let items = list_conversations(&[("cli", cli_root)], &TimeZoneSpec::fixed_offset(0));
        let counts: HashMap<&str, usize> = items
            .iter()
            .map(|i| (i.conversation_id.as_str(), i.subagent_count))
            .collect();
        assert_eq!(counts[self_loop], 0);
        assert_eq!(counts[cycle_a], 1);
        assert_eq!(counts[cycle_b], 1);
    }
}
