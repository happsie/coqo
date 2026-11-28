// SPDX-License-Identifier: Apache-2.0

//! The settings service against the real built-in mods' schema, with a
//! temporary config directory and workspace.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use coqo_protocol::methods::settings::{GetParams, SetParams};
use coqo_protocol::methods::workspace::{OpenParams, WorkspaceId};
use coqo_storage::DataDirs;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::mods::tests::test_builtins;

/// How often the tests' service reads the files.
const TEST_POLL: Duration = Duration::from_millis(20);

struct Fixture {
    tmp: TempDir,
    mods: Mods,
    svc: Arc<SettingsService>,
    workspace: WorkspaceId,
    shutdown: CancellationToken,
}

impl Fixture {
    async fn start(user: Option<&str>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        if let Some(text) = user {
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join(FILE_NAME), text).unwrap();
        }
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mods = Mods::load(test_builtins(&tmp.path().join("mods")));
        mods.start().await;
        let workspaces =
            Arc::new(WorkspaceService::open(DataDirs::at(tmp.path().join("data"))).unwrap());
        let workspace = workspaces
            .open_workspace(OpenParams {
                path: Some(project.display().to_string()),
                id: None,
                name: None,
            })
            .await
            .unwrap()
            .id;
        let shutdown = CancellationToken::new();
        let svc = SettingsService::open(
            &config,
            Some(mods.clone()),
            Some(workspaces),
            TEST_POLL,
            shutdown.clone(),
        );
        Self {
            tmp,
            mods,
            svc,
            workspace,
            shutdown,
        }
    }

    fn user_path(&self) -> std::path::PathBuf {
        self.tmp.path().join("config").join(FILE_NAME)
    }

    fn workspace_path(&self) -> std::path::PathBuf {
        let project = std::fs::canonicalize(self.tmp.path().join("project")).unwrap();
        project.join(WORKSPACE_DIR).join(FILE_NAME)
    }

    async fn get(&self) -> GetResult {
        self.svc
            .get(GetParams {
                workspaces: vec![self.workspace.clone()],
            })
            .await
    }

    async fn set(
        &self,
        key: &str,
        value: Option<Value>,
        workspace: bool,
    ) -> Result<SetResult, ErrorObject> {
        self.svc
            .set(SetParams {
                key: key.into(),
                value,
                workspace: workspace.then(|| self.workspace.clone()),
            })
            .await
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.mods.shutdown().await;
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Wait for `settings/changes`' next event, at most a few seconds.
async fn next_change(feed: &mut coqo_protocol::EventStream<SettingsChanged>) -> u64 {
    let event = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("a change within 5 s")
        .expect("the feed is open")
        .expect("no error");
    event.event.generation
}

#[tokio::test]
async fn loads_the_schema_and_the_users_file() {
    let fx = Fixture::start(Some(
        "# mine\n[appearance]\ntheme = \"dark\"\n\n[files]\nauto_reveal = true\n",
    ))
    .await;
    let got = fx.get().await;
    let ids: Vec<_> = got.sections.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(&ids[..2], ["appearance", "files"], "{ids:?}");
    // Mods' manifests contribute too: Claude's settings get a section.
    assert!(ids.contains(&"claude"), "{ids:?}");
    assert_eq!(got.sections[0].provider, "coqo.workbench");
    assert!(got.user.exists);
    assert!(got.user.problems.is_empty(), "{:?}", got.user.problems);
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("dark")));
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(true))
    );
    // Unset: the default.
    assert_eq!(got.effective("claude.guide", None), Some(json!(true)));
    fx.stop().await;
}

#[tokio::test]
async fn a_missing_file_is_the_defaults() {
    let fx = Fixture::start(None).await;
    let got = fx.get().await;
    assert!(!got.user.exists);
    assert!(got.user.values.is_empty());
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("system")));
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(false))
    );
    fx.stop().await;
}

