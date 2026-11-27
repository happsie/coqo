// SPDX-License-Identifier: Apache-2.0

//! File contents ↔ buffer text: encoding and line-ending detection.
//!
//! Only UTF-8 (with or without a byte-order mark) is supported. Line breaks
//! are kept exactly as they are; the detected convention is reported so the
//! editor inserts matching breaks.

use coqo_protocol::methods::buffer::{LineEnding, TextEncoding};

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// A file's contents decoded for a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The text, without a byte-order mark.
    pub text: String,
    /// How it was encoded.
    pub encoding: TextEncoding,
    /// Its line-ending convention.
    pub line_ending: LineEnding,
}

/// The file is not valid UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not valid UTF-8 (first bad byte at offset {offset}); only UTF-8 files can be edited")]
pub struct NotUtf8 {
    /// Offset of the first invalid byte in the file.
    pub offset: usize,
}

/// Decode a file's bytes.
pub fn decode(bytes: Vec<u8>) -> Result<Decoded, NotUtf8> {
    let (encoding, bytes) = match bytes.strip_prefix(BOM) {
        Some(rest) => (TextEncoding::Utf8Bom, rest.to_vec()),
        None => (TextEncoding::Utf8, bytes),
    };
    let skipped = if encoding == TextEncoding::Utf8Bom {
        BOM.len()
    } else {
        0
    };
    let text = String::from_utf8(bytes).map_err(|e| NotUtf8 {
        offset: skipped + e.utf8_error().valid_up_to(),
    })?;
    let line_ending = detect_line_ending(&text);
    Ok(Decoded {
        text,
        encoding,
        line_ending,
    })
}

/// The convention of the first line break (`\n` if there is none).
pub fn detect_line_ending(text: &str) -> LineEnding {
    match text.find('\n') {
        Some(i) if i > 0 && text.as_bytes()[i - 1] == b'\r' => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// The bytes to write for `text` in `encoding`.
pub fn encode(text: &str, encoding: TextEncoding) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + BOM.len());
    if encoding == TextEncoding::Utf8Bom {
        out.extend_from_slice(BOM);
    }
    out.extend_from_slice(text.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_with_and_without_bom() {
        let d = decode(b"a\r\nb\n".to_vec()).unwrap();
        assert_eq!(d.text, "a\r\nb\n");
        assert_eq!(d.encoding, TextEncoding::Utf8);
        assert_eq!(d.line_ending, LineEnding::Crlf);

        let d = decode(b"\xEF\xBB\xBFh\xC3\xA9\n".to_vec()).unwrap();
        assert_eq!(d.text, "hé\n");
        assert_eq!(d.encoding, TextEncoding::Utf8Bom);
        assert_eq!(d.line_ending, LineEnding::Lf);
        assert_eq!(encode(&d.text, d.encoding), b"\xEF\xBB\xBFh\xC3\xA9\n");

        assert_eq!(decode(Vec::new()).unwrap().line_ending, LineEnding::Lf);
    }

    #[test]
    fn other_encodings_are_refused() {
        // Latin-1 "é".
        assert_eq!(decode(b"caf\xE9".to_vec()), Err(NotUtf8 { offset: 3 }));
        assert_eq!(
            decode(b"\xEF\xBB\xBF\xFF".to_vec()),
            Err(NotUtf8 { offset: 3 })
        );
    }
}
