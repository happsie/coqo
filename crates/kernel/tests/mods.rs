// SPDX-License-Identifier: Apache-2.0

//! Mods running through the kernel's `ModHost`: `mods/hello` and test mods
//! that provide services, fail, panic or activate lazily.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use coqo_kernel::mods::{BuiltinMods, ModManager, ModState};
use coqo_kernel::{
    ContributionId, ContributionRegistry, ProviderId, ProviderKind, ServiceRegistry,
};
use coqo_mod_api::contrib::{Command, StatusItem};
use coqo_mod_api::serde_json::{Value, json};
use coqo_mod_api::{
    BoxFuture, ContributionPoint, Manifest, Mod, ModContext, ModError, Service, ServiceVersion,
    points,
};
use coqo_mod_hello::{GREET, Hello, STATUS};

struct Kernel {
    services: Arc<ServiceRegistry>,
    contributions: Arc<ContributionRegistry>,
    mods: ModManager,
}

fn kernel() -> Kernel {
    let services = Arc::new(ServiceRegistry::new());
    let contributions = Arc::new(ContributionRegistry::new());
    let mods = ModManager::with_registries(Arc::clone(&services), Arc::clone(&contributions));
    Kernel {
        services,
        contributions,
        mods,
    }
}

fn pid(s: &str) -> ProviderId {
    s.parse().unwrap()
}

fn cid(s: &str) -> ContributionId {
    s.parse().unwrap()
}

fn manifest(id: &str, activation: &str) -> String {
    format!(
        "id = \"{id}\"\nversion = \"1.2.3\"\nengine = \"*\"\ntier = \"core\"\nactivation = [{activation}]\n"
    )
}

impl Kernel {
    fn state(&self, id: &str) -> ModState {
        self.mods.status_of(&pid(id)).unwrap().state
    }

    fn error(&self, id: &str) -> String {
        self.mods
            .status_of(&pid(id))
            .unwrap()
            .error
            .unwrap_or_default()
    }

    fn command_ids(&self) -> Vec<String> {
        self.contributions
            .contributions_of::<points::Commands>()
            .unwrap()
            .iter()
            .map(|c| c.id.to_string())
            .collect()
    }

    fn status_text(&self, id: &str) -> Option<String> {
        self.contributions
            .get_of::<points::StatusItems>(&cid(id))
            .unwrap()
            .map(|c| c.value.text.clone())
    }

    /// Commands, handlers, status items and services left.
    fn registrations(&self) -> usize {
        let c = &self.contributions;
        c.contributions_of::<points::Commands>().unwrap().len()
            + c.contributions_of::<points::CommandHandlers>()
                .unwrap()
                .len()
            + c.contributions_of::<points::StatusItems>().unwrap().len()
            + self.services.services().len()
    }
}

#[tokio::test]
async fn hello_activates_and_its_contributions_are_typed_registry_entries() {
    let k = kernel();
    let errors = k
        .mods
        .load_builtins(BuiltinMods::new().with(coqo_mod_hello::MANIFEST, Hello::new()));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(k.state("coqo.hello"), ModState::Loaded);
    assert!(
        k.command_ids().is_empty(),
        "hello contributes nothing up front"
    );

    k.mods.start().await;
    let status = k.mods.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].id, pid("coqo.hello"));
    assert_eq!(status[0].name, "Hello");
    assert_eq!(status[0].version.to_string(), "0.1.0");
    assert_eq!(status[0].kind, ProviderKind::Builtin);
    assert_eq!(status[0].state, ModState::Active);
    assert_eq!(status[0].error, None);

    let greet = k
        .contributions
        .get_of::<points::Commands>(&cid(GREET))
        .unwrap()
        .unwrap();
    assert_eq!(greet.value.title, "Say hello");
    assert_eq!(greet.value.category.as_deref(), Some("Hello"));
    assert_eq!(greet.provider, pid("coqo.hello"));
    let item = k
        .contributions
        .get_of::<points::StatusItems>(&cid(STATUS))
        .unwrap()
        .unwrap();
    assert_eq!(item.value.text, "Hello");
    assert_eq!(item.value.command.as_ref().unwrap().as_str(), GREET);

    let reply = k
        .mods
        .run_command(&cid(GREET), json!({ "name": "Ada" }))
        .await
        .unwrap();
    assert_eq!(reply, "Hello, Ada!");
    // The handler updated its status item through the kept context.
    assert_eq!(k.status_text(STATUS).as_deref(), Some("Hello ×1"));

    // Starting again changes nothing.
    k.mods.start().await;
    assert_eq!(k.state("coqo.hello"), ModState::Active);
}

