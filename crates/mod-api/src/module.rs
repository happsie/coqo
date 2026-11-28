// SPDX-License-Identifier: Apache-2.0

//! The [`Mod`] trait, the [`ModContext`] a mod talks to, and the
//! [`ModHost`] behind it.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::contrib::{Command, Keyed, McpTool, Setting, SettingsSection};
use crate::erased::{AnyArc, BoxFuture};
use crate::error::ModError;
use crate::id::{ContributionId, ContributionPointId, ProviderId, ServiceId, ServiceRef};
use crate::points::{
    CommandHandlers, Commands, ContributionPoint, McpToolHandlers, McpTools, Settings,
    SettingsSections,
};
use crate::service::Service;

/// A mod's code.
///
/// Core mods implement it directly; the WASM and process hosts implement it
/// for the guest they load, so the kernel drives every tier the same way.
///
/// # Lifecycle
///
/// 1. The host parses and validates the mod's
///    [`Manifest`](crate::manifest::Manifest), checks its `engine` range, and
///    registers its static `[contributes]` (see
///    [`Contributes::register`](crate::manifest::Contributes::register)), so
///    commands and views show up before the mod runs.
/// 2. When one of the manifest's activation events fires, the host calls
///    [`activate`](Self::activate) once. The mod provides its services and
///    registers handlers and runtime contributions through the
///    [`ModContext`], which it may keep for later.
/// 3. When the mod is disabled, unloaded or reloaded, the host calls
///    [`deactivate`](Self::deactivate) and then withdraws every service and
///    contribution the mod registered. Mods don't need to clean those up.
pub trait Mod: Send + Sync + 'static {
    /// Starts the mod.
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>>;

    /// Stops the mod: cancel background work, flush state. The default does
    /// nothing.
    fn deactivate(&self) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Runs a command (point `command_handlers`).
///
/// Any `Fn(Value) -> impl Future<Output = Result<Value, ModError>>` is a
/// handler, so a closure with an `async move` block works.
pub trait CommandHandler: Send + Sync + 'static {
    /// Runs the command with `args` (`null` when there are none) and returns
    /// its result (`null` when there is none).
    fn run(&self, args: Value) -> BoxFuture<'static, Result<Value, ModError>>;
}

impl<F, Fut> CommandHandler for F
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ModError>> + Send + 'static,
{
    fn run(&self, args: Value) -> BoxFuture<'static, Result<Value, ModError>> {
        Box::pin(self(args))
    }
}

/// One call of an MCP tool, as an agent made it (point `mcp_tool_handlers`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolCall {
    /// The Coqo workspace of the calling agent session. A tool acts on this
    /// workspace only.
    pub workspace: String,
    /// The calling agent session (Coqo's id).
    pub session: String,
    /// The arguments, already checked against the tool's `input_schema`.
    pub arguments: Map<String, Value>,
}

/// What an MCP tool returns to the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolOutput {
    /// Text for the model.
    pub text: String,
    /// The same result as JSON (MCP `structuredContent`), if it has one.
    pub structured: Option<Value>,
}

impl McpToolOutput {
    /// A plain-text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            structured: None,
        }
    }

    /// A JSON result, also sent to the model as pretty-printed text.
    pub fn json(value: Value) -> Self {
        Self {
            text: serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            structured: Some(value),
        }
    }
}

/// Runs an MCP tool (point `mcp_tool_handlers`). An error is shown to the
/// agent as a failed tool result, so it can correct itself.
///
/// Any `Fn(McpToolCall) -> impl Future<Output = Result<McpToolOutput,
/// ModError>>` is a handler.
pub trait McpToolHandler: Send + Sync + 'static {
    /// Runs the tool.
    fn call(&self, call: McpToolCall) -> BoxFuture<'static, Result<McpToolOutput, ModError>>;
}

impl<F, Fut> McpToolHandler for F
where
    F: Fn(McpToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<McpToolOutput, ModError>> + Send + 'static,
{
    fn call(&self, call: McpToolCall) -> BoxFuture<'static, Result<McpToolOutput, ModError>> {
        Box::pin(self(call))
    }
}

/// A resolved service, as a [`ModHost`] returns it.
#[derive(Debug, Clone)]
pub struct ResolvedService {
    /// The service at the provider's exact version.
    pub service: ServiceRef,
    /// Who provides it.
    pub provider: ProviderId,
    /// The provider's value, an erased `Arc<S>`.
    pub value: AnyArc,
}

/// What a host implements so mods can register with it.
///
/// It is object safe and type erased; mods use the typed [`ModContext`]
/// wrapper instead. The kernel implements it over its service and
/// contribution registries; [`testing::TestHost`](crate::testing::TestHost)
/// is an in-memory one for tests.
///
/// Every call names the calling mod, so a host can check the call against the
/// mod's manifest (declared `provides` / `consumes`, permissions) and
/// withdraw everything the mod registered when it is deactivated.
pub trait ModHost: Send + Sync {
    /// Registers `provider`'s implementation of `service` (at exactly
    /// `service.version`). Providing the same id and major version again
    /// replaces the earlier value.
    fn provide_service(
        &self,
        provider: &ProviderId,
        service: ServiceRef,
        value: AnyArc,
    ) -> Result<(), ModError>;