#[tokio::test]
async fn the_workspace_file_overrides_the_users() {
    let fx = Fixture::start(Some("[files]\nauto_reveal = true\n")).await;
    write(
        &fx.workspace_path(),
        "[files]\nauto_reveal = false\n[appearance]\ntheme = \"light\"\n",
    );
    let got = fx.get().await;
    let ws = &fx.workspace;
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(true))
    );
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, Some(ws)),
        Some(json!(false))
    );
    // The theme is user-only: the workspace's is reported and ignored.
    assert_eq!(
        got.effective(workbench::THEME, Some(ws)),
        Some(json!("system"))
    );
    let own = got.workspace(ws).unwrap();
    assert!(own.path.ends_with(".coqo/settings.toml"), "{}", own.path);
    assert_eq!(own.problems.len(), 1, "{:?}", own.problems);
    assert_eq!(own.problems[0].key.as_deref(), Some(workbench::THEME));
    assert_eq!(own.problems[0].line, Some(4));
    fx.stop().await;
}

#[tokio::test]
async fn invalid_values_are_reported_and_left_out() {
    let fx = Fixture::start(Some(
        "[appearance]\ntheme = \"blue\"\n[files]\nauto_reveal = \"yes\"\nauto_reveel = true\n",
    ))
    .await;
    let got = fx.get().await;
    let messages: Vec<_> = got
        .user
        .problems
        .iter()
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "`appearance.theme`: `blue` is not one of system, dark, light",
            "`files.auto_reveal`: expected true or false",
            "`files.auto_reveel` is not a known setting",
        ]
    );
    assert!(got.user.values.is_empty());
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("system")));
    fx.stop().await;
}

#[tokio::test]
async fn invalid_toml_keeps_the_last_good_values_and_never_crashes() {
    let fx = Fixture::start(Some("[appearance]\ntheme = \"dark\"\n")).await;
    let mut feed = fx.svc.follow().unwrap().events;
    write(&fx.user_path(), "[appearance]\ntheme = \"light\n");
    next_change(&mut feed).await;
    let got = fx.get().await;
    assert_eq!(got.user.problems.len(), 1, "{:?}", got.user.problems);
    let problem = &got.user.problems[0];
    assert_eq!(problem.key, None);
    assert_eq!(problem.line, Some(2));
    assert!(
        problem
            .message
            .starts_with("settings.toml is not valid TOML: line 2"),
        "{problem:?}"
    );
    // The last good value stays in effect.
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("dark")));
    // And a write refuses to clobber the broken file.
    let refused = fx
        .set(workbench::AUTO_REVEAL, Some(json!(true)), false)
        .await
        .unwrap_err();
    assert_eq!(refused.code, SETTINGS_FILE_INVALID, "{refused:?}");
    assert!(refused.message.contains("line 2"), "{refused:?}");
    assert_eq!(
        std::fs::read_to_string(fx.user_path()).unwrap(),
        "[appearance]\ntheme = \"light\n"
    );
    // Fixed by hand: the new value applies.
    write(&fx.user_path(), "[appearance]\ntheme = \"light\"\n");
    next_change(&mut feed).await;
    let got = fx.get().await;
    assert!(got.user.problems.is_empty(), "{:?}", got.user.problems);
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("light")));
    fx.stop().await;
}

