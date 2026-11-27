// SPDX-License-Identifier: Apache-2.0

//! `coqo.hello`: a sample core mod that uses only `coqo-mod-api`.
//!
//! On activation it registers the command `hello.greet` and the status bar
//! item `hello.status`. Each greeting updates the status item's text.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use coqo_mod_api::contrib::{Command, StatusItem};
use coqo_mod_api::serde_json::Value;
use coqo_mod_api::{BoxFuture, Manifest, Mod, ModContext, ModError, points};

/// The mod's `coqo.mod.toml`.
pub const MANIFEST: &str = include_str!("../coqo.mod.toml");

/// Command id of "Say hello".
pub const GREET: &str = "hello.greet";

/// Id of the status bar item.
pub const STATUS: &str = "hello.status";

/// The parsed, validated [`MANIFEST`].
pub fn manifest() -> Manifest {
    Manifest::from_toml_str(MANIFEST).expect("hello's manifest is valid")
}

/// The mod.
#[derive(Debug, Default)]
pub struct Hello {
    greetings: Arc<AtomicU64>,
}

impl Hello {
    /// A fresh instance that has greeted nobody.
    pub fn new() -> Self {
        Self::default()
    }
}

fn status_item(greetings: u64) -> Result<StatusItem, ModError> {
    let text = match greetings {
        0 => "Hello".to_owned(),
        n => format!("Hello ×{n}"),
    };
    Ok(StatusItem::new(STATUS.parse()?, text)
        .with_tooltip("Say hello")
        .with_command(GREET.parse()?))
}

impl Mod for Hello {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.contribute::<points::StatusItems>(status_item(0)?)?;

            let greetings = Arc::clone(&self.greetings);
            let handler_cx = cx.clone();
            let command = Command::new(GREET.parse()?, "Say hello").with_category("Hello");
            cx.register_command(command, move |args: Value| {
                let greetings = Arc::clone(&greetings);
                let cx = handler_cx.clone();
                async move {
                    let name = args.get("name").and_then(Value::as_str).unwrap_or("world");
                    let n = greetings.fetch_add(1, Ordering::SeqCst) + 1;
                    cx.contribute::<points::StatusItems>(status_item(n)?)?;
                    Ok(Value::from(format!("Hello, {name}!")))
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_mod_api::manifest::{ActivationEvent, Tier};
    use coqo_mod_api::serde_json::json;
    use coqo_mod_api::testing::{TestHost, block_on};

    #[test]
    fn manifest_is_a_core_mod_activated_at_startup() {
        let m = manifest();
        assert_eq!(m.id.as_str(), "coqo.hello");
        assert_eq!(m.tier, Tier::Core);
        assert!(m.activates_on(&ActivationEvent::Startup));
        assert!(m.supports_api(&coqo_mod_api::api_version()));
    }

    #[test]
    fn registers_a_command_and_a_status_item_through_the_api() {
        let m = manifest();
        let host = TestHost::new();
        let hello = Hello::new();
        block_on(hello.activate(host.context(m.id.clone()))).unwrap();

        let command = host.contribution::<points::Commands>(GREET).unwrap();
        assert_eq!(command.title, "Say hello");
        let status = host.contribution::<points::StatusItems>(STATUS).unwrap();
        assert_eq!(status.text, "Hello");
        assert_eq!(status.command.as_ref().unwrap().as_str(), GREET);
        let providers: Vec<_> = host
            .contributions::<points::StatusItems>()
            .into_iter()
            .map(|c| c.provider)
            .collect();
        assert_eq!(providers, std::slice::from_ref(&m.id));

        assert_eq!(
            host.run_command(GREET, json!({ "name": "Ada" })).unwrap(),
            "Hello, Ada!"
        );
        assert_eq!(
            host.run_command(GREET, Value::Null).unwrap(),
            "Hello, world!"
        );
        let status = host.contribution::<points::StatusItems>(STATUS).unwrap();
        assert_eq!(status.text, "Hello ×2");

        // Deactivation: the host withdraws the command, its handler and the item.
        block_on(hello.deactivate()).unwrap();
        assert_eq!(host.withdraw_mod(&m.id), 3);
    }
}
