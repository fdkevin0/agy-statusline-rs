//! Zero-copy Protobuf wire-format decoder for AGY generation metadata.
//!
//! Extracts the last turn's output token count and generation duration from
//! the serialized `gen_metadata` blob without schema codegen or heap allocation.

/// A decoded Protobuf wire field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldValue<'a> {
    /// Varint (wire type 0) or fixed-width placeholder (wire types 1 and 5).
    Number(u64),
    /// Length-delimited byte slice (wire type 2).
    Bytes(&'a [u8]),
}

/// Decodes a base-128 varint starting at `*pos`, advancing `*pos` past it.
///
/// Returns `None` on truncated input or 64-bit overflow.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::decode_varint;
///
/// let mut pos = 0;
/// assert_eq!(decode_varint(&[0xac, 0x02], &mut pos), Some(300));
/// assert_eq!(pos, 2);
/// ```
pub fn decode_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        let &b = data.get(*pos)?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
        if shift == 63 {
            return None;
        }
        shift += 7;
    }
    None
}

/// Scans a Protobuf message slice for the last occurrence of field `wanted`.
///
/// Supports wire types 0 (varint), 1 (64-bit), 2 (length-delimited), and 5 (32-bit).
/// Returns `None` if the wire stream is malformed or the field is absent.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::{find_field, FieldValue};
///
/// let msg = [0x08, 0x2a]; // field 1 = 42
/// assert_eq!(find_field(&msg, 1), Some(FieldValue::Number(42)));
/// assert_eq!(find_field(&msg, 2), None);
/// ```
pub fn find_field(data: &[u8], wanted: u64) -> Option<FieldValue<'_>> {
    let mut pos = 0;
    let mut result = None;
    while pos < data.len() {
        let key = decode_varint(data, &mut pos)?;
        let field_id = key >> 3;
        if field_id == 0 {
            return None;
        }
        let field = match key & 7 {
            0 => FieldValue::Number(decode_varint(data, &mut pos)?),
            2 => {
                let size_u64 = decode_varint(data, &mut pos)?;
                let remaining = data.len() - pos;
                if size_u64 > remaining as u64 {
                    return None;
                }
                let size = size_u64 as usize;
                let slice = &data[pos..pos + size];
                pos += size;
                FieldValue::Bytes(slice)
            }
            1 => {
                if data.len() - pos < 8 {
                    return None;
                }
                pos += 8;
                FieldValue::Number(0)
            }
            5 => {
                if data.len() - pos < 4 {
                    return None;
                }
                pos += 4;
                FieldValue::Number(0)
            }
            _ => return None,
        };
        if field_id == wanted {
            result = Some(field);
        }
    }
    result
}

/// Returns the byte slice for length-delimited field `id`, if present.
pub fn find_bytes(data: &[u8], id: u64) -> Option<&[u8]> {
    match find_field(data, id)? {
        FieldValue::Bytes(bytes) => Some(bytes),
        FieldValue::Number(_) => None,
    }
}

/// Returns the varint value for numeric field `id`, defaulting to `0` if absent or non-numeric.
pub fn find_number(data: &[u8], id: u64) -> u64 {
    match find_field(data, id) {
        Some(FieldValue::Number(n)) => n,
        _ => 0,
    }
}

/// Extracts output generation speed (tokens/second) from a `gen_metadata` Protobuf blob.
///
/// Returns `None` if required nested fields are missing, `nanos >= 1_000_000_000`,
/// `seconds < 0.1`, `output == 0`, or the resulting speed is not in `(0.0, 1000.0)`.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::extract_metadata_speed;
///
/// // chat(1) -> { usage(4) -> { output(3)=100 }, duration(12) -> { seconds(1)=2 } }
/// let blob = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
/// assert_eq!(extract_metadata_speed(&blob), Some(50.0));
/// ```
pub fn extract_metadata_speed(data: &[u8]) -> Option<f64> {
    let chat = find_bytes(data, 1)?;
    let usage = find_bytes(chat, 4)?;
    let duration = find_bytes(chat, 12)?;
    let nanos = find_number(duration, 2);
    if nanos >= 1_000_000_000 {
        return None;
    }
    let seconds = (find_number(duration, 1) as f64) + (nanos as f64) / 1e9;
    let output = find_number(usage, 3);
    if seconds < 0.1 || output == 0 {
        return None;
    }
    let speed = (output as f64) / seconds;
    if speed > 0.0 && speed < 1000.0 {
        Some(speed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_edge_cases() {
        let mut pos = 0;
        assert_eq!(decode_varint(&[], &mut pos), None);
        pos = 0;
        assert_eq!(decode_varint(&[0x00], &mut pos), Some(0));
        pos = 0;
        assert_eq!(
            decode_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], &mut pos),
            Some(u64::MAX)
        );
        pos = 0;
        assert_eq!(
            decode_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02], &mut pos),
            None
        );
    }

    #[test]
    fn metadata_speed_valid_and_invalid() {
        let valid = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        assert_eq!(extract_metadata_speed(&valid), Some(50.0));
        assert_eq!(extract_metadata_speed(&[0x0a, 0xff]), None);
        assert_eq!(extract_metadata_speed(&[]), None);
    }

    #[test]
    fn wire_types_1_and_5_skipped() {
        // field 2 (64-bit, 8 bytes), then field 1 (varint = 7)
        let msg = [0x11, 1, 2, 3, 4, 5, 6, 7, 8, 0x08, 0x07];
        assert_eq!(find_number(&msg, 1), 7);
        // field 2 (32-bit, 4 bytes), then field 1 (varint = 9)
        let msg32 = [0x15, 1, 2, 3, 4, 0x08, 0x09];
        assert_eq!(find_number(&msg32, 1), 9);
        // truncated 64-bit field
        assert_eq!(find_field(&[0x11, 1, 2, 3], 2), None);
        // invalid wire type 3
        assert_eq!(find_field(&[0x0b, 0x01], 1), None);
        // field id 0 is invalid
        assert_eq!(find_field(&[0x00, 0x01], 0), None);
    }
}
