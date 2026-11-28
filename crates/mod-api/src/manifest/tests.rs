// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::contrib::{AgentProviderKind, Menu, SettingType, StatusAlignment, ThemeAppearance};
use crate::testing::TestHost;

const FULL: &str = include_str!("../../examples/coqo.mod.toml");

const MINIMAL: &str = r#"
id = "coqo.hello"
version = "0.1.0"
engine = "^0.0"
tier = "core"
"#;

fn problems(source: &str) -> Vec<ManifestProblem> {
    Manifest::from_toml_str(source)
        .expect_err("manifest should be rejected")
        .problems()
        .to_vec()
}

/// The 1-based line of the first occurrence of `needle` in `source`.
fn line_of(source: &str, needle: &str) -> usize {
    let offset = source.find(needle).expect("needle in source");
    source[..offset].matches('\n').count() + 1
}

#[test]
fn parses_the_full_example() {
    let m = Manifest::from_toml_str(FULL).unwrap();
    assert_eq!(m.id.as_str(), "acme.maven-tools");
    assert_eq!(m.display_name(), "Maven tools");
    assert_eq!(m.version, semver::Version::new(1, 4, 0));
    assert_eq!(m.tier, Tier::Process);
    assert_eq!(m.entry.as_deref(), Some("bin/maven-tools"));
    assert_eq!(m.args, ["--stdio"]);
    assert!(m.activates_on(&"command:maven.run-goal".parse().unwrap()));
    assert!(!m.activates_on(&ActivationEvent::Startup));
    assert_eq!(m.provides[0].to_string(), "acme.maven@1.2");
    assert_eq!(m.consumes.len(), 2);
    assert!(!m.consumes[0].optional);
    assert!(m.consumes[1].optional);
    assert_eq!(m.permissions.fs.write, ["$workspace/target", "$cache"]);
    assert_eq!(m.permissions.secrets, ["acme.repo-token"]);

    let c = &m.contributes;
    assert_eq!(c.commands.len(), 2);
    assert_eq!(c.keybindings[0].mac.as_deref(), Some("cmd+alt+m"));
    assert_eq!(
        c.keybindings[0].args,
        Some(serde_json::json!({ "goal": "verify" }))
    );
    assert_eq!(c.menus[0].menu, Menu::ExplorerContext);
    let types: Vec<_> = c.settings.iter().map(|s| s.ty).collect();
    assert_eq!(
        types,
        [
            SettingType::String,
            SettingType::Boolean,
            SettingType::Integer,
            SettingType::Enum,
            SettingType::StringList
        ]
    );
    assert_eq!(c.settings_sections[0].title, "Maven");
    assert!(c.settings_sections[0].holds("maven.threads"));
    assert_eq!(c.views.sidebar[0].id.as_str(), "maven.modules");
    assert_eq!(c.views.status[0].alignment, StatusAlignment::Right);
    assert_eq!(c.views.status[0].priority, 10);
    assert_eq!(c.languages[0].filenames, ["pom.xml"]);
    assert_eq!(c.lsp_servers[0].languages[0].as_str(), "maven-pom");
    assert!(c.debug_adapters[0].command.is_none());
    assert_eq!(
        c.mcp_tools[0].input_schema["required"],
        serde_json::json!(["module"])
    );
    assert_eq!(c.agent_providers[0].kind, AgentProviderKind::Native);
    assert_eq!(c.slash_commands[0].name.as_str(), "maven-test");
    assert_eq!(c.context_providers.len(), 1);
    assert_eq!(c.alert_sources.len(), 1);
    assert_eq!(c.themes[0].appearance, ThemeAppearance::Dark);
}

#[test]
fn round_trips_through_toml() {
    for source in [FULL, MINIMAL] {
        let m = Manifest::from_toml_str(source).unwrap();
        let written = m.to_toml_string().unwrap();
        let again = Manifest::from_toml_str(&written)
            .unwrap_or_else(|e| panic!("re-parse failed:\n{e}\n---\n{written}"));
        assert_eq!(again, m, "round trip changed the manifest:\n{written}");
    }
}