#[tokio::test]
async fn disabling_withdraws_everything_and_enabling_brings_it_back() {
    let k = kernel();
    k.mods
        .load_builtins(BuiltinMods::new().with(coqo_mod_hello::MANIFEST, Hello::new()));
    k.mods.start().await;
    assert_eq!(k.registrations(), 3, "command, handler and status item");

    k.mods.disable(&pid("coqo.hello")).await.unwrap();
    assert_eq!(k.state("coqo.hello"), ModState::Disabled);
    assert_eq!(k.registrations(), 0);
    assert!(!k.mods.host().is_admitted(&pid("coqo.hello")));
    assert!(matches!(
        k.mods.run_command(&cid(GREET), Value::Null).await,
        Err(ModError::UnknownCommand(_))
    ));
    // Disabling twice is fine.
    k.mods.disable(&pid("coqo.hello")).await.unwrap();

    k.mods.enable(&pid("coqo.hello")).await.unwrap();
    assert_eq!(k.state("coqo.hello"), ModState::Active);
    assert_eq!(k.registrations(), 3);
    assert_eq!(
        k.mods.run_command(&cid(GREET), Value::Null).await.unwrap(),
        "Hello, world!"
    );
}

/// `test.greeter@1`.
trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

impl Service for dyn Greeter {
    const ID: &'static str = "test.greeter";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
}

struct Fixed(&'static str);
impl Greeter for Fixed {
    fn greet(&self) -> String {
        self.0.to_owned()
    }
}

/// Provides `Greeter`, keeps its context, and counts deactivations.
#[derive(Default)]
struct Provider {
    text: &'static str,
    cx: Arc<Mutex<Option<ModContext>>>,
    deactivated: Arc<AtomicUsize>,
}

impl Mod for Provider {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.provide::<dyn Greeter>(Arc::new(Fixed(self.text)))?;
            *self.cx.lock().unwrap() = Some(cx);
            Ok(())
        })
    }

    fn deactivate(&self) -> BoxFuture<'_, Result<(), ModError>> {
        self.deactivated.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Consumes `Greeter` at activation and shows the greeting.
struct Consumer;

impl Mod for Consumer {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            let greeting = cx.consume::<dyn Greeter>()?.greet();
            cx.contribute::<points::StatusItems>(StatusItem::new(
                "consumer.status".parse()?,
                greeting,
            ))
        })
    }
}

#[tokio::test]
async fn services_resolve_through_the_kernel_registry_with_builtin_precedence() {
    let k = kernel();
    let provider = Provider {
        text: "builtin",
        ..Provider::default()
    };
    let kept_cx = Arc::clone(&provider.cx);
    let deactivated = Arc::clone(&provider.deactivated);
    let errors = k.mods.load_builtins(
        BuiltinMods::new()
            .with(&manifest("test.provider", "\"startup\""), provider)
            .with(&manifest("test.consumer", "\"startup\""), Consumer),
    );
    assert!(errors.is_empty(), "{errors:?}");
    k.mods.start().await;
    assert_eq!(k.state("test.consumer"), ModState::Active);
    assert_eq!(k.status_text("consumer.status").as_deref(), Some("builtin"));

    // Kernel code consumes the mod's service with its own type.
    let request = <dyn Greeter>::service_ref();
    assert_eq!(
        k.services.consume::<dyn Greeter>(&request).unwrap().greet(),
        "builtin"
    );
    let descriptor = k.services.resolve(&request).unwrap();
    assert_eq!(descriptor.provider, pid("test.provider"));
    assert_eq!(descriptor.kind, ProviderKind::Builtin);

    // An installed mod replaces the built-in provider.
    let replacement = Manifest::from_toml_str(&manifest("user.greeter", "\"startup\"")).unwrap();
    k.mods
        .load(
            replacement,
            Box::new(Provider {
                text: "mod",
                ..Provider::default()
            }),
            ProviderKind::Mod,
        )
        .unwrap();
    k.mods.start().await;
    assert_eq!(
        k.services.consume::<dyn Greeter>(&request).unwrap().greet(),
        "mod"
    );
    k.mods.disable(&pid("user.greeter")).await.unwrap();
    assert_eq!(
        k.services.consume::<dyn Greeter>(&request).unwrap().greet(),
        "builtin"
    );

    // Disabling the built-in calls deactivate and withdraws its service; the
    // context it kept no longer works.
    k.mods.disable(&pid("test.provider")).await.unwrap();
    assert_eq!(deactivated.load(Ordering::SeqCst), 1);
    assert!(k.services.consume::<dyn Greeter>(&request).is_err());
    let cx = kept_cx.lock().unwrap().clone().unwrap();
    assert!(matches!(
        cx.provide::<dyn Greeter>(Arc::new(Fixed("zombie"))),
        Err(ModError::PermissionDenied(_))
    ));
    assert!(k.services.services().is_empty());
}

