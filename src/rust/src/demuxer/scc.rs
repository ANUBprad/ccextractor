//! SCC (Scenarist Closed Caption) format parser
//!
//! This module provides functionality to parse SCC files and extract CEA-608 caption data.
//! SCC is a text-based format commonly used in professional video production.
//!
//! # Format Specification
//!
//! ```text
//! Scenarist_SCC V1.0
//!
//! 00:00:00:00    9420 9420 94ad 94ad 9470 9470 4c6f 7265
//! 00:00:02:15    942c 942c
//! ```
//!
//! - **Header:** `Scenarist_SCC V1.0` (first line)
//! - **Lines:** `HH:MM:SS:FF\t<hex pairs>` (SMPTE timecode + TAB + space-separated hex bytes)
//! - **Hex pairs:** CEA-608 byte pairs with odd parity bits (e.g., `9420` = RCL command)
//! - **Frame rate:** 29.97fps NTSC drop-frame (default), also supports 24/25/30fps

/// SCC format header
pub const SCC_HEADER: &[u8] = b"Scenarist_SCC V1.0";

/// Frame rate options for SMPTE timecode conversion
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SccFrameRate {
    /// NTSC 29.97fps (default); the timecode separator selects drop-frame or non-drop-frame
    #[default]
    Fps29_97,
    /// Film - 24fps
    Fps24,
    /// PAL - 25fps
    Fps25,
    /// NTSC non-drop-frame - 30fps
    Fps30,
}

impl SccFrameRate {
    /// Convert from integer value (used in FFI)
    /// 0=29.97 (default), 1=24, 2=25, 3=30
    pub fn from_int(value: i32) -> Self {
        match value {
            1 => SccFrameRate::Fps24,
            2 => SccFrameRate::Fps25,
            3 => SccFrameRate::Fps30,
            _ => SccFrameRate::Fps29_97,
        }
    }

    /// Get milliseconds per frame for this frame rate
    pub fn ms_per_frame(&self) -> f64 {
        match self {
            SccFrameRate::Fps29_97 => 1001.0 / 30.0, // ~33.37ms
            SccFrameRate::Fps24 => 1000.0 / 24.0,    // ~41.67ms
            SccFrameRate::Fps25 => 1000.0 / 25.0,    // 40ms
            SccFrameRate::Fps30 => 1000.0 / 30.0,    // ~33.33ms
        }
    }
}

/// Result of parsing an SCC file
#[derive(Debug, Clone)]
pub struct SccParseResult {
    /// Parsed caption lines with timing
    pub lines: Vec<SccLine>,
    /// Number of bytes consumed from the input
    pub bytes_consumed: usize,
}

/// Single line from SCC file with timing and caption data
#[derive(Debug, Clone)]
pub struct SccLine {
    /// Timestamp in milliseconds
    pub time_ms: i64,
    /// CEA-608 byte pairs
    pub pairs: Vec<[u8; 2]>,
}

/// Caption block type for Field 1 (CC1/CC3)
pub const CC_TYPE_FIELD1: u8 = 0x04;

/// Caption block type for Field 2 (CC2/CC4)
#[allow(dead_code)]
pub const CC_TYPE_FIELD2: u8 = 0x05;

/// Check if buffer starts with SCC header
///
/// # Arguments
/// * `buffer` - The byte buffer to check
///
/// # Returns
/// `true` if the buffer starts with the SCC header signature
pub fn is_scc_file(buffer: &[u8]) -> bool {
    // Skip UTF-8 BOM if present
    let data = if buffer.len() >= 3 && buffer[0] == 0xEF && buffer[1] == 0xBB && buffer[2] == 0xBF {
        &buffer[3..]
    } else {
        buffer
    };

    data.len() >= SCC_HEADER.len() && &data[..SCC_HEADER.len()] == SCC_HEADER
}

