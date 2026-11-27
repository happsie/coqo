// SPDX-License-Identifier: Apache-2.0

//! `ModContext` against the in-memory `TestHost`: typed services, typed
//! contribution points and command handlers, as a built-in mod uses them.

use std::sync::Arc;

use coqo_mod_api::contrib::{Command, Keybinding, StatusItem};
use coqo_mod_api::serde_json::{Value, json};
use coqo_mod_api::testing::{TestHost, block_on};
use coqo_mod_api::{
    BoxFuture, Mod, ModContext, ModError, ProviderId, Service, ServiceVersion, points,
};

/// `test.greeter@1.1`.
trait Greeter: Send + Sync {
    fn greet(&self, name: &str) -> String;
}

impl Service for dyn Greeter {
    const ID: &'static str = "test.greeter";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 1);
}

/// Same id, but a consumer that needs minor version 2.
trait NewerGreeter: Send + Sync {}
impl Service for dyn NewerGreeter {
    const ID: &'static str = "test.greeter";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 2);
}

/// Same id and version as `Greeter`, but a different Rust type.
trait OtherGreeter: Send + Sync {}
impl Service for dyn OtherGreeter {
    const ID: &'static str = "test.greeter";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
}

struct Lang(&'static str);
impl Greeter for Lang {
    fn greet(&self, name: &str) -> String {
        format!("{} {name}", self.0)
    }
}

fn pid(s: &str) -> ProviderId {
    s.parse().unwrap()
}

#[test]
fn services_are_typed_versioned_and_replaceable() {
    let host = TestHost::new();
    let builtin = host.context(pid("coqo.greeter"));
    let user = host.context(pid("user.greeter"));
    let consumer = host.context(pid("consumer"));

    assert!(matches!(
        consumer.consume::<dyn Greeter>(),
        Err(ModError::ServiceNotFound(_))
    ));

    builtin
        .provide::<dyn Greeter>(Arc::new(Lang("Hello")))
        .unwrap();
    assert_eq!(
        consumer.consume::<dyn Greeter>().unwrap().greet("Ada"),
        "Hello Ada"
    );

    // A newer minor is required: 1.1 doesn't satisfy 1.2.
    let err = consumer.consume::<dyn NewerGreeter>().err().unwrap();
    assert!(
        matches!(err, ModError::ServiceVersionMismatch { .. }),
        "{err}"
    );
    assert!(err.to_string().contains("active versions: 1.1"), "{err}");

    // Same service, wrong Rust type.
    let err = consumer.consume::<dyn OtherGreeter>().err().unwrap();
    assert!(matches!(err, ModError::ServiceTypeMismatch { .. }), "{err}");

    // Another mod replaces the provider; withdrawing it brings the first back.
    user.provide::<dyn Greeter>(Arc::new(Lang("Hej"))).unwrap();
    assert_eq!(
        consumer.consume::<dyn Greeter>().unwrap().greet("Ada"),
        "Hej Ada"
    );
    user.withdraw_service::<dyn Greeter>().unwrap();
    assert_eq!(
        consumer.consume::<dyn Greeter>().unwrap().greet("Ada"),
        "Hello Ada"
    );
}

#[test]
fn commands_and_contributions_round_trip() {
    let host = TestHost::new();
    let cx = host.context(pid("calc"));

    cx.register_command(
        Command::new("calc.double".parse().unwrap(), "Double").with_category("Calc"),
        |args: Value| async move {
            let n = args["n"]
                .as_i64()
                .ok_or_else(|| ModError::failed("need `n`"))?;
            Ok(json!(n * 2))
        },
    )
    .unwrap();
    assert_eq!(
        host.run_command("calc.double", json!({ "n": 21 })).unwrap(),
        json!(42)
    );
    let err = host.run_command("calc.double", Value::Null).unwrap_err();
    assert_eq!(err.to_string(), "need `n`");
    assert!(matches!(
        host.run_command("calc.missing", Value::Null),
        Err(ModError::UnknownCommand(_))
    ));
    let command = host
        .contribution::<points::Commands>("calc.double")
        .unwrap();
    assert_eq!(command.category.as_deref(), Some("Calc"));

    // Keyless values take an explicit id.
    let binding = Keybinding {
        command: "calc.double".parse().unwrap(),
        key: "ctrl+d".into(),
        mac: None,
        when: None,
        args: Some(json!({ "n": 1 })),
    };
    cx.contribute_as::<points::Keybindings>("calc.double-key".parse().unwrap(), binding)
        .unwrap();
    assert_eq!(host.contributions::<points::Keybindings>().len(), 1);

    // Contributing the same id again updates in place.
    let status = |text: &str| StatusItem::new("calc.status".parse().unwrap(), text);
    cx.contribute::<points::StatusItems>(status("0")).unwrap();
    cx.contribute::<points::StatusItems>(status("1")).unwrap();
    let items = host.contributions::<points::StatusItems>();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].value.text, "1");

    // Another mod can't take the id.
    let other = host.context(pid("other"));
    assert!(matches!(
        other.contribute::<points::StatusItems>(status("x")),
        Err(ModError::DuplicateContribution { .. })
    ));

    assert!(
        cx.withdraw::<points::StatusItems>(&"calc.status".parse().unwrap())
            .unwrap()
    );
    assert!(host.contributions::<points::StatusItems>().is_empty());
    assert_eq!(host.withdraw_mod(&pid("calc")), 3);
}

/// A built-in mod holding state and a kept `ModContext`, the way an agent
/// provider or the files mod would.
struct Counter {
    cx: std::sync::OnceLock<ModContext>,
}

impl Mod for Counter {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.provide::<dyn Greeter>(Arc::new(Lang("Hi")))?;
            cx.contribute::<points::StatusItems>(StatusItem::new(
                "counter.status".parse()?,
                "ready",
            ))?;
            self.cx
                .set(cx)
                .map_err(|_| ModError::failed("activated twice"))
        })
    }

    fn deactivate(&self) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async {
            let cx = self
                .cx
                .get()
                .ok_or_else(|| ModError::failed("not active"))?;
            cx.contribute::<points::StatusItems>(StatusItem::new(
                "counter.status".parse()?,
                "stopping",
            ))
        })
    }
}

#[test]
fn mods_activate_and_deactivate_through_the_trait_object() {
    let host = TestHost::new();
    let m: Box<dyn Mod> = Box::new(Counter {
        cx: std::sync::OnceLock::new(),
    });
    block_on(m.activate(host.context(pid("counter")))).unwrap();
    assert_eq!(host.services().len(), 1);
    assert_eq!(
        host.contribution::<points::StatusItems>("counter.status")
            .unwrap()
            .text,
        "ready"
    );
    block_on(m.deactivate()).unwrap();
    assert_eq!(
        host.contribution::<points::StatusItems>("counter.status")
            .unwrap()
            .text,
        "stopping"
    );
    // The host cleans up after deactivation.
    assert_eq!(host.withdraw_mod(&pid("counter")), 2);
    assert!(host.services().is_empty());
}
