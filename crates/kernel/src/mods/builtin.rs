// SPDX-License-Identifier: Apache-2.0

//! [`BuiltinMods`]: the table of core mods linked into the daemon.

use std::fmt;

use coqo_mod_api::manifest::ManifestError;
use coqo_mod_api::{ContributionPoint, Manifest, Mod};

use crate::contribution::{ContributionError, ContributionRegistry};

/// Declares one contribution point on a registry.
pub(super) type DeclarePoint = fn(&ContributionRegistry) -> Result<(), ContributionError>;

fn declare<P: ContributionPoint>(registry: &ContributionRegistry) -> Result<(), ContributionError> {
    registry.declare::<P>()
}

/// One entry of [`BuiltinMods`].
pub(super) struct BuiltinMod {
    pub(super) manifest: Result<Manifest, ManifestError>,
    pub(super) module: Box<dyn Mod>,
}

/// The core mods linked into the daemon, with the contribution points their
/// crates own. Build it explicitly, one line per mod, and hand it to
/// [`ModManager::load_builtins`](super::ModManager::load_builtins):
///
/// ```
/// use coqo_kernel::mods::BuiltinMods;
/// # use coqo_mod_api::{BoxFuture, Mod, ModContext, ModError};
/// # struct Hello;
/// # impl Mod for Hello {
/// #     fn activate(&self, _: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
/// #         Box::pin(async { Ok(()) })
/// #     }
/// # }
/// # const MANIFEST: &str = "id = \"coqo.hello\"\nversion = \"0.1.0\"\nengine = \"*\"\ntier = \"core\"\nactivation = [\"startup\"]\n";
///
/// let builtins = BuiltinMods::new().with(MANIFEST, Hello);
/// assert_eq!(builtins.len(), 1);
/// ```
///
/// A crate that owns a contribution point (agent providers, editor
/// decorations) declares it here too, with
/// [`with_point`](Self::with_point), so it exists before any mod
/// contributes to it.
#[derive(Default)]
pub struct BuiltinMods {
    pub(super) points: Vec<DeclarePoint>,
    pub(super) mods: Vec<BuiltinMod>,
}

impl fmt::Debug for BuiltinMods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ids: Vec<_> = self
            .mods
            .iter()
            .map(|m| match &m.manifest {
                Ok(manifest) => manifest.id.to_string(),
                Err(_) => "<invalid manifest>".to_owned(),
            })
            .collect();
        f.debug_struct("BuiltinMods")
            .field("points", &self.points.len())
            .field("mods", &ids)
            .finish()
    }
}

impl BuiltinMods {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `module` with its `coqo.mod.toml` source (usually
    /// `include_str!("../coqo.mod.toml")`). A manifest that doesn't parse
    /// doesn't panic: loading reports it and skips the mod.
    #[must_use]
    pub fn with(mut self, manifest: &str, module: impl Mod) -> Self {
        self.push(manifest, module);
        self
    }

    /// [`with`](Self::with) for a manifest that is already parsed.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Manifest, module: impl Mod) -> Self {
        self.mods.push(BuiltinMod {
            manifest: Ok(manifest),
            module: Box::new(module),
        });
        self
    }

    /// Adds `module` in place; see [`with`](Self::with).
    pub fn push(&mut self, manifest: &str, module: impl Mod) {
        self.mods.push(BuiltinMod {
            manifest: Manifest::from_toml_str(manifest),
            module: Box::new(module),
        });
    }

    /// Declares contribution point `P` (owned by the crate adding it) before
    /// any mod loads.
    #[must_use]
    pub fn with_point<P: ContributionPoint>(mut self) -> Self {
        self.points.push(declare::<P>);
        self
    }

    /// Number of mods.
    pub fn len(&self) -> usize {
        self.mods.len()
    }

    /// Whether there are no mods.
    pub fn is_empty(&self) -> bool {
        self.mods.is_empty()
    }
}