/// Parse SMPTE timecode to milliseconds.
///
/// A colon before the frame number denotes non-drop-frame timecode. A
/// semicolon denotes drop-frame timecode and is valid only for 29.97 fps.
///
/// For 29.97 fps, each nominal frame represents 1001/30 milliseconds. Drop
/// frame timecode omits frame labels 00 and 01 at the start of every minute,
/// except each tenth minute; the omitted labels are subtracted before
/// converting the frame count to elapsed time.
///
/// Returns `None` for malformed or out-of-range timecodes, invalid
/// drop-frame labels, or timestamps that cannot fit in an `i64`.
pub fn parse_smpte_timecode(timecode: &str, fps: SccFrameRate) -> Option<i64> {
    // Split only at the final delimiter. The clock portion must contain
    // exactly three colon-separated fields, preventing mixed delimiters.
    let (clock, frame_part) = timecode
        .rsplit_once(':')
        .or_else(|| timecode.rsplit_once(';'))?;
    let drop_frame = timecode.as_bytes().get(clock.len()) == Some(&b';');

    let mut clock_parts = clock.split(':');
    let h: u64 = clock_parts.next()?.parse().ok()?;
    let m: u64 = clock_parts.next()?.parse().ok()?;
    let s: u64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some() || m >= 60 || s >= 60 {
        return None;
    }

    let frame: u64 = frame_part.parse().ok()?;
    let nominal_fps = match fps {
        SccFrameRate::Fps29_97 => 30_u64,
        SccFrameRate::Fps24 => 24,
        SccFrameRate::Fps25 => 25,
        SccFrameRate::Fps30 => 30,
    };

    if frame >= nominal_fps || (drop_frame && fps != SccFrameRate::Fps29_97) {
        return None;
    }

    let total_minutes = h.checked_mul(60)?.checked_add(m)?;
    let total_seconds = total_minutes.checked_mul(60)?.checked_add(s)?;
    let mut total_frames = total_seconds
        .checked_mul(nominal_fps)?
        .checked_add(frame)?;

    if drop_frame {
        // SMPTE drop-frame skips two labels at every minute except each tenth.
        // Labels 00 and 01 at those minute boundaries are therefore invalid.
        if m % 10 != 0 && s == 0 && frame < 2 {
            return None;
        }

        let dropped_frames = total_minutes
            .checked_sub(total_minutes / 10)?
            .checked_mul(2)?;
        total_frames = total_frames.checked_sub(dropped_frames)?;
    }

    // Use integer rational arithmetic to avoid floating-point drift and
    // preserve the 1000/24, 1000/25, 1000/30, and 1001/30 ms frame periods.
    let (numerator, denominator) = match fps {
        SccFrameRate::Fps29_97 => (u128::from(total_frames) * 1001, 30_u128),
        SccFrameRate::Fps24 => (u128::from(total_frames) * 1000, 24),
        SccFrameRate::Fps25 => (u128::from(total_frames) * 1000, 25),
        SccFrameRate::Fps30 => (u128::from(total_frames) * 1000, 30),
    };

    i64::try_from(numerator / denominator).ok()
}

/// Parse hex pair string (e.g., "9420") to bytes
///
/// # Arguments
/// * `s` - A 4-character hex string
///
/// # Returns
/// The two bytes, or None if parsing fails
pub fn parse_hex_pair(s: &str) -> Option<[u8; 2]> {
    if s.len() != 4 || !s.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let b1 = u8::from_str_radix(&s[0..2], 16).ok()?;
    let b2 = u8::from_str_radix(&s[2..4], 16).ok()?;
    Some([b1, b2])
}

/// Parse space-separated hex pairs from SCC line
///
/// # Arguments
/// * `hex_str` - Space-separated hex pairs (e.g., "9420 9420 94ad")
///
/// # Returns
/// Vector of byte pairs
pub fn parse_hex_pairs(hex_str: &str) -> Vec<[u8; 2]> {
    hex_str
        .split_whitespace()
        .filter_map(parse_hex_pair)
        .collect()
}

/// Parse entire SCC file content
///
/// # Arguments
/// * `content` - The SCC file content as a string
/// * `fps` - The frame rate to use for timecode conversion
///
/// # Returns
/// Parse result containing all caption lines
pub fn parse_scc(content: &str, fps: SccFrameRate) -> SccParseResult {
    let mut lines = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines, header, and comments
        if line.is_empty() || line.starts_with("Scenarist_SCC") {
            continue;
        }

        // Parse timecode and hex data
        // Format: "HH:MM:SS:FF\t<hex pairs>"
        if let Some(tab_pos) = line.find('\t') {
            let timecode = &line[..tab_pos];
            let hex_data = &line[tab_pos + 1..];

            if let Some(time_ms) = parse_smpte_timecode(timecode, fps) {
                let pairs = parse_hex_pairs(hex_data);
                if !pairs.is_empty() {
                    lines.push(SccLine { time_ms, pairs });
                }
            }
        }
    }

    SccParseResult {
        lines,
        bytes_consumed: content.len(),
    }
}

