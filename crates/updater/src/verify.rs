// SPDX-License-Identifier: Apache-2.0

//! Minisign signatures and sha256.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use minisign_verify::Signature;
use sha2::{Digest as _, Sha256};

use crate::{TrustedKey, UpdateError};

/// Check that `signature` (the text of a `.minisig` file) signs `data`
/// with `key`. Both minisign modes verify: prehashed (BLAKE2b-512, the
/// default since minisign 0.8) and plain Ed25519 ("legacy"); both are
/// Ed25519 signatures by the same key.
pub fn signature(
    key: &TrustedKey,
    data: &[u8],
    signature: &str,
    what: &str,
) -> Result<(), UpdateError> {
    let bad = || UpdateError::BadSignature {
        what: what.to_owned(),
    };
    let signature = Signature::decode(signature).map_err(|_| bad())?;
    key.0.verify(data, &signature, true).map_err(|_| bad())
}

/// The sha256 of `data`, in lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// The sha256 of the file at `path`, in lowercase hex, and its size.
pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_nothing() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// minisign's own test vector: a legacy (plain Ed25519) signature.
    #[test]
    fn minisigns_test_vector_verifies_and_a_flipped_byte_does_not() {
        let key = TrustedKey::parse(
            "untrusted comment: minisign public key E7620F1842B4E81F\n\
             RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n",
        )
        .unwrap();
        let sig = "untrusted comment: signature from minisign secret key\n\
                   RWQf6LRCGA9i59SLOFxz6NxvASXDJeRtuZykwQepbDEGt87ig1BNpWaVWuNrm73YiIiJbq71Wi+dP9eKL8OC351vwIasSSbXxwA=\n\
                   trusted comment: timestamp:1555779966\tfile:test\n\
                   QtKMXWyYcwdpZAlPF7tE2ENJkRd1ujvKjlj1m9RtHTBnZPa5WKU5uWRs5GoP5M/VqE81QFuMKI5k/SfNQUaOAA==\n";
        signature(&key, b"test", sig, "the test").unwrap();
        let e = signature(&key, b"tesT", sig, "the test").unwrap_err();
        assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");
        let e = signature(&key, b"test", "garbage", "the test").unwrap_err();
        assert!(matches!(e, UpdateError::BadSignature { .. }), "{e}");
    }
}
