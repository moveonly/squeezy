//! Serialize a [`CaptureLog`] to the JSON the out-of-process xterm.js oracle
//! (`tools/termsim/xtermcheck/replay.js`) consumes.
//!
//! The oracle's contract (documented in `replay.js`) is:
//!
//! ```json
//! {
//!   "bytes_hex": "1b5b...",            // OR "bytes_base64"
//!   "frames": [ { "byte_offset": N, "w": COLS, "h": ROWS }, ... ]
//! }
//! ```
//!
//! We emit `bytes_hex` so this leg needs no base64 dependency — the oracle
//! accepts either encoding. Frame *i*'s bytes are
//! `bytes[frames[i-1].byte_offset .. frames[i].byte_offset]`, matching the
//! self-slicing contract on both sides.

use std::io;
use std::path::Path;

use super::types::CaptureLog;

/// Lower-case hex-encode `bytes` with no separators (the encoding
/// `replay.js`'s `bytes_hex` branch expects).
fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

/// Serialize a [`CaptureLog`] to the xtermcheck JSON string (hex byte
/// encoding).
pub(crate) fn capture_log_to_json(log: &CaptureLog) -> String {
    let frames: Vec<serde_json::Value> = log
        .frames
        .iter()
        .map(|f| {
            serde_json::json!({
                "byte_offset": f.byte_offset,
                "w": f.w,
                "h": f.h,
            })
        })
        .collect();
    let value = serde_json::json!({
        "bytes_hex": to_hex(&log.bytes),
        "frames": frames,
    });
    // Pretty-print so an exported fixture is diffable / human-readable; the
    // oracle parses either form.
    serde_json::to_string_pretty(&value).expect("CaptureLog JSON serializes")
}

/// Write a [`CaptureLog`] as xtermcheck JSON to `path`.
pub(crate) fn export_capture_log(log: &CaptureLog, path: &Path) -> io::Result<()> {
    std::fs::write(path, capture_log_to_json(log))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termsim::types::FrameMark;

    fn sample_log() -> CaptureLog {
        CaptureLog {
            bytes: b"\x1b[?2026hAB\r\n\x1b[?2026l".to_vec(),
            frames: vec![FrameMark {
                byte_offset: 13,
                w: 80,
                h: 24,
            }],
        }
    }

    #[test]
    fn hex_round_trips_byte_values() {
        assert_eq!(to_hex(&[0x00, 0x1b, 0x5b, 0xff]), "001b5bff");
    }

    #[test]
    fn json_carries_hex_and_frames() {
        let json = capture_log_to_json(&sample_log());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        // bytes_hex decodes back to the original stream.
        let hex = parsed["bytes_hex"].as_str().expect("bytes_hex string");
        let decoded: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(decoded, sample_log().bytes);

        // The single frame mark survives with its size + offset.
        let frames = parsed["frames"].as_array().expect("frames array");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["byte_offset"], 13);
        assert_eq!(frames[0]["w"], 80);
        assert_eq!(frames[0]["h"], 24);
    }

    /// Writes a real exported fixture so the xterm.js leg has a sample to
    /// consume, and proves the round trip end to end through a file. Ignored
    /// by default (it touches the temp dir); run explicitly with
    /// `--ignored` to regenerate the sample export.
    #[test]
    #[ignore = "writes a sample export to the temp dir; run with --ignored"]
    fn writes_sample_export_to_disk() {
        use crate::termsim::driver::run_scenario;
        use crate::termsim::scenario::shipped_scenarios;

        let scenario = shipped_scenarios()
            .into_iter()
            .find(|s| s.name == "single_turn")
            .expect("single_turn is shipped");
        let run = run_scenario(&scenario);

        let path = std::env::temp_dir().join("squeezy_termsim_single_turn_capturelog.json");
        export_capture_log(&run.log, &path).expect("export writes the JSON file");

        let written = std::fs::read_to_string(&path).expect("read back the export");
        let parsed: serde_json::Value = serde_json::from_str(&written).expect("valid JSON on disk");
        assert!(parsed["bytes_hex"].is_string());
        assert!(parsed["frames"].is_array());
        assert!(
            !parsed["frames"].as_array().unwrap().is_empty(),
            "the scenario painted at least one frame",
        );
        // Keep the file when regenerating a fixture for the xterm.js leg
        // (`KEEP_EXPORT=1`); otherwise clean up so the test leaves no residue.
        if std::env::var_os("KEEP_EXPORT").is_none() {
            let _ = std::fs::remove_file(&path);
        } else {
            eprintln!("kept exported CaptureLog at {}", path.display());
        }
    }
}