    /// Removes `provider`'s registrations of `service`.
    fn withdraw_service(&self, provider: &ProviderId, service: &ServiceId) -> Result<(), ModError>;

    /// Resolves `request` (same major, at least that minor) for `consumer`.
    fn consume_service(
        &self,
        consumer: &ProviderId,
        request: &ServiceRef,
    ) -> Result<ResolvedService, ModError>;

    /// Adds or replaces `provider`'s contribution `id` to `point`.
    fn contribute(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: ContributionId,
        value: AnyArc,
    ) -> Result<(), ModError>;

    /// Removes `provider`'s contribution `id` from `point`. Returns whether
    /// there was one.
    fn withdraw_contribution(
        &self,
        provider: &ProviderId,
        point: &ContributionPointId,
        id: &ContributionId,
    ) -> Result<bool, ModError>;
}

/// A mod's handle to its host: provide and consume services, contribute to
/// contribution points. Cheap to clone; keep it to make changes after
/// activation (update a status item, say).
#[derive(Clone)]
pub struct ModContext {
    mod_id: ProviderId,
    host: Arc<dyn ModHost>,
}

impl fmt::Debug for ModContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModContext")
            .field("mod_id", &self.mod_id)
            .finish_non_exhaustive()
    }
}

impl ModContext {
    /// A context for mod `mod_id` on `host`. Hosts create these; mods receive
    /// them in [`Mod::activate`].
    pub fn new(mod_id: ProviderId, host: Arc<dyn ModHost>) -> Self {
        Self { mod_id, host }
    }

    /// The mod's id, from its manifest.
    pub fn mod_id(&self) -> &ProviderId {
        &self.mod_id
    }

    /// Provides `value` as service `S` at [`S::VERSION`](Service::VERSION).
    pub fn provide<S>(&self, value: Arc<S>) -> Result<(), ModError>
    where
        S: Service + ?Sized,
    {
        self.host
            .provide_service(&self.mod_id, S::service_ref(), AnyArc::new(value))
    }

    /// Stops providing service `S` (every major version).
    pub fn withdraw_service<S>(&self) -> Result<(), ModError>
    where
        S: Service + ?Sized,
    {
        self.host
            .withdraw_service(&self.mod_id, &S::service_ref().id)
    }

    /// The active provider of service `S`, at [`S::VERSION`](Service::VERSION)
    /// or a later minor version.
    ///
    /// Resolve services when you need them rather than once at activation, so
    /// a replaced provider takes effect.
    pub fn consume<S>(&self) -> Result<Arc<S>, ModError>
    where
        S: Service + ?Sized,
    {
        let resolved = self.host.consume_service(&self.mod_id, &S::service_ref())?;
        resolved
            .value
            .downcast::<S>()
            .ok_or_else(|| ModError::ServiceTypeMismatch {
                service: resolved.service.clone(),
                provider: resolved.provider.clone(),
                expected: std::any::type_name::<S>(),
                actual: resolved.value.type_name(),
            })
    }

    /// Contributes `value` to point `P` under its own id. Contributing the
    /// same id again replaces the value, which is how status items and other
    /// live contributions update.
    pub fn contribute<P>(&self, value: P::Value) -> Result<(), ModError>
    where
        P: ContributionPoint,
        P::Value: Keyed,
    {
        let id = value.key().clone();
        self.contribute_as::<P>(id, value)
    }

    /// Contributes `value` to point `P` under `id`, for values without an id
    /// of their own (keybindings, menu items, handlers).
    pub fn contribute_as<P>(&self, id: ContributionId, value: P::Value) -> Result<(), ModError>
    where
        P: ContributionPoint,
    {
        self.host
            .contribute(&self.mod_id, &P::id(), id, AnyArc::from_value(value))
    }

    /// Removes this mod's contribution `id` from point `P`. Returns whether
    /// there was one.
    pub fn withdraw<P>(&self, id: &ContributionId) -> Result<bool, ModError>
    where
        P: ContributionPoint,
    {
        self.host.withdraw_contribution(&self.mod_id, &P::id(), id)
    }

    /// Registers MCP tool `tool` together with the `handler` that runs it.
    /// Agents get it through `coqod mcp` (PLAN §7.4).
    pub fn register_mcp_tool(
        &self,
        tool: McpTool,
        handler: impl McpToolHandler,
    ) -> Result<(), ModError> {
        let id = tool.name.clone();
        let handler: Arc<dyn McpToolHandler> = Arc::new(handler);
        self.contribute_as::<McpToolHandlers>(id.clone(), handler)?;
        if let Err(e) = self.contribute::<McpTools>(tool) {
            // Best effort: don't leave a handler without its tool.
            let _ = self.withdraw::<McpToolHandlers>(&id);
            return Err(e);
        }
        Ok(())
    }

