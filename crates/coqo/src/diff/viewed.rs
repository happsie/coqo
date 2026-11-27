// SPDX-License-Identifier: Apache-2.0

//! "Viewed" marks and the remembered layout, kept in the workspace's state
//! document (`workspace/setState`), so they survive restarts and follow the
//! workspace to other windows.
//!
//! - [`MODE_KEY`]: `"split"` or `"unified"`, the last layout chosen.
//! - [`viewed_key`]: one entry per diff identity
//!   ([`DiffSource::identity`](super::DiffSource::identity)), mapping each
//!   viewed path to the [content hash](super::patch::content_hash) it was
//!   viewed at. A file whose change differs from that hash is not viewed.

use std::collections::BTreeMap;

use coqo_ui::serde_json::{Map, Value};

use super::patch::Fnv;

/// The workspace state entry of the remembered layout.
pub const MODE_KEY: &str = "ui.diff.mode";

/// The workspace state entry of `identity`'s viewed marks. Hashed, because
/// state keys are at most 128 characters and identities can be long.
pub fn viewed_key(identity: &str) -> String {
    format!("ui.diff.viewed.{:016x}", Fnv::hash(identity.as_bytes()))
}

/// Viewed paths of one diff, with the content hash each was viewed at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Viewed(BTreeMap<String, String>);

impl Viewed {
    /// Read a stored entry; anything unexpected reads as nothing viewed.
    pub fn from_value(value: Option<&Value>) -> Self {
        let map = value
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Self(map)
    }

    /// The entry to store; `null` (removing it) when nothing is viewed.
    pub fn to_value(&self) -> Value {
        if self.0.is_empty() {
            return Value::Null;
        }
        Value::Object(
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect::<Map<_, _>>(),
        )
    }

    /// Whether `path` was viewed with exactly this change.
    pub fn is_viewed(&self, path: &str, hash: &str) -> bool {
        self.0.get(path).is_some_and(|h| h == hash)
    }

    /// Mark `path` viewed at `hash`, or unmark it.
    pub fn set(&mut self, path: &str, hash: &str, viewed: bool) {
        if viewed {
            self.0.insert(path.to_owned(), hash.to_owned());
        } else {
            self.0.remove(path);
        }
    }

    /// Forget marks of paths that are no longer in the diff, or whose
    /// change moved on (a `None` hash, not loaded, keeps its mark). Returns
    /// whether anything was forgotten.
    pub fn retain_current<'a>(
        &mut self,
        current: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    ) -> bool {
        let current: BTreeMap<&str, Option<&str>> = current.into_iter().collect();
        let before = self.0.len();
        self.0
            .retain(|path, hash| match current.get(path.as_str()) {
                None => false,
                Some(None) => true,
                Some(Some(now)) => *now == hash.as_str(),
            });
        self.0.len() != before
    }

    /// How many paths are marked.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}