/// Process SCC with callbacks (similar to dvdraw pattern)
///
/// This function parses the SCC format and calls callbacks for each caption block
/// and timing update.
///
/// # Arguments
/// * `content` - The SCC file content as a string
/// * `fps` - The frame rate to use for timecode conversion
/// * `callback` - Called for each caption pair: (cc_type, data1, data2)
/// * `timing_callback` - Called with the timestamp in milliseconds before each line
///
/// # Returns
/// The number of bytes consumed
pub fn parse_scc_with_callbacks<F, T>(
    content: &str,
    fps: SccFrameRate,
    mut callback: F,
    mut timing_callback: T,
) -> usize
where
    F: FnMut(u8, u8, u8),
    T: FnMut(i64),
{
    let result = parse_scc(content, fps);

    for line in &result.lines {
        // Call timing callback with this line's timestamp
        timing_callback(line.time_ms);

        // Process each caption pair as field 1 (CC1)
        for pair in &line.pairs {
            callback(CC_TYPE_FIELD1, pair[0], pair[1]);
        }
    }

    result.bytes_consumed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_scc_file() {
        assert!(is_scc_file(b"Scenarist_SCC V1.0\n\n"));
        assert!(is_scc_file(b"\xEF\xBB\xBFScenarist_SCC V1.0\n")); // With BOM
        assert!(!is_scc_file(b"Not an SCC file"));
        assert!(!is_scc_file(b"Scenarist_SCC V2.0")); // Wrong version
        assert!(!is_scc_file(b"")); // Empty
        assert!(!is_scc_file(b"Scenarist")); // Too short
    }

    #[test]
    fn test_scc_frame_rate_default() {
        assert_eq!(SccFrameRate::default(), SccFrameRate::Fps29_97);
    }

    #[test]
    fn test_scc_frame_rate_from_int() {
        assert_eq!(SccFrameRate::from_int(0), SccFrameRate::Fps29_97);
        assert_eq!(SccFrameRate::from_int(1), SccFrameRate::Fps24);
        assert_eq!(SccFrameRate::from_int(2), SccFrameRate::Fps25);
        assert_eq!(SccFrameRate::from_int(3), SccFrameRate::Fps30);
        assert_eq!(SccFrameRate::from_int(99), SccFrameRate::Fps29_97); // Invalid defaults
    }

    #[test]
    fn test_parse_smpte_timecode() {
        // 29.97fps non-drop-frame: 30 nominal frames take 1001ms.
        assert_eq!(
            parse_smpte_timecode("00:00:00:00", SccFrameRate::Fps29_97),
            Some(0)
        );
        assert_eq!(
            parse_smpte_timecode("00:00:01:00", SccFrameRate::Fps29_97),
            Some(1001)
        );
        assert_eq!(
            parse_smpte_timecode("00:01:00:00", SccFrameRate::Fps29_97),
            Some(60060)
        );
        assert_eq!(
            parse_smpte_timecode("01:00:00:00", SccFrameRate::Fps29_97),
            Some(3603600)
        );

        // Integer frame rates use the frame count, not wall-clock fields.
        assert_eq!(
            parse_smpte_timecode("00:00:01:00", SccFrameRate::Fps25),
            Some(1000)
        );
        assert_eq!(
            parse_smpte_timecode("00:00:01:00", SccFrameRate::Fps24),
            Some(1000)
        );
        assert_eq!(
            parse_smpte_timecode("00:00:01:00", SccFrameRate::Fps30),
            Some(1000)
        );

        // Drop-frame skips two frame labels at each minute except every tenth.
        assert_eq!(
            parse_smpte_timecode("00:00:01;00", SccFrameRate::Fps29_97),
            Some(1001)
        );
        assert_eq!(
            parse_smpte_timecode("00:10:00;00", SccFrameRate::Fps29_97),
            Some(599999)
        );
        assert_eq!(
            parse_smpte_timecode("01:00:00;00", SccFrameRate::Fps29_97),
            Some(3599996)
        );

        // Invalid dropped labels at minute boundaries.
        assert_eq!(
            parse_smpte_timecode("00:01:00;00", SccFrameRate::Fps29_97),
            None
        );
        assert_eq!(
            parse_smpte_timecode("00:01:00;01", SccFrameRate::Fps29_97),
            None
        );
        assert_eq!(
            parse_smpte_timecode("00:10:00;00", SccFrameRate::Fps29_97),
            Some(599999)
        );

        // Invalid frame, minute, and second components.
        for (timecode, rate) in [
            ("00:00:00:30", SccFrameRate::Fps29_97),
            ("00:00:00:24", SccFrameRate::Fps24),
            ("00:00:00:25", SccFrameRate::Fps25),
            ("00:00:00:30", SccFrameRate::Fps30),
            ("00:60:00:00", SccFrameRate::Fps29_97),
            ("00:00:60:00", SccFrameRate::Fps29_97),
            ("00:00:00;00", SccFrameRate::Fps25),
            ("00;00:00:00", SccFrameRate::Fps29_97),
            ("00:00:00:00:00", SccFrameRate::Fps29_97),
        ] {
            assert_eq!(parse_smpte_timecode(timecode, rate), None, "{timecode}");
        }

        // Reject overflow rather than wrapping arithmetic.
        assert_eq!(
            parse_smpte_timecode("18446744073709551615:00:00:00", SccFrameRate::Fps29_97),
            None
        );

        // Invalid shapes and non-numeric components.
        assert_eq!(
            parse_smpte_timecode("00:00:00", SccFrameRate::Fps29_97),
            None
        );
        assert_eq!(
            parse_smpte_timecode("invalid", SccFrameRate::Fps29_97),
            None
        );
        assert_eq!(parse_smpte_timecode("", SccFrameRate::Fps29_97), None);
    }

    #[test]
    fn test_parse_hex_pair() {
        assert_eq!(parse_hex_pair("9420"), Some([0x94, 0x20]));
        assert_eq!(parse_hex_pair("80ff"), Some([0x80, 0xFF]));
        assert_eq!(parse_hex_pair("0000"), Some([0x00, 0x00]));
        assert_eq!(parse_hex_pair("ABCD"), Some([0xAB, 0xCD])); // Uppercase
        assert_eq!(parse_hex_pair("xyz"), None); // Invalid
        assert_eq!(parse_hex_pair("94"), None); // Too short
        assert_eq!(parse_hex_pair("942020"), None); // Too long
        assert_eq!(parse_hex_pair(""), None); // Empty
    }

    #[test]
    fn test_parse_hex_pair_rejects_non_hex_tokens() {
        for token in ["a€", "€a", "😀", "éé", "+120", "94+0", "zzzz"] {
            assert_eq!(parse_hex_pair(token), None, "token: {token:?}");
        }
    }

    #[test]
    fn test_parse_scc_skips_unicode_tokens_and_keeps_valid_pairs() {
        let content = "Scenarist_SCC V1.0\n\n00:00:01:00\t9420 a€ 4849 😀 942f\n";
        let result = parse_scc(content, SccFrameRate::Fps30);
        assert_eq!(result.bytes_consumed, content.len());
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].time_ms, 1000);
        assert_eq!(
            result.lines[0].pairs,
            [[0x94, 0x20], [0x48, 0x49], [0x94, 0x2f]]
        );
    }

    #[test]
    fn test_parse_hex_pairs() {
        let pairs = parse_hex_pairs("9420 9420 94ad 94ad");
        assert_eq!(pairs.len(), 4);
        assert_eq!(pairs[0], [0x94, 0x20]);
        assert_eq!(pairs[1], [0x94, 0x20]);
        assert_eq!(pairs[2], [0x94, 0xAD]);
        assert_eq!(pairs[3], [0x94, 0xAD]);

        // With extra whitespace
        let pairs2 = parse_hex_pairs("  9420   9420  ");
        assert_eq!(pairs2.len(), 2);

        // Empty
        let pairs3 = parse_hex_pairs("");
        assert!(pairs3.is_empty());

        // Mixed valid and invalid
        let pairs4 = parse_hex_pairs("9420 invalid 94ad");
        assert_eq!(pairs4.len(), 2);
    }

    #[test]
    fn test_parse_scc_simple() {
        let content = "Scenarist_SCC V1.0\n\n00:00:00:00\t9420 9420\n00:00:01:00\t942c 942c\n";
        let result = parse_scc(content, SccFrameRate::Fps29_97);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].time_ms, 0);
        assert_eq!(result.lines[0].pairs.len(), 2);
        assert_eq!(result.lines[0].pairs[0], [0x94, 0x20]);
        assert_eq!(result.lines[1].time_ms, 1001);
        assert_eq!(result.lines[1].pairs[0], [0x94, 0x2C]);
    }

    #[test]
    fn test_parse_scc_with_bom() {
        let content = "\u{FEFF}Scenarist_SCC V1.0\n\n00:00:00:00\t9420\n";
        let result = parse_scc(content, SccFrameRate::Fps29_97);
        assert_eq!(result.lines.len(), 1);
    }

    #[test]
    fn test_parse_scc_empty_lines() {
        let content = "Scenarist_SCC V1.0\n\n\n\n00:00:00:00\t9420\n\n\n";
        let result = parse_scc(content, SccFrameRate::Fps29_97);
        assert_eq!(result.lines.len(), 1);
    }

    #[test]
    fn test_parse_scc_real_sample() {
        // Sample from the plan with actual caption commands
        // Note: "a1a0" is "!" with parity + space (proper 4-char hex pair)
        let content = r#"Scenarist_SCC V1.0

00:00:00:00	9420 9420 94ad 94ad 9470 9470 4869 2074 6865 7265 a1a0
00:00:02:00	942c 942c
00:00:04:00	9420 9420 9470 9470 5465 7374 696e 6720 5343 4320 696e 7075 742e
00:00:06:00	942c 942c
"#;
        let result = parse_scc(content, SccFrameRate::Fps29_97);
        assert_eq!(result.lines.len(), 4);

        // First line: "Hi there!" with positioning commands
        assert_eq!(result.lines[0].time_ms, 0);
        assert_eq!(result.lines[0].pairs.len(), 11);

        // Second line: EDM (Erase Displayed Memory)
        assert_eq!(result.lines[1].time_ms, 2002);
        assert_eq!(result.lines[1].pairs[0], [0x94, 0x2C]);

        // Third line: "Testing SCC input."
        assert_eq!(result.lines[2].time_ms, 4004);

        // Fourth line: EDM
        assert_eq!(result.lines[3].time_ms, 6006);
    }

    #[test]
    fn test_parse_scc_with_callbacks() {
        let content = "Scenarist_SCC V1.0\n\n00:00:00:00\t9420 9420\n00:00:01:00\t942c\n";

        let mut caption_count = 0;
        let mut timing_count = 0;
        let mut last_time_ms: i64 = -1;

        parse_scc_with_callbacks(
            content,
            SccFrameRate::Fps29_97,
            |cc_type, _d1, _d2| {
                assert_eq!(cc_type, CC_TYPE_FIELD1);
                caption_count += 1;
            },
            |time_ms| {
                timing_count += 1;
                last_time_ms = time_ms;
            },
        );

        assert_eq!(caption_count, 3); // 2 from first line + 1 from second
        assert_eq!(timing_count, 2); // One per line
        assert_eq!(last_time_ms, 1001); // 30 frames at 29.97 fps
    }

    #[test]
    fn test_ms_per_frame() {
        // 29.97fps: ~33.37ms per frame
        let fps_29_97 = SccFrameRate::Fps29_97.ms_per_frame();
        assert!(fps_29_97 > 33.0 && fps_29_97 < 34.0);

        // 24fps: ~41.67ms per frame
        let fps_24 = SccFrameRate::Fps24.ms_per_frame();
        assert!(fps_24 > 41.0 && fps_24 < 42.0);

        // 25fps: 40ms per frame
        let fps_25 = SccFrameRate::Fps25.ms_per_frame();
        assert!((fps_25 - 40.0).abs() < 0.001);

        // 30fps: ~33.33ms per frame
        let fps_30 = SccFrameRate::Fps30.ms_per_frame();
        assert!(fps_30 > 33.0 && fps_30 < 34.0);
    }
}
