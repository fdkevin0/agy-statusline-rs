#![forbid(unsafe_code)]
//! Tier 2 binary CLI integration tests for quota token calibration.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use agy_statusline::calibration::{load_calibration_store, BucketCalibration, CalibrationStore};
use agy_statusline::layout::{save_layout_config, LayoutConfig, WidgetId};

use common::{run_statusline_stdin, unix_now_secs};

fn write_quota_layout(base: &Path) {
    let config = LayoutConfig {
        rows: vec![vec![WidgetId::Quota5h, WidgetId::QuotaWeekly]],
        ..LayoutConfig::default()
    };
    save_layout_config(&base.join("config/agy-statusline/config.json"), &config).unwrap();
}

#[test]
fn cli_calibrates_quota_tokens_across_ticks_and_persists_in_xdg_cache() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    write_quota_layout(base);

    let tick1 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-1",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 10000, "total_output_tokens": 0},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.90, "reset_in_seconds": 14400}
        }
    }"#;
    let out1 = run_statusline_stdin(base, &[], tick1);
    assert!(out1.contains("5h 90%"));
    assert!(!out1.contains("(~"));
    assert!(base.join("cache/agy-statusline/quota_cal.json").is_file());

    // +200K tokens and -10% quota -> calibrates 2.0M pool -> remaining 80% = 1.6M.
    let tick2 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-1",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 200000, "total_output_tokens": 10000},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.80, "reset_in_seconds": 14300}
        }
    }"#;
    let out2 = run_statusline_stdin(base, &[], tick2);
    assert!(out2.contains("5h 80% (~1.6M) ↻3h58m"));

    // Conversation change re-anchors while preserving 2.0M calibration -> 75% = 1.5M.
    let tick3 = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli-2",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 5000, "total_output_tokens": 0},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.75, "reset_in_seconds": 14200}
        }
    }"#;
    let out3 = run_statusline_stdin(base, &[], tick3);
    assert!(out3.contains("5h 75% (~1.5M)"));

    let cal_raw = fs::read_to_string(base.join("cache/agy-statusline/quota_cal.json")).unwrap();
    assert!(cal_raw.contains("\"gemini-5h\""));
    assert!(!cal_raw.contains("\"5h:5h\""));
}

#[test]
fn cli_uses_unexpired_estimates_while_another_process_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    write_quota_layout(base);
    let cache = base.join("cache/agy-statusline");
    let age_eight_days = unix_now_secs() - 8 * 86_400;
    let short = BucketCalibration {
        calibrated_total_tokens: Some(2_000_000.0),
        calibrated_at_secs: Some(age_eight_days),
        sample_count: 1,
        last_conversation_id: Some("conv-cli".to_owned()),
        anchor_fraction: Some(0.80),
        anchor_session_tokens: Some(210_000),
        last_fraction: Some(0.80),
        ..Default::default()
    };
    let weekly = BucketCalibration {
        calibrated_total_tokens: Some(10_000_000.0),
        ..short.clone()
    };
    let store = CalibrationStore {
        buckets: BTreeMap::from([("gemini-5h".into(), short), ("gemini-7d".into(), weekly)]),
        ..Default::default()
    };
    fs::create_dir_all(&cache).unwrap();
    fs::write(
        cache.join("quota_cal.json"),
        serde_json::to_vec(&store).unwrap(),
    )
    .unwrap();

    let tick = r#"{
        "agent_state": "idle",
        "conversation_id": "conv-cli",
        "model": {"id": "gemini-4-hydrogen"},
        "context_window": {"total_input_tokens": 510000},
        "quota": {
            "gemini-5h": {"remaining_fraction": 0.70},
            "gemini-7d": {"remaining_fraction": 0.75}
        }
    }"#;
    let path = cache.join("quota_cal.json");
    let before = fs::read(&path).unwrap();
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(cache.join("quota_cal.lock"))
        .unwrap();
    lock.lock().unwrap();

    let skipped = run_statusline_stdin(base, &[], tick);
    assert!(skipped.contains("5h 70%"));
    assert!(!skipped.contains("5h 70% (~"));
    assert!(skipped.contains("7d 75% (~7.5M)"), "{skipped}");
    assert_eq!(fs::read(&path).unwrap(), before);

    lock.unlock().unwrap();
    drop(lock);
    let sampled = run_statusline_stdin(base, &[], tick);
    assert!(sampled.contains("5h 70% (~2.1M)"));
    assert!(sampled.contains("7d 75% (~5.7M)"), "{sampled}");
    let reloaded = load_calibration_store(&cache);
    assert_eq!(
        reloaded.buckets["gemini-5h"].anchor_session_tokens,
        Some(510_000)
    );
    assert_eq!(reloaded.buckets["gemini-7d"].sample_count, 2);
}

#[test]
fn cli_with_credit_only_quota_skips_the_calibration_cache() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    write_quota_layout(base);
    let tick = r#"{
        "model": {"id": "gemini-4-hydrogen"},
        "quota": {"gemini-5h": {"remaining_amount": 42}}
    }"#;

    let output = run_statusline_stdin(base, &[], tick);
    assert!(output.contains("5h 42"));
    assert!(!base.join("cache/agy-statusline/quota_cal.lock").exists());
    assert!(!base.join("cache/agy-statusline/quota_cal.json").exists());
}
