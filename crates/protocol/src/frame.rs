// SPDX-License-Identifier: Apache-2.0

//! Length-prefixed framing.
//!
//! Every message on a Coqo Protocol connection is one frame: a 4-byte
//! big-endian unsigned length followed by exactly that many payload bytes.
//! The payload is one encoded [`Message`](crate::Message) (JSON or
//! MessagePack, see [`Encoding`](crate::Encoding)).
//!
//! The length is checked against a maximum *before* any payload is
//! buffered, so a peer that is not speaking the protocol (or a corrupt
//! stream) fails fast with [`FrameError::TooLarge`] instead of making the
//! reader wait for gigabytes that never arrive.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Size of the length prefix in bytes.
pub const HEADER_LEN: usize = 4;

/// Default maximum payload size of a frame after the handshake (16 MiB).
pub const DEFAULT_MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Maximum payload size while the handshake is in progress (64 KiB).
///
/// A `coqo/hello` exchange is tiny; anything bigger means the peer is not a
/// Coqo Protocol endpoint.
pub const HANDSHAKE_MAX_FRAME_LEN: usize = 64 * 1024;

/// Errors produced by [`FrameCodec`].
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The underlying transport failed.
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),
    /// A frame announced (or tried to send) a payload above the limit.
    #[error("frame of {len} bytes exceeds the {max}-byte limit")]
    TooLarge {
        /// Announced payload length.
        len: usize,
        /// Configured maximum payload length.
        max: usize,
    },
}

/// A [`tokio_util::codec`] codec for Coqo Protocol frames.
///
/// Decodes to the raw payload bytes; encoding the payload into a
/// [`Message`](crate::Message) is a separate step (see
/// [`Encoding`](crate::Encoding)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameCodec {
    max_frame_len: usize,
}

impl FrameCodec {
    /// A codec with the given maximum payload length.
    pub fn new(max_frame_len: usize) -> Self {
        Self { max_frame_len }
    }

    /// The maximum payload length.
    pub fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    /// Change the maximum payload length (used after the handshake).
    pub fn set_max_frame_len(&mut self, max_frame_len: usize) {
        self.max_frame_len = max_frame_len;
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_LEN)
    }
}

impl Decoder for FrameCodec {
    type Item = Bytes;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Bytes>, FrameError> {
        if src.len() < HEADER_LEN {
            src.reserve(HEADER_LEN - src.len());
            return Ok(None);
        }
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&src[..HEADER_LEN]);
        let len = u32::from_be_bytes(header) as usize;
        if len > self.max_frame_len {
            return Err(FrameError::TooLarge {
                len,
                max: self.max_frame_len,
            });
        }
        let total = HEADER_LEN + len;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        src.advance(HEADER_LEN);
        Ok(Some(src.split_to(len).freeze()))
    }
}

impl Encoder<Bytes> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, payload: Bytes, dst: &mut BytesMut) -> Result<(), FrameError> {
        self.encode(payload.as_ref(), dst)
    }
}

impl Encoder<&[u8]> for FrameCodec {
    type Error = FrameError;

