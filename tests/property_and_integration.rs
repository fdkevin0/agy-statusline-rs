//! Property-based (`proptest`) and end-to-end integration tests for `agy-statusline`.

use proptest::prelude::*;

use agy_statusline::db::{is_valid_uuid, SpeedReader};
use agy_statusline::format::{format_tokens, render_statusline, TokenUsage, MAX_TERMINAL_WIDTH};
use agy_statusline::protobuf::{decode_varint, extract_metadata_speed, find_field};
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
        if res.is_some() {
            prop_assert!(pos >= 1 && pos <= data.len().min(10));
        }
    }

    #[test]
    fn prop_find_field_never_panics(data in prop::collection::vec(any::<u8>(), 0..128), field_id in 0u64..16) {
        let _ = find_field(&data, field_id);
    }

    #[test]
    fn prop_extract_metadata_speed_bounds(data in prop::collection::vec(any::<u8>(), 0..128)) {
        if let Some(speed) = extract_metadata_speed(&data) {
            prop_assert!(speed > 0.0 && speed < 1000.0);
        }
    }

    #[test]
    fn prop_format_tokens_matches_k_rule(value in 0u64..1_000_000_000) {
        let mut buf = String::new();
        format_tokens(value, &mut buf);
        if value >= 1000 {
            prop_assert!(buf.ends_with('K'));
        } else {
            prop_assert!(!buf.ends_with('K'));
        }
    }

    #[test]
    fn prop_render_statusline_ansi_framing(
        input in -10i64..1_000_000,
        read in -10i64..1_000_000,
        creation in -1i64..10,
        speed in prop::option::of(0.1f64..999.9),
        width in prop::option::of(0usize..1500),
    ) {
        let usage = TokenUsage {
            input_tokens: Some(input),
            cache_read_input_tokens: Some(read),
            cache_creation_input_tokens: Some(creation),
        };
        let rendered = render_statusline(Some(usage), speed);
        let mut out = Vec::new();
        rendered.write_padded(&mut out, width).unwrap();
        let text = String::from_utf8(out).unwrap();
        prop_assert!(text.ends_with("\x1b[0m\n"));
        if let Some(w) = width {
            let clamped = w.min(MAX_TERMINAL_WIDTH);
            if clamped > rendered.payload.len() {
                let spaces = clamped - rendered.payload.len();
                let expected_tail = format!("{}\x1b[0m\n", " ".repeat(spaces));
                prop_assert!(text.ends_with(&expected_tail));
            }
        }
    }
}

#[test]
fn integration_full_json_and_uuid_checks() {
    assert!(is_valid_uuid("01234567-89ab-cdef-0123-456789abcdef"));
    assert!(!is_valid_uuid("01234567-89ab-cdef-0123-456789abcde"));

    let json = r#"{
        "terminal_width": 80,
        "conversation_id": "01234567-89ab-cdef-0123-456789abcdef",
        "context_window": {
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
    assert!(s.starts_with("\x1b[44;97mCache hit: 75.0% | Cache read: 1K | Last avg speed: 120 token/s"));
    assert!(s.ends_with("\x1b[0m\n"));
}