#[test]
fn minimal_manifest_has_empty_defaults() {
    let m: Manifest = MINIMAL.parse().unwrap();
    assert_eq!(m.display_name(), "coqo.hello");
    assert!(m.activation.is_empty());
    assert!(m.permissions.is_empty());
    assert!(m.contributes.is_empty());
    assert!(m.supports_api(&semver::Version::new(0, 0, 7)));
    assert!(!m.supports_api(&semver::Version::new(0, 1, 0)));
    // Optional sections are left out when written back.
    let written = m.to_toml_string().unwrap();
    assert!(!written.contains("permissions") && !written.contains("contributes"));
}

#[test]
fn syntax_errors_have_positions() {
    let source = "id = \"coqo.hello\"\nversion = 0.1.0\n";
    let p = problems(source);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].position.unwrap().line, 2);
    assert!(p[0].path.is_none());
}

#[test]
fn unknown_fields_name_the_field_and_line() {
    let source = format!("{MINIMAL}\n[[contributes.commands]]\nid = \"a\"\ntitel = \"A\"\n");
    let p = problems(&source);
    assert_eq!(p.len(), 1);
    assert!(
        p[0].message.contains("unknown field `titel`"),
        "{}",
        p[0].message
    );
    assert!(p[0].message.contains("`title`"), "{}", p[0].message);
    assert_eq!(p[0].position.unwrap().line, line_of(&source, "titel"));
}

