//! Coqo Protocol framing for the spike: JSON-RPC 2.0 notifications in u32 length-prefixed
//! frames, encoded as JSON or MessagePack (PLAN §4.4 makes MessagePack negotiable for hot
//! paths). Binary payloads (yrs updates) are base64 in JSON and raw bytes in MessagePack.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::ot::Edit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Json,
    MsgPack,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Msg {
    /// Model A, client -> daemon: a local change against `base_rev`.
    #[serde(rename = "buffer/edit")]
    Edit {
        buffer: u64,
        base_rev: u64,
        seq: u64,
        edits: Vec<Edit>,
    },
    /// Model A, daemon -> every subscriber (the origin's copy is its ack).
    #[serde(rename = "buffer/changed")]
    Changed {
        buffer: u64,
        rev: u64,
        origin: u32,
        seq: u64,
        edits: Vec<Edit>,
    },
    /// Model B, both directions: a yrs v1 update.
    #[serde(rename = "buffer/update")]
    Update {
        buffer: u64,
        origin: u32,
        #[serde(with = "bytes")]
        update: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    jsonrpc: String,
    #[serde(flatten)]
    msg: Msg,
}

pub fn encode(msg: &Msg, enc: Encoding) -> Vec<u8> {
    let env = Envelope {
        jsonrpc: "2.0".into(),
        msg: msg.clone(),
    };
    match enc {
        Encoding::Json => serde_json::to_vec(&env).unwrap(),
        Encoding::MsgPack => rmp_serde::to_vec_named(&env).unwrap(),
    }
}

pub fn decode(buf: &[u8], enc: Encoding) -> Msg {
    let env: Envelope = match enc {
        Encoding::Json => serde_json::from_slice(buf).unwrap(),
        Encoding::MsgPack => rmp_serde::from_slice(buf).unwrap(),
    };
    env.msg
}

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

mod bytes {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.serialize_str(&b64::encode(v))
        } else {
            serde_bytes::serialize(v, s)
        }
    }

    /// Accepts base64 text or raw bytes: serde's `flatten` buffers the params and then
    /// reports `is_human_readable() == true` even for MessagePack, so don't trust it here.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("base64 string or bytes")
            }
            fn visit_str<E>(self, v: &str) -> Result<Vec<u8>, E> {
                Ok(b64::decode(v))
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
        }
        d.deserialize_any(V)
    }

    /// Tiny standard-alphabet base64, to avoid another dependency in a throwaway spike.
    mod b64 {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

        pub fn encode(v: &[u8]) -> String {
            let mut out = String::with_capacity(v.len().div_ceil(3) * 4);
            for c in v.chunks(3) {
                let n = (c[0] as u32) << 16
                    | (*c.get(1).unwrap_or(&0) as u32) << 8
                    | *c.get(2).unwrap_or(&0) as u32;
                for i in 0..4 {
                    if i <= c.len() {
                        out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }

        pub fn decode(s: &str) -> Vec<u8> {
            let mut rev = [0u8; 256];
            for (i, &c) in T.iter().enumerate() {
                rev[c as usize] = i as u8;
            }
            let b = s.trim_end_matches('=').as_bytes();
            let mut out = Vec::with_capacity(b.len() * 3 / 4);
            for c in b.chunks(4) {
                let mut n = 0u32;
                for (i, &x) in c.iter().enumerate() {
                    n |= (rev[x as usize] as u32) << (18 - 6 * i);
                }
                for i in 0..c.len() - 1 {
                    out.push((n >> (16 - 8 * i)) as u8);
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_both_encodings() {
        for enc in [Encoding::Json, Encoding::MsgPack] {
            let m = Msg::Update {
                buffer: 7,
                origin: 2,
                update: vec![0, 1, 2, 250, 251, 252, 253],
            };
            match decode(&encode(&m, enc), enc) {
                Msg::Update { update, .. } => assert_eq!(update, vec![0, 1, 2, 250, 251, 252, 253]),
                _ => panic!(),
            }
            let m = Msg::Edit {
                buffer: 1,
                base_rev: 3,
                seq: 4,
                edits: vec![Edit::new(1..2, "é")],
            };
            assert!(matches!(
                decode(&encode(&m, enc), enc),
                Msg::Edit { seq: 4, .. }
            ));
        }
        let json = String::from_utf8(encode(
            &Msg::Edit {
                buffer: 1,
                base_rev: 3,
                seq: 4,
                edits: vec![Edit::new(10..10, "x")],
            },
            Encoding::Json,
        ))
        .unwrap();
        assert!(
            json.starts_with(r#"{"jsonrpc":"2.0","method":"buffer/edit","params":"#),
            "{json}"
        );
    }
}