/// Contributes a status item, then fails or panics.
enum Broken {
    Fails,
    PanicsInActivate,
    PanicsWhilePolled,
}

impl Mod for Broken {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        if let Broken::PanicsInActivate = self {
            panic!("broken before the future");
        }
        Box::pin(async move {
            let id = format!("{}.status", cx.mod_id());
            cx.contribute::<points::StatusItems>(StatusItem::new(id.parse()?, "half done"))?;
            match self {
                Broken::Fails => Err(ModError::failed("no luck")),
                _ => panic!("broken while polled"),
            }
        })
    }
}

/// Registers a command whose handler panics.
struct PanickyCommand;

impl Mod for PanickyCommand {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            cx.register_command(
                Command::new("panicky.run".parse()?, "Panic"),
                |_args: Value| async { panic!("handler blew up") },
            )
        })
    }
}

#[tokio::test]
async fn failing_and_panicking_mods_are_isolated() {
    let k = kernel();
    let errors = k.mods.load_builtins(
        BuiltinMods::new()
            .with(&manifest("broken.fails", "\"startup\""), Broken::Fails)
            .with(
                &manifest("broken.early", "\"startup\""),
                Broken::PanicsInActivate,
            )
            .with(
                &manifest("broken.late", "\"startup\""),
                Broken::PanicsWhilePolled,
            )
            .with(&manifest("panicky", "\"startup\""), PanickyCommand)
            .with(coqo_mod_hello::MANIFEST, Hello::new()),
    );
    assert!(errors.is_empty(), "{errors:?}");
    k.mods.start().await;

    assert_eq!(k.state("broken.fails"), ModState::Failed);
    assert_eq!(k.error("broken.fails"), "no luck");
    assert_eq!(k.state("broken.early"), ModState::Failed);
    assert!(
        k.error("broken.early")
            .contains("panicked: broken before the future")
    );
    assert_eq!(k.state("broken.late"), ModState::Failed);
    assert!(
        k.error("broken.late")
            .contains("panicked: broken while polled")
    );
    // What they registered before failing is gone.
    assert_eq!(k.status_text("broken.fails.status"), None);
    assert_eq!(k.status_text("broken.late.status"), None);

    // Everyone else is fine.
    assert_eq!(k.state("coqo.hello"), ModState::Active);
    assert_eq!(k.state("panicky"), ModState::Active);
    let err = k
        .mods
        .run_command(&cid("panicky.run"), Value::Null)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("handler blew up"), "{err}");
    assert_eq!(k.state("panicky"), ModState::Active);
    assert_eq!(
        k.mods.run_command(&cid(GREET), Value::Null).await.unwrap(),
        "Hello, world!"
    );

    // A failed mod can't be activated, but it can be disabled and retried.
    assert!(k.mods.activate(&pid("broken.fails")).await.is_err());
    k.mods.disable(&pid("broken.fails")).await.unwrap();
    assert_eq!(k.state("broken.fails"), ModState::Disabled);
    assert!(k.mods.enable(&pid("broken.fails")).await.is_err());
    assert_eq!(k.state("broken.fails"), ModState::Failed);
}

#[tokio::test]
async fn bad_manifests_are_reported_without_stopping_other_mods() {
    let k = kernel();
    let too_new =
        manifest("future.mod", "\"startup\"").replace("engine = \"*\"", "engine = \"^99\"");
    let errors = k.mods.load_builtins(
        BuiltinMods::new()
            .with("id = \"not valid", Hello::new())
            .with(&too_new, Hello::new())
            .with(coqo_mod_hello::MANIFEST, Hello::new())
            .with(coqo_mod_hello::MANIFEST, Hello::new()),
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors[0].to_string().contains("invalid manifest"));
    assert!(errors[1].to_string().contains("needs mod API `^99`"));
    assert!(errors[2].to_string().contains("already loaded"));

    // The unparseable one has no id to list; the others are listed.
    let ids: Vec<_> = k.mods.status().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, [pid("future.mod"), pid("coqo.hello")]);
    assert_eq!(k.state("future.mod"), ModState::Failed);

    k.mods.start().await;
    assert_eq!(k.state("future.mod"), ModState::Failed);
    assert_eq!(k.state("coqo.hello"), ModState::Active);
}

/// Registers the handler for its statically declared command.
#[derive(Default)]
struct Lazy {
    activations: Arc<AtomicUsize>,
}