#[test]
fn bad_ids_and_versions_point_at_the_value() {
    let cases = [
        ("id = \"Coqo Hello\"", "invalid provider id `Coqo Hello`"),
        ("version = \"1\"", "version"),
        ("engine = \"~>1\"", ""),
        ("tier = \"native\"", "unknown variant `native`"),
    ];
    for (line, expected) in cases {
        let key = line.split(' ').next().unwrap();
        let source: String = MINIMAL
            .lines()
            .map(|l| {
                if l.starts_with(&format!("{key} ")) {
                    line
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let p = problems(&source);
        assert_eq!(p.len(), 1, "{line}");
        assert!(p[0].message.contains(expected), "{line}: {}", p[0].message);
        assert_eq!(
            p[0].position.unwrap().line,
            line_of(&source, line),
            "{line}"
        );
    }
}

#[test]
fn bad_activation_events_and_service_refs_are_explained() {
    let source = format!("{MINIMAL}activation = [\"onStartup\"]\n");
    let p = problems(&source);
    assert!(
        p[0].message
            .contains("unknown activation event `onStartup`"),
        "{}",
        p[0].message
    );
    assert_eq!(p[0].position.unwrap().line, line_of(&source, "onStartup"));

    let source = format!("{MINIMAL}consumes = [\"coqo.git\"]\n");
    let p = problems(&source);
    assert!(p[0].message.contains("has no version"), "{}", p[0].message);

    let source = format!("{MINIMAL}consumes = [{{ service = \"coqo.git@1\", optonal = true }}]\n");
    let p = problems(&source);
    assert!(
        p[0].message.contains("unknown field `optonal`"),
        "{}",
        p[0].message
    );
}

#[test]
fn reports_every_semantic_problem_with_path_and_line() {
    let source = r#"
id = "coqo.bad"
version = "0.1.0"
engine = "^0.0"
tier = "core"
entry = "lib.so"
activation = ["startup", "startup"]
provides = ["coqo.x@1", "coqo.x@1.1"]

[permissions]
fs.read = ["$workpsace"]
network = ["https://api.github.com"]

[[contributes.commands]]
id = "bad.run"
title = ""

[[contributes.commands]]
id = "bad.run"
title = "Run again"

[[contributes.keybindings]]
command = "bad.run"
key = "ctl+r"

[[contributes.settings]]
key = "bad.count"
description = "A count."
type = "integer"
default = "three"

[[contributes.views.sidebar]]
id = "bad.view"
title = "View"

[[contributes.views.status]]
id = "bad.view"
text = "Status"

[[contributes.mcp_tools]]
name = "bad_tool"
description = "Does things."
input_schema = { type = "string" }
"#;
    let p = problems(source);
    let expect = [
        ("entry", "take no `entry`", "\"lib.so\""),
        (
            "activation[1]",
            "duplicate activation event `startup`",
            "\"startup\"]",
        ),
        (
            "provides[1]",
            "already provided at provides[0]",
            "\"coqo.x@1.1\"",
        ),
        (
            "permissions.fs.read[0]",
            "unknown location variable `$workpsace`",
            "\"$workpsace",
        ),
        ("permissions.network[0]", "is a URL", "\"https://"),
        (
            "contributes.commands[0].title",
            "must not be empty",
            "\"\"\n",
        ),
        (
            "contributes.commands[1].id",
            "duplicate command `bad.run` (also at contributes.commands[0])",
            "\"bad.run\"\ntitle = \"Run again\"",
        ),
        (
            "contributes.keybindings[0].key",
            "unknown modifier `ctl`",
            "\"ctl+r\"",
        ),
        (
            "contributes.settings[0].default",
            "doesn't match the setting's type; expected a whole number",
            "\"three\"",
        ),
        (
            "contributes.views.status[0].id",
            "duplicate view `bad.view`",
            "\"bad.view\"\ntext",
        ),
        (
            "contributes.mcp_tools[0].input_schema",
            "`type = \"object\"`",
            "{ type = \"string\" }",
        ),
    ];
    let paths: Vec<_> = p.iter().map(|p| p.path.clone().unwrap()).collect();
    assert_eq!(
        paths,
        expect.iter().map(|e| e.0).collect::<Vec<_>>(),
        "problems:\n{}",
        ManifestError::new(p.clone())
    );
    for (problem, (path, message, at)) in p.iter().zip(expect) {
        assert!(
            problem.message.contains(message),
            "{path}: {}",
            problem.message
        );
        assert_eq!(
            problem.position.unwrap().line,
            line_of(source, at),
            "{path}"
        );
        let span = problem.span.clone().unwrap();
        assert!(
            at.starts_with(&source[span.clone()][..1]),
            "{path}: span `{}`",
            &source[span]
        );
    }
}

#[test]
fn display_lists_problems_one_per_line() {
    let source = format!("{MINIMAL}args = [\"-v\"]\nactivation = [\"startup\", \"startup\"]\n");
    let err = Manifest::from_toml_str(&source).unwrap_err();
    assert_eq!(
        err.to_string(),
        "6:8: args: `args` only applies to process mods, and this is a core mod\n\
         7:26: activation[1]: duplicate activation event `startup` (also at activation[0])"
    );
}

#[test]
fn tier_and_entry_must_agree() {
    let with = |tier: &str, entry: &str| {
        MINIMAL.replace("tier = \"core\"", &format!("tier = \"{tier}\"\n{entry}"))
    };
    let p = problems(&with("wasm", ""));
    assert_eq!(p[0].path.as_deref(), Some("tier"));
    assert!(p[0].message.contains("wasm mods need `entry`"));
    let p = problems(&with("wasm", "entry = \"mod.js\""));
    assert!(p[0].message.contains("ending in `.wasm`"));
    let p = problems(&with("wasm", "entry = \"../mod.wasm\""));
    assert!(p[0].message.contains("`..` is not allowed"));
    let p = problems(&with("process", ""));
    assert!(p[0].message.contains("process mods need `entry`"));
    Manifest::from_toml_str(&with("wasm", "entry = \"dist/mod.wasm\"")).unwrap();
    Manifest::from_toml_str(&with("process", "entry = \"node\"\nargs = [\"main.js\"]")).unwrap();
}

#[test]
fn settings_are_checked_against_their_type() {
    let setting = |body: &str| {
        format!("{MINIMAL}\n[[contributes.settings]]\nkey = \"s\"\ndescription = \"d\"\n{body}\n")
    };
    let first = |body: &str| problems(&setting(body))[0].message.clone();
    assert!(first("type = \"enum\"").contains("need `values`"));
    assert!(
        first("type = \"enum\"\nvalues = [\"a\"]\ndefault = \"b\"").contains("not one of `values`")
    );
    assert!(
        first("type = \"string\"\nvalues = [\"a\"]").contains("only applies to `type = \"enum\"`")
    );
    assert!(
        first("type = \"boolean\"\nminimum = 1").contains("only applies to `integer` and `number`")
    );
    assert!(
        first("type = \"number\"\nminimum = 2\nmaximum = 1").contains("greater than `maximum`")
    );
    assert!(first("type = \"integer\"\nmaximum = 5\ndefault = 9").contains("above `maximum`"));
    assert!(first("type = \"string_list\"\ndefault = [1]").contains("a list of strings"));
    Manifest::from_toml_str(&setting("type = \"number\"\ndefault = 1.5\nminimum = 0")).unwrap();
}

#[test]
fn programmatic_manifests_validate_without_positions() {
    let mut m: Manifest = MINIMAL.parse().unwrap();
    m.validate().unwrap();
    m.entry = Some("x".into());
    let err = m.validate().unwrap_err();
    assert_eq!(err.problems().len(), 1);
    assert_eq!(err.problems()[0].path.as_deref(), Some("entry"));
    assert!(err.problems()[0].position.is_none());
}

#[test]
fn service_dependencies_accept_strings_and_tables() {
    let deps: Vec<ServiceDependency> = toml::from_str::<toml::Table>(
        "d = [\"coqo.git@1\", { service = \"coqo.github@2.1\", optional = true }, { service = \"coqo.x@1\" }]",
    )
    .unwrap()["d"]
        .clone()
        .try_into()
        .unwrap();
    assert_eq!(
        deps[0],
        ServiceDependency::required("coqo.git@1".parse().unwrap())
    );
    assert!(deps[1].optional);
    assert_eq!(deps[1].service.to_string(), "coqo.github@2.1");
    assert!(!deps[2].optional);
}

#[test]
fn contributes_register_every_point() {
    let m = Manifest::from_toml_str(FULL).unwrap();
    let host = TestHost::new();
    let cx = host.context(m.id.clone());
    m.contributes.register(&cx).unwrap();

    assert_eq!(host.contributions::<points::Commands>().len(), 2);
    let bindings = host.contributions::<points::Keybindings>();
    assert_eq!(bindings[0].id.as_str(), "acme.maven-tools.keybinding-0");
    assert_eq!(bindings[0].provider, m.id);
    assert_eq!(
        host.contributions::<points::Menus>()[0].id.as_str(),
        "acme.maven-tools.menu-0"
    );
    assert_eq!(host.contributions::<points::Settings>().len(), 5);
    assert_eq!(host.contributions::<points::SettingsSections>().len(), 1);
    assert_eq!(host.contributions::<points::SidebarViews>().len(), 1);
    assert_eq!(host.contributions::<points::PanelViews>().len(), 1);
    assert_eq!(host.contributions::<points::TabViews>().len(), 1);
    assert_eq!(host.contributions::<points::RailViews>().len(), 1);
    assert_eq!(
        host.contribution::<points::StatusItems>("maven.status")
            .unwrap()
            .text,
        "Maven: idle"
    );
    assert_eq!(host.contributions::<points::Languages>().len(), 2);
    assert_eq!(host.contributions::<points::LspServers>().len(), 1);
    assert_eq!(host.contributions::<points::DebugAdapters>().len(), 1);
    assert!(
        host.contribution::<points::McpTools>("maven_test")
            .is_some()
    );
    assert_eq!(host.contributions::<points::AgentProviders>().len(), 1);
    assert_eq!(host.contributions::<points::SlashCommands>().len(), 1);
    assert_eq!(host.contributions::<points::ContextProviders>().len(), 1);
    assert_eq!(host.contributions::<points::AlertSources>().len(), 1);
    assert_eq!(host.contributions::<points::Themes>().len(), 1);

    // Registering again (a reload) replaces rather than duplicates.
    m.contributes.register(&cx).unwrap();
    assert_eq!(host.contributions::<points::Commands>().len(), 2);
    // Another mod can't take the same ids.
    let other = host.context("acme.other".parse().unwrap());
    assert!(matches!(
        m.contributes.register(&other),
        Err(ModError::DuplicateContribution { .. })
    ));
}
