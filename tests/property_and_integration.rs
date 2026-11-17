//! Property-based (`proptest`) and end-to-end integration tests for `agy-statusline`.

use proptest::prelude::*;

use agy_statusline::db::{is_valid_uuid, SpeedReader};
use agy_statusline::format::format_tokens;
use agy_statusline::protobuf::{decode_varint, extract_metadata_speed, find_field};
use agy_statusline::render::build_progress_bar;
use agy_statusline::statusline::process_statusline;

struct FixedSpeed(Option<f64>);
impl SpeedReader for FixedSpeed {
    fn last_speed(&self, _id: &str) -> Option<f64> {
        self.0
    }
}

proptest! {
    #[test]
    fn prop_decode_varint_never_panics(data in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut pos = 0;
        let res = decode_varint(&data, &mut pos);
        prop_assert!(res.is_none() || (pos >= 1 && pos <= data.len().min(10)));
    }

    #[test]
    fn prop_find_field_never_panics(data in prop::collection::vec(any::<u8>(), 0..128), field_id in 0u64..16) {
        let _ = find_field(&data, field_id);
    }

    #[test]
    fn prop_extract_metadata_speed_bounds(data in prop::collection::vec(any::<u8>(), 0..128)) {
        let speed = extract_metadata_speed(&data);
        prop_assert!(speed.is_none_or(|s| s > 0.0 && s < 1000.0));
    }

    #[test]
    fn prop_format_tokens_small_no_suffix(value in 0u64..1000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(!buf.ends_with('K'));
    }

    #[test]
    fn prop_format_tokens_large_has_k_suffix(value in 1000u64..1_000_000_000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        prop_assert!(buf.ends_with('K'));
    }

    #[test]
    fn prop_progress_bar_clamps_and_matches_char_len(pct in -50.0f64..200.0, bar_len in 4usize..40) {
        let (bar, clamped) = build_progress_bar(pct, bar_len);
        prop_assert!((0.0..=100.0).contains(&clamped));
        prop_assert_eq!(bar.chars().count(), bar_len);
    }
}

#[test]
fn integration_full_json_and_uuid_checks() {
    assert!(is_valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
    assert!(!is_valid_uuid("01234567-89ab-cdef-0123-456789abcde"));

    let json = r#"{
        "agent_state": "working",
        "model": {"display_name": "Gemini 4", "effort": "high"},
        "vcs": {"branch": "main", "dirty": true},
        "terminal_width": 120,
        "conversation_id": "01234567-89ab-cdef-0123-456789abcdef",
        "context_window": {
            "used_percentage": 35.0,
            "current_usage": {
                "input_tokens": 500,
                "cache_read_input_tokens": 1500,
                "cache_creation_input_tokens": 0
            }
        }
    }"#;
    let mut out = Vec::new();
    process_statusline(json, &FixedSpeed(Some(120.4)), &mut out).unwrap();
    let s = String::from_utf8(out).unwrap();
    assert!(s.contains("WORKING"));
    assert!(!s.contains("Gemini 4"));
    assert!(s.contains("main*"));
    assert!(s.contains("75.0%"));
    assert!(s.contains("120 tok/s"));
}