    /// Registers a settings page `section` with its `fields` (points
    /// `settings_sections` and `settings`). Each field's key must start with
    /// the section's id and a dot (`files.auto_reveal` in `files`), and its
    /// default must be valid for its type. The settings service validates
    /// the user's `settings.toml` against them and the settings page shows
    /// them, a switch, a choice or a text field by type.
    ///
    /// Registering the section again replaces it; registering a field
    /// again replaces that field.
    pub fn register_settings(
        &self,
        section: SettingsSection,
        fields: impl IntoIterator<Item = Setting>,
    ) -> Result<(), ModError> {
        let fields: Vec<Setting> = fields.into_iter().collect();
        for field in &fields {
            if !section.holds(field.key.as_str()) {
                return Err(ModError::Failed(format!(
                    "setting `{}` is not in section `{}`: its key must start with `{}.`",
                    field.key, section.id, section.id
                )));
            }
            if let Some(default) = &field.default
                && let Err(e) = field.check_value(default)
            {
                return Err(ModError::Failed(format!(
                    "setting `{}`: default `{default}`: {e}",
                    field.key
                )));
            }
        }
        self.contribute::<SettingsSections>(section)?;
        fields
            .into_iter()
            .try_for_each(|field| self.contribute::<Settings>(field))
    }

    /// Registers `command` together with the `handler` that runs it.
    pub fn register_command(
        &self,
        command: Command,
        handler: impl CommandHandler,
    ) -> Result<(), ModError> {
        let id = command.id.clone();
        let handler: Arc<dyn CommandHandler> = Arc::new(handler);
        self.contribute_as::<CommandHandlers>(id.clone(), handler)?;
        if let Err(e) = self.contribute::<Commands>(command) {
            // Best effort: don't leave a handler without its command.
            let _ = self.withdraw::<CommandHandlers>(&id);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::contrib::{Setting, SettingScope, SettingType, SettingsSection};
    use crate::points;
    use crate::testing::TestHost;

    fn id(s: &str) -> crate::ContributionId {
        s.parse().unwrap()
    }

    #[test]
    fn register_settings_adds_the_section_and_its_fields() {
        let host = TestHost::new();
        let cx = host.context("coqo.files".parse().unwrap());
        cx.register_settings(
            SettingsSection::new(id("files"), "Files").with_order(20),
            [
                Setting::boolean(id("files.auto_reveal"), "Reveal", false),
                Setting::enumeration(id("files.sort"), "Sort", ["name", "kind"], "name"),
            ],
        )
        .unwrap();
        assert_eq!(host.contributions::<points::SettingsSections>().len(), 1);
        assert_eq!(host.contributions::<points::Settings>().len(), 2);

        // A field outside the section, or a bad default, is refused.
        let outside = cx.register_settings(
            SettingsSection::new(id("files"), "Files"),
            [Setting::boolean(id("filesystem.x"), "X", false)],
        );
        assert!(outside.unwrap_err().to_string().contains("`files.`"));
        let bad = cx.register_settings(
            SettingsSection::new(id("files"), "Files"),
            [Setting::enumeration(
                id("files.sort"),
                "Sort",
                ["name"],
                "size",
            )],
        );
        assert!(bad.unwrap_err().to_string().contains("not one of"));
    }

    #[test]
    fn values_are_checked_against_the_type() {
        let theme = Setting::enumeration(
            id("appearance.theme"),
            "Theme",
            ["system", "dark", "light"],
            "system",
        )
        .user_only();
        assert_eq!(theme.scope, SettingScope::User);
        assert_eq!(theme.check_value(&json!("dark")), Ok(()));
        assert_eq!(
            theme.check_value(&json!("blue")).unwrap_err(),
            "`blue` is not one of system, dark, light"
        );
        assert_eq!(
            theme.check_value(&json!(1)).unwrap_err(),
            "expected one of system, dark, light"
        );
        let n = Setting::integer(id("a.n"), "N", 1).with_bounds(Some(1.0), Some(8.0));
        assert_eq!(n.check_value(&json!(4)), Ok(()));
        assert!(n.check_value(&json!(4.5)).is_err());
        assert!(n.check_value(&json!(9)).unwrap_err().contains("maximum"));
        let b = Setting::boolean(id("a.b"), "B", true);
        assert_eq!(
            b.check_value(&json!("yes")).unwrap_err(),
            "expected true or false"
        );
        assert_eq!(b.default_value(), json!(true));
        let mut list = Setting::string(id("a.s"), "S", "");
        list.ty = SettingType::StringList;
        list.default = None;
        assert_eq!(list.default_value(), json!([]));
        assert!(list.check_value(&json!(["a", 1])).is_err());
    }
}