#[tokio::test]
async fn a_write_keeps_comments_and_order_and_notifies() {
    let original = "# Coqo settings\n\n[files]\n# off for now\nauto_reveal = false # try it\n\n[editor]\nfont = \"x\"\n";
    let fx = Fixture::start(Some(original)).await;
    let mut feed = fx.svc.follow().unwrap().events;
    let before = fx.get().await.generation;
    let set = fx
        .set(workbench::AUTO_REVEAL, Some(json!(true)), false)
        .await
        .unwrap();
    assert!(set.generation > before);
    assert_eq!(next_change(&mut feed).await, set.generation);
    assert_eq!(
        std::fs::read_to_string(fx.user_path()).unwrap(),
        original.replace("auto_reveal = false", "auto_reveal = true")
    );
    let got = fx.get().await;
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(true))
    );
    // Keys the schema doesn't know (`editor.font`) are left alone but
    // reported.
    assert_eq!(got.user.problems.len(), 1);

    // A new key goes in its own table at the end.
    fx.set(workbench::THEME, Some(json!("light")), false)
        .await
        .unwrap();
    let text = std::fs::read_to_string(fx.user_path()).unwrap();
    assert!(text.starts_with("# Coqo settings\n"), "{text}");
    assert!(
        text.ends_with("[appearance]\ntheme = \"light\"\n"),
        "{text}"
    );
    // `null` removes it again: the default applies.
    fx.set(workbench::THEME, None, false).await.unwrap();
    let got = fx.get().await;
    assert_eq!(got.effective(workbench::THEME, None), Some(json!("system")));
    assert!(
        !std::fs::read_to_string(fx.user_path())
            .unwrap()
            .contains("theme =")
    );
    fx.stop().await;
}

#[tokio::test]
async fn writes_are_checked_against_the_schema() {
    let fx = Fixture::start(None).await;
    let unknown = fx
        .set("files.nope", Some(json!(true)), false)
        .await
        .unwrap_err();
    assert_eq!(unknown.code, UNKNOWN_SETTING);
    let wrong = fx
        .set(workbench::THEME, Some(json!("blue")), false)
        .await
        .unwrap_err();
    assert_eq!(wrong.code, INVALID_SETTING);
    assert_eq!(
        wrong.message,
        "`appearance.theme`: `blue` is not one of system, dark, light"
    );
    let user_only = fx
        .set(workbench::THEME, Some(json!("dark")), true)
        .await
        .unwrap_err();
    assert_eq!(user_only.code, INVALID_SETTING);
    assert!(!fx.user_path().exists(), "nothing was written");

    // A workspace write creates `.coqo/settings.toml`.
    fx.set(workbench::AUTO_REVEAL, Some(json!(true)), true)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fx.workspace_path()).unwrap(),
        "[files]\nauto_reveal = true\n"
    );
    let got = fx.get().await;
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, Some(&fx.workspace)),
        Some(json!(true))
    );
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(false))
    );
    fx.stop().await;
}

#[tokio::test]
async fn edits_by_hand_reload_live() {
    let fx = Fixture::start(None).await;
    // Follow the workspace's file too.
    fx.get().await;
    let mut feed = fx.svc.follow().unwrap().events;
    write(&fx.user_path(), "[files]\nauto_reveal = true\n");
    let first = next_change(&mut feed).await;
    let got = fx.get().await;
    assert_eq!(got.generation, first);
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(true))
    );

    write(&fx.workspace_path(), "[files]\nauto_reveal = false\n");
    assert!(next_change(&mut feed).await > first);
    let got = fx.get().await;
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, Some(&fx.workspace)),
        Some(json!(false))
    );

    // Deleting the file brings the defaults back.
    std::fs::remove_file(fx.user_path()).unwrap();
    next_change(&mut feed).await;
    let got = fx.get().await;
    assert!(!got.user.exists);
    assert_eq!(
        got.effective(workbench::AUTO_REVEAL, None),
        Some(json!(false))
    );
    fx.stop().await;
}

#[test]
fn startup_values_need_no_schema() {
    let dir = tempfile::tempdir().unwrap();
    assert!(startup_values(dir.path()).is_empty());
    std::fs::write(dir.path().join(FILE_NAME), "[claude]\nguide = false\n").unwrap();
    assert_eq!(startup_values(dir.path())["claude.guide"], json!(false));
    std::fs::write(dir.path().join(FILE_NAME), "[claude\n").unwrap();
    assert!(startup_values(dir.path()).is_empty());
}
