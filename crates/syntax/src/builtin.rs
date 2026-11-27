// SPDX-License-Identifier: Apache-2.0

//! `coqo.syntax`: the built-in mod that makes the bundled languages known.
//!
//! It is always on (activated at startup) and contributes one
//! [`contrib::Language`] per bundled language to the `languages` point, so
//! language mods (LSP servers, debuggers) and the UI find them by id. The
//! grammars are linked into Coqo, so the contributions name no grammar file.

use coqo_mod_api::contrib;
use coqo_mod_api::{BoxFuture, Manifest, Mod, ModContext, ModError, points};

use crate::language::{Language, languages};

/// The mod's `coqo.mod.toml`.
pub const MANIFEST: &str = include_str!("../coqo.mod.toml");

/// The parsed, validated [`MANIFEST`].
pub fn manifest() -> Manifest {
    Manifest::from_toml_str(MANIFEST).expect("coqo.syntax's manifest is valid")
}

/// The mod.
#[derive(Debug, Default)]
pub struct SyntaxMod;

impl SyntaxMod {
    /// The mod.
    pub fn new() -> Self {
        Self
    }
}

/// The `languages` contribution for `language`.
pub fn contribution(language: &Language) -> Result<contrib::Language, ModError> {
    Ok(contrib::Language {
        id: language.id().parse()?,
        name: language.name().to_owned(),
        extensions: language
            .extensions()
            .iter()
            .map(|e| format!(".{e}"))
            .collect(),
        filenames: language.filenames().iter().map(|&f| f.to_owned()).collect(),
        grammar: None,
        line_comment: language.line_comment().map(str::to_owned),
    })
}

impl Mod for SyntaxMod {
    fn activate(&self, cx: ModContext) -> BoxFuture<'_, Result<(), ModError>> {
        Box::pin(async move {
            for language in languages() {
                cx.contribute::<points::Languages>(contribution(language)?)?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coqo_mod_api::manifest::{ActivationEvent, Tier};
    use coqo_mod_api::testing::{TestHost, block_on};

    #[test]
    fn manifest_is_a_core_mod_activated_at_startup() {
        let m = manifest();
        assert_eq!(m.id.as_str(), "coqo.syntax");
        assert_eq!(m.tier, Tier::Core);
        assert!(m.activates_on(&ActivationEvent::Startup));
        assert!(m.supports_api(&coqo_mod_api::api_version()));
    }

    #[test]
    fn contributes_every_bundled_language() {
        let m = manifest();
        let host = TestHost::new();
        block_on(SyntaxMod::new().activate(host.context(m.id.clone()))).unwrap();

        let contributed = host.contributions::<points::Languages>();
        assert_eq!(contributed.len(), languages().len());
        let rust = host.contribution::<points::Languages>("rust").unwrap();
        assert_eq!(rust.name, "Rust");
        assert_eq!(rust.extensions, [".rs"]);
        assert_eq!(rust.line_comment.as_deref(), Some("//"));
        let toml = host.contribution::<points::Languages>("toml").unwrap();
        assert!(toml.filenames.iter().any(|f| f == "Cargo.lock"));

        block_on(SyntaxMod::new().deactivate()).unwrap();
        assert_eq!(host.withdraw_mod(&m.id), languages().len());
    }
}