    fn encode(&mut self, payload: &[u8], dst: &mut BytesMut) -> Result<(), FrameError> {
        let len = payload.len();
        if len > self.max_frame_len || u32::try_from(len).is_err() {
            return Err(FrameError::TooLarge {
                len,
                max: self.max_frame_len,
            });
        }
        dst.reserve(HEADER_LEN + len);
        dst.put_u32(len as u32);
        dst.put_slice(payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn encode_all(codec: &mut FrameCodec, payloads: &[Vec<u8>]) -> BytesMut {
        let mut buf = BytesMut::new();
        for p in payloads {
            codec.encode(p.as_slice(), &mut buf).unwrap();
        }
        buf
    }

    #[test]
    fn encodes_big_endian_length_prefix() {
        let mut codec = FrameCodec::default();
        let buf = encode_all(&mut codec, &[b"hi".to_vec()]);
        assert_eq!(&buf[..], &[0, 0, 0, 2, b'h', b'i']);
    }

    #[test]
    fn round_trips_empty_frame() {
        let mut codec = FrameCodec::default();
        let mut buf = encode_all(&mut codec, &[Vec::new()]);
        assert_eq!(codec.decode(&mut buf).unwrap().unwrap().len(), 0);
        assert!(buf.is_empty());
    }

    #[test]
    fn waits_for_partial_header_and_payload() {
        let mut codec = FrameCodec::default();
        let mut buf = BytesMut::from(&[0u8, 0][..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&[0, 3, b'a']);
        assert!(codec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"bc");
        assert_eq!(&codec.decode(&mut buf).unwrap().unwrap()[..], b"abc");
    }

    #[test]
    fn rejects_oversized_header_before_buffering_payload() {
        let mut codec = FrameCodec::new(8);
        let mut buf = BytesMut::from(&[0u8, 0, 0, 9][..]);
        match codec.decode(&mut buf) {
            Err(FrameError::TooLarge { len: 9, max: 8 }) => {}
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn rejects_oversized_payload_on_encode() {
        let mut codec = FrameCodec::new(2);
        let mut buf = BytesMut::new();
        assert!(matches!(
            codec.encode(&b"abc"[..], &mut buf),
            Err(FrameError::TooLarge { len: 3, max: 2 })
        ));
        assert!(buf.is_empty());
    }

    #[test]
    fn non_protocol_bytes_fail_fast() {
        // An HTTP request on the socket reads as a ~1.2 GB frame length.
        let mut codec = FrameCodec::new(HANDSHAKE_MAX_FRAME_LEN);
        let mut buf = BytesMut::from(&b"GET / HTTP/1.1\r\n"[..]);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(FrameError::TooLarge { .. })
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Any sequence of frames, delivered in arbitrary chunks, decodes to
        /// exactly the payloads that were encoded.
        #[test]
        fn fuzz_round_trip_with_arbitrary_chunking(
            payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..512), 0..16),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..32),
        ) {
            let mut codec = FrameCodec::new(1024);
            let wire = encode_all(&mut codec, &payloads);

            let mut cut_points: Vec<usize> = cuts.iter().map(|i| i.index(wire.len() + 1)).collect();
            cut_points.push(0);
            cut_points.push(wire.len());
            cut_points.sort_unstable();
            cut_points.dedup();

            let mut buf = BytesMut::new();
            let mut decoded = Vec::new();
            for w in cut_points.windows(2) {
                buf.extend_from_slice(&wire[w[0]..w[1]]);
                while let Some(frame) = codec.decode(&mut buf).unwrap() {
                    decoded.push(frame.to_vec());
                }
            }
            prop_assert!(buf.is_empty());
            prop_assert_eq!(decoded, payloads);
        }

        /// Arbitrary bytes never panic, never yield a frame above the limit,
        /// and never leave the decoder holding more than one frame's worth of
        /// buffered input without either producing a frame or an error.
        #[test]
        fn fuzz_arbitrary_input_never_panics(
            input in prop::collection::vec(any::<u8>(), 0..4096),
            max in 0usize..2048,
        ) {
            let mut codec = FrameCodec::new(max);
            let mut buf = BytesMut::from(&input[..]);
            loop {
                match codec.decode(&mut buf) {
                    Ok(Some(frame)) => prop_assert!(frame.len() <= max),
                    Ok(None) => {
                        prop_assert!(buf.len() < HEADER_LEN + max);
                        break;
                    }
                    Err(FrameError::TooLarge { len, max: m }) => {
                        prop_assert_eq!(m, max);
                        prop_assert!(len > max);
                        break;
                    }
                    Err(FrameError::Io(e)) => panic!("unexpected io error {e}"),
                }
            }
        }
    }
}
