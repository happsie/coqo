// SPDX-License-Identifier: Apache-2.0

//! The MCP resources every `coqo-editor` server offers: fixed documents
//! agents read on demand (`resources/list`, `resources/read`).
//!
//! | URI | What |
//! |---|---|
//! | `coqo://schemas/walkthrough.json` | JSON Schema of what `publish_walkthrough` takes |

use coqo_protocol::methods::walkthroughs;

/// One resource.
#[derive(Debug)]
pub struct StaticResource {
    /// Its URI.
    pub uri: &'static str,
    /// Its programmatic name.
    pub name: &'static str,
    /// Display title.
    pub title: &'static str,
    /// What it is.
    pub description: &'static str,
    /// Its media type.
    pub mime_type: &'static str,
    /// Its contents.
    pub text: &'static str,
}

const ALL: &[StaticResource] = &[StaticResource {
    uri: walkthroughs::SCHEMA_URI,
    name: "walkthrough.schema.json",
    title: "Walkthrough schema",
    description: "JSON Schema of a code walkthrough as publish_walkthrough takes it: steps in \
                  reading order, anchored to code, with what, why and risks.",
    mime_type: "application/schema+json",
    text: walkthroughs::SCHEMA_JSON,
}];

/// Every resource.
pub fn all() -> &'static [StaticResource] {
    ALL
}