impl Mod for Lazy {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        self.activations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            cx.register_command(
                Command::new("lazy.run".parse()?, "Run lazily"),
                |args: Value| async move { Ok(json!({ "echo": args })) },
            )
        })
    }
}

#[tokio::test]
async fn command_activation_event_activates_on_first_run() {
    let k = kernel();
    let lazy = Lazy::default();
    let activations = Arc::clone(&lazy.activations);
    let source = format!(
        "{}\n[[contributes.commands]]\nid = \"lazy.run\"\ntitle = \"Run lazily\"\n",
        manifest("test.lazy", "\"command:lazy.run\"")
    );
    let errors = k.mods.load_builtins(BuiltinMods::new().with(&source, lazy));
    assert!(errors.is_empty(), "{errors:?}");

    // The static command is listed before the mod runs, and startup leaves
    // it alone.
    k.mods.start().await;
    assert_eq!(k.command_ids(), ["lazy.run"]);
    assert_eq!(k.state("test.lazy"), ModState::Loaded);
    assert_eq!(activations.load(Ordering::SeqCst), 0);

    let reply = k
        .mods
        .run_command(&cid("lazy.run"), json!(1))
        .await
        .unwrap();
    assert_eq!(reply, json!({ "echo": 1 }));
    assert_eq!(k.state("test.lazy"), ModState::Active);
    k.mods
        .run_command(&cid("lazy.run"), json!(2))
        .await
        .unwrap();
    assert_eq!(activations.load(Ordering::SeqCst), 1);

    assert!(matches!(
        k.mods
            .run_command(&cid("nobody.has.this"), Value::Null)
            .await,
        Err(ModError::UnknownCommand(_))
    ));
}

/// A point owned by a crate rather than the mod API.
struct Decorations;

impl ContributionPoint for Decorations {
    const ID: &'static str = "test.decorations";
    type Value = Vec<u32>;
}

struct Decorator;

impl Mod for Decorator {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(
            async move { cx.contribute_as::<Decorations>("decorator.lines".parse()?, vec![1, 2]) },
        )
    }
}

#[tokio::test]
async fn crates_declare_their_own_points_and_read_typed_values() {
    let k = kernel();
    let errors = k.mods.load_builtins(
        BuiltinMods::new()
            .with_point::<Decorations>()
            .with(&manifest("test.decorator", "\"startup\""), Decorator),
    );
    assert!(errors.is_empty(), "{errors:?}");
    k.mods.start().await;
    let lines = k.contributions.contributions_of::<Decorations>().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(*lines[0].value, [1, 2]);

    // Without the declaration the mod would fail, not the daemon.
    let other = kernel();
    other.mods.load_builtins(
        BuiltinMods::new().with(&manifest("test.decorator", "\"startup\""), Decorator),
    );
    other.mods.start().await;
    assert_eq!(other.state("test.decorator"), ModState::Failed);
    assert!(
        other
            .error("test.decorator")
            .contains("unknown contribution point")
    );
}

#[tokio::test]
async fn shutdown_deactivates_every_mod() {
    let k = kernel();
    let provider = Provider {
        text: "x",
        ..Provider::default()
    };
    let deactivated = Arc::clone(&provider.deactivated);
    k.mods.load_builtins(
        BuiltinMods::new()
            .with(&manifest("test.provider", "\"startup\""), provider)
            .with(coqo_mod_hello::MANIFEST, Hello::new()),
    );
    k.mods.start().await;
    k.mods.shutdown().await;
    assert_eq!(deactivated.load(Ordering::SeqCst), 1);
    assert!(
        k.mods
            .status()
            .iter()
            .all(|s| s.state == ModState::Disabled)
    );
    assert_eq!(k.registrations(), 0);
}

/// Never finishes activating.
struct Stuck;

impl Mod for Stuck {
    fn activate(&self, _cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_stuck_mod_times_out_without_holding_up_the_others() {
    let services = Arc::new(ServiceRegistry::new());
    let contributions = Arc::new(ContributionRegistry::new());
    let mods = ModManager::with_registries(services, contributions)
        .with_lifecycle_timeout(std::time::Duration::from_millis(50));
    mods.load_builtins(
        BuiltinMods::new()
            .with(&manifest("test.stuck", "\"startup\""), Stuck)
            .with(coqo_mod_hello::MANIFEST, Hello::new()),
    );
    mods.start().await;
    let status = mods.status();
    assert_eq!(status[0].state, ModState::Failed);
    assert!(status[0].error.as_deref().unwrap().contains("timed out"));
    assert_eq!(status[1].state, ModState::Active);
}
