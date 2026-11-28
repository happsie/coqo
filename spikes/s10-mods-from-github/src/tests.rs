//! Offline tests on `fixtures/`, through the same planner as the GitHub path.

use std::path::Path;

use crate::plan::{self, Trust};
use crate::repo::Local;

fn fixtures() -> Local {
    Local::open(&Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")).unwrap()
}

fn plan(dir: &str, pick: Option<&str>) -> plan::Plan {
    plan::plan(&fixtures(), dir, pick, "fixtures")
}

#[test]
fn a_content_mod_runs_nothing_but_its_prompts() {
    let p = plan("content-mod", None);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.id.as_deref(), Some("acme.team-kit"));
    assert_eq!(p.trust, Trust::Prompts);
    assert!(p.runs.is_empty(), "{:?}", p.runs);
    let c = p.contributes.join("\n");
    assert!(
        c.contains("1 themes") && c.contains("1 settings") && c.contains("1 sidebar views"),
        "{c}"
    );
    assert!(
        c.contains("agent plugin agent: 1 skills, 1 commands"),
        "{c}"
    );
}

#[test]
fn content_mods_may_not_carry_code_or_permissions() {
    let p = plan("bad-content", None);
    let e = p.errors.join("\n");
    assert!(e.contains("activation"), "{e}");
    assert!(e.contains("permissions"), "{e}");
    assert!(e.contains("`commands` needs code"), "{e}");
}

#[test]
fn core_mods_and_newer_engines_are_refused() {
    assert!(plan("core-mod", None).errors[0].contains("compiled into Coqo"));
    let e = plan("future-mod", None).errors.join("\n");
    assert!(e.contains("needs Coqo mod API ^9"), "{e}");
}

#[test]
fn a_wasm_mod_is_sandboxed_and_says_where_its_component_comes_from() {
    let p = plan("wasm-mod", None);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.trust, Trust::Sandboxed);
    assert!(
        p.permissions
            .iter()
            .any(|x| x == "network: registry.npmjs.org")
    );
    assert!(p.notes[0].contains("release asset"), "{:?}", p.notes);
    assert!(p.runs[0].contains("WASM host"));
}

#[test]
fn a_claude_plugin_with_hooks_and_mcp_is_flagged_as_running_code() {
    let p = plan("claude-plugin", None);
    assert_eq!(p.id.as_deref(), Some("claude.fixtures.deployer"));
    assert_eq!(p.trust, Trust::Unsandboxed);
    let runs = p.runs.join("\n");
    assert!(
        runs.contains("PostToolUse: runs `${CLAUDE_PLUGIN_ROOT}/bin/fmt.sh`"),
        "{runs}"
    );
    assert!(
        runs.contains("deploy-db: runs `npx -y deploy-db-mcp`"),
        "{runs}"
    );
    assert!(
        runs.contains("docs: connects to https://docs.example.com/mcp"),
        "{runs}"
    );
    assert!(runs.contains("pre-approves tools"), "{runs}");
    assert!(
        p.notes[0].contains("ships 1 executable scripts"),
        "{:?}",
        p.notes
    );
}

#[test]
fn a_coqo_marketplace_lists_and_picks() {
    let p = plan("marketplace", None);
    assert_eq!(p.listing.len(), 3);
    assert!(p.id.is_none());

    let p = plan("marketplace", Some("acme.night"));
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    assert_eq!(p.id.as_deref(), Some("acme.night"));
    assert_eq!(p.trust, Trust::Content);

    let p = plan("marketplace", Some("acme.mismatch"));
    assert!(
        p.errors[0].contains("manifest says `acme.other`"),
        "{:?}",
        p.errors
    );

    let p = plan("marketplace", Some("acme.lint"));
    assert!(p.notes[0].contains("acme/lint-mod@v1.0.0"), "{:?}", p.notes);
}

#[test]
fn an_empty_folder_hints_at_ref_syntax() {
    let p = plan("themes-nowhere", None);
    assert_eq!(p.found, "nothing installable");
    assert!(
        p.notes[0].contains("write `@themes-nowhere`"),
        "{:?}",
        p.notes
    );
}

#[test]
fn the_plan_renders_without_network() {
    let repo = fixtures();
    let p = plan::plan(&repo, "content-mod", None, "fixtures");
    let text = plan::render(&repo, "content-mod", &p, &[]);
    assert!(
        text.contains("install to  <data>/mods/acme.team-kit/1.2.0-local/"),
        "{text}"
    );
    assert!(
        text.contains("network     0 api.github.com calls, 0 raw file fetches"),
        "{text}"
    );
    assert!(
        text.contains("verdict     installable after the user approves"),
        "{text}"
    );
}
