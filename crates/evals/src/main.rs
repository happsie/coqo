// SPDX-License-Identifier: Apache-2.0

//! `cargo run -p coqo-evals`: does Claude use Coqo's editor tools at the
//! right moments (#216, docs/evals.md)?
//!
//! Starts a private `coqod` (temporary `COQO_RUNTIME_DIR` and
//! `COQO_DATA_DIR`, the Claude ACP adapter copied from the user's data
//! directory), then runs each scenario ([`scenarios::SCENARIOS`]) on a fresh
//! copy of its fixture against the user's own, signed-in `claude`, playing
//! the UI over the protocol: it publishes editor context (`editor/context`),
//! follows reveals, answers questions (`questions/answer`) and permission
//! requests (allows everything but shell commands). It prints a pass table
//! with token use per scenario.
//!
//! It spends real usage and is never part of `cargo test` or CI. Claude
//! Code keeps its own transcripts of these runs, as of any session; this
//! tool never reads or touches them.

mod scenarios;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use coqo_client::{DATA_DIR_ENV, DaemonPaths, RUNTIME_DIR_ENV, connect};
use coqo_protocol::methods::agents::{
    self as agm, ContentBlock, CreateSessionParams, PermissionOptionKind, PermissionOutcome,
    PermissionRequest, RespondPermissionParams, SessionId, SessionParams, SessionUpdate, ToolKind,
};
use coqo_protocol::methods::editor::{
    self as edm, EditorTab, Position, PublishContextParams, Range, RevealsParams, Selection,
    WindowContext,
};
use coqo_protocol::methods::questions::{self as qm, AnswerParams, Question};
use coqo_protocol::methods::workspace::{self as wsm, WorkspaceInfo};
use coqo_protocol::{Client, ClientConfig};
use futures_util::StreamExt as _;

use crate::scenarios::{Outcome, SCENARIOS, Scenario};

/// The Claude provider's id.
const PROVIDER: &str = "claude-acp";
/// How long one scenario may take.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(300);

const USAGE: &str = "\
Usage: cargo run -p coqo-evals -- [options]

Runs Coqo's agent evals against your own signed-in `claude` (real usage).

Options:
  --without-guide   Run with Coqo's operating guide off (COQO_CLAUDE_GUIDE=0),
                    for an A/B against the default.
  --only <name>     Run only this scenario (repeatable).
  --list            List the scenarios and exit.
  --coqod <path>    Use this coqod binary instead of building one.
  --keep            Keep the temporary directory (logs, fixture copies).
";

#[derive(Debug, Default)]
struct Args {
    without_guide: bool,
    only: Vec<String>,
    list: bool,
    coqod: Option<PathBuf>,
    keep: bool,
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--without-guide" => args.without_guide = true,
            "--only" => args.only.push(it.next().context("--only needs a name")?),
            "--list" => args.list = true,
            "--coqod" => args.coqod = Some(it.next().context("--coqod needs a path")?.into()),
            "--keep" => args.keep = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    Ok(args)
}

/// The repository root (this crate is `crates/evals`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds `coqod` with the profile this binary was built with; its path.
fn build_coqod() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("no binary directory")?;
    let release = dir.ends_with("release");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build
        .args(["build", "-q", "-p", "coqod", "--bin", "coqod"])
        .current_dir(repo_root());
    if release {
        build.arg("--release");
    }
    let status = build.status().context("cannot run cargo build")?;
    if !status.success() {
        bail!("cargo build -p coqod failed");
    }
    Ok(dir.join("coqod"))
}

/// `cp -a`, which keeps the adapter's `node_modules/.bin` symlinks.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let status = Command::new("cp").arg("-a").arg(from).arg(to).status()?;
    if !status.success() {
        bail!("cannot copy {} to {}", from.display(), to.display());
    }
    Ok(())
}

/// The user's installed adapter (read only; it is copied).
fn installed_adapter() -> Result<PathBuf> {
    let data = match std::env::var_os(DATA_DIR_ENV) {
        Some(d) => PathBuf::from(d),
        None if cfg!(target_os = "macos") => {
            PathBuf::from(std::env::var_os("HOME").context("no HOME")?)
                .join("Library/Application Support/Coqo")
        }
        None => match std::env::var_os("XDG_DATA_HOME") {
            Some(d) => PathBuf::from(d).join("coqo"),
            None => PathBuf::from(std::env::var_os("HOME").context("no HOME")?)
                .join(".local/share/coqo"),
        },
    };
    let adapter = data.join("claude-acp");
    if !adapter.join("node_modules").is_dir() {
        bail!(
            "the Claude ACP adapter is not installed in {} (install it from Coqo's chat first)",
            adapter.display()
        );
    }
    Ok(adapter)
}

/// The private daemon and its directories.
struct Daemon {
    root: PathBuf,
    run: PathBuf,
    child: Child,
    keep: bool,
}

impl Daemon {
    fn start(coqod: &Path, without_guide: bool, keep: bool) -> Result<Self> {
        // A short path: the socket's must fit `sun_path`.
        let root = PathBuf::from(format!("/tmp/coqo-eval-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::create_dir_all(root.join("data"))?;
        std::fs::create_dir_all(root.join("work"))?;
        copy_tree(&installed_adapter()?, &root.join("data"))?;
        let run = root.join("run");
        let log = std::fs::File::create(root.join("coqod.log"))?;
        let mut command = Command::new(coqod);
        command
            .args(["--foreground", "--always-on"])
            .env(RUNTIME_DIR_ENV, &run)
            .env(DATA_DIR_ENV, root.join("data"))
            .env("COQO_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        if without_guide {
            command.env("COQO_CLAUDE_GUIDE", "0");
        } else {
            command.env_remove("COQO_CLAUDE_GUIDE");
        }
        let child = command.spawn().context("cannot start coqod")?;
        Ok(Self {
            root,
            run,
            child,
            keep,
        })
    }

    async fn connect(&self) -> Result<Client> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let config = ClientConfig::new("coqo-evals", env!("CARGO_PKG_VERSION"));
            match connect(&DaemonPaths::at(&self.run), config).await {
                Ok((client, mut notifications)) => {
                    tokio::spawn(async move { while notifications.recv().await.is_some() {} });
                    return Ok(client);
                }
                Err(e) if Instant::now() > deadline => {
                    bail!("coqod did not start: {e} (log: {})", self.log().display())
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn log(&self) -> PathBuf {
        self.root.join("coqod.log")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // SIGTERM first, so coqod stops its agent processes itself.
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.keep {
            eprintln!("kept {}", self.root.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// The path of `path` relative to the workspace's first root, if inside.
fn relative(ws: &WorkspaceInfo, path: &str) -> String {
    let root = &ws.roots[0].path;
    path.strip_prefix(root.as_str())
        .map(|p| p.trim_start_matches('/').to_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// The simulated window's context for `scenario`.
fn window_context(scenario: &Scenario, ws: &WorkspaceInfo) -> Option<WindowContext> {
    if scenario.tabs.is_empty() {
        return None;
    }
    let root = &ws.roots[0];
    let tabs = scenario
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let text =
                std::fs::read_to_string(Path::new(&root.path).join(tab.path)).unwrap_or_default();
            let selections = tab
                .selection
                .map(|(first, last)| {
                    let lines: Vec<&str> = text.lines().collect();
                    let end_line = last.min(lines.len() as u32);
                    let selected =
                        lines[(first as usize - 1).min(lines.len())..end_line as usize].join("\n");
                    let end_char = lines
                        .get(end_line as usize - 1)
                        .map_or(0, |l| l.chars().count() as u32);
                    vec![Selection {
                        range: Range::new(
                            Position::new(first - 1, 0),
                            Position::new(end_line - 1, end_char),
                        ),
                        text: Some(selected),
                        truncated: false,
                    }]
                })
                .unwrap_or_default();
            let lines = text.lines().count() as u32;
            EditorTab {
                root: root.id,
                path: tab.path.into(),
                active: i == 0,
                dirty: false,
                selections,
                visible: Some(edm::LineRange {
                    start: 0,
                    end: lines.min(40),
                }),
            }
        })
        .collect();
    Some(WindowContext {
        focused: true,
        tabs,
    })
}

/// The option to pick for a permission request: allow, except for shell
/// commands.
fn permission_answer(request: &PermissionRequest) -> PermissionOutcome {
    let shell = request
        .tool_call
        .as_ref()
        .and_then(|c| c.kind)
        .is_some_and(|k| k == ToolKind::Execute);
    let wanted: &[PermissionOptionKind] = if shell {
        &[
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
        ]
    } else {
        &[
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
        ]
    };
    request
        .options
        .iter()
        .find(|o| wanted.contains(&o.kind))
        .map_or(PermissionOutcome::Cancelled, |o| {
            PermissionOutcome::Selected {
                option_id: o.id.clone(),
            }
        })
}

async fn answer_question(client: &Client, q: &Question, answer: &str) -> Result<()> {
    let params = match q.options.first() {
        Some(option) => AnswerParams {
            id: q.id,
            option: Some(option.id.clone()),
            text: Some(answer.into()),
        },
        None => AnswerParams {
            id: q.id,
            option: None,
            text: Some(answer.into()),
        },
    };
    client.request::<qm::Answer>(params).await?;
    Ok(())
}

/// Runs one scenario on a fresh copy of its fixture.
async fn run(
    client: &Client,
    daemon: &Daemon,
    scenario: &Scenario,
    reveals: &Arc<Mutex<Vec<(String, String)>>>,
) -> Result<Outcome> {
    let dir = daemon.root.join("work").join(scenario.name);
    copy_tree(
        &repo_root().join("evals/fixtures").join(scenario.fixture),
        &dir,
    )?;
    let ws = client
        .request::<wsm::Open>(wsm::OpenParams {
            path: Some(dir.canonicalize()?.display().to_string()),
            ..wsm::OpenParams::default()
        })
        .await?;
    client
        .notify::<edm::PublishContext>(PublishContextParams {
            workspace: ws.id.clone(),
            window: "eval".into(),
            context: window_context(scenario, &ws),
        })
        .await?;
    let info = client
        .request::<agm::Create>(CreateSessionParams {
            provider: PROVIDER.into(),
            cwd: ws.roots[0].path.clone(),
            workspace: Some(ws.id.0.clone()),
            title: Some(format!("eval: {}", scenario.name)),
            mode: None,
        })
        .await?;
    let session = info.id.clone();
    let mut updates = client
        .subscribe::<agm::Updates>(
            SessionParams {
                session: session.clone(),
            },
            Some(0),
        )
        .await?;
    client
        .request::<agm::Prompt>(agm::PromptParams {
            session: session.clone(),
            content: vec![ContentBlock::text(scenario.prompt)],
        })
        .await?;
    let mut outcome = Outcome::default();
    let turn = async {
        while let Some(item) = updates.next().await {
            match item?.event {
                SessionUpdate::PermissionRequested(request) => {
                    client
                        .request::<agm::RespondPermission>(RespondPermissionParams {
                            session: session.clone(),
                            request_id: request.id,
                            outcome: permission_answer(&request),
                            remember: false,
                        })
                        .await?;
                }
                SessionUpdate::Question(q) => {
                    if let Some(anchor) = q.anchors.first() {
                        outcome.questions.push((
                            relative(&ws, &anchor.anchor.path),
                            anchor.anchor.range.start.line + 1,
                        ));
                    }
                    if q.kind.is_question() {
                        answer_question(client, &q, scenario.answer).await?;
                    }
                }
                SessionUpdate::Usage(usage) => {
                    outcome.tokens = usage.used_tokens.or(outcome.tokens);
                    outcome.cost = usage.cost.map(|c| c.amount).or(outcome.cost);
                }
                SessionUpdate::TurnEnded {
                    stop_reason, error, ..
                } => {
                    outcome.ended = match error {
                        Some(e) => format!("{stop_reason:?}: {e}"),
                        None => format!("{stop_reason:?}"),
                    };
                    return Ok::<_, anyhow::Error>(());
                }
                _ => {}
            }
        }
        bail!("the session's updates ended")
    };
    match tokio::time::timeout(SCENARIO_TIMEOUT, turn).await {
        Ok(result) => result?,
        Err(_) => {
            outcome.ended = "timed out".into();
            let _ = client
                .request::<agm::Cancel>(SessionParams {
                    session: session.clone(),
                })
                .await;
        }
    }
    let snapshot = client
        .request::<agm::Get>(SessionParams {
            session: session.clone(),
        })
        .await?;
    outcome.tool_use = snapshot.info.tool_use;
    outcome.reveals = reveals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(w, _)| *w == ws.id.0)
        .map(|(_, path)| path.clone())
        .collect();
    close(client, &session).await;
    Ok(outcome)
}

async fn close(client: &Client, session: &SessionId) {
    let _ = client
        .request::<agm::Close>(SessionParams {
            session: session.clone(),
        })
        .await;
}

fn print_table(results: &[(&Scenario, Result<Outcome>)], without_guide: bool) {
    let guide = if without_guide { "off" } else { "on" };
    println!("\nCoqo agent evals (guide {guide})\n");
    println!("| Scenario | Expected | Coqo tools called | Other tools | Tokens | Result |");
    println!("|---|---|---|---|---|---|");
    let mut passed = 0;
    for (scenario, result) in results {
        match result {
            Ok(o) => {
                let ok = (scenario.check)(o) && !o.ended.starts_with("timed out");
                passed += usize::from(ok);
                let tokens = o
                    .tokens
                    .map_or_else(|| "-".into(), |t| format!("{:.1}k", t as f64 / 1000.));
                println!(
                    "| {} | {} | {} | {} | {} | {}{} |",
                    scenario.name,
                    scenario.expects,
                    o.coqo_summary(),
                    o.agent_calls(),
                    tokens,
                    if ok { "pass" } else { "FAIL" },
                    if o.ended == "EndTurn" {
                        String::new()
                    } else {
                        format!(" ({})", o.ended)
                    },
                );
            }
            Err(e) => println!(
                "| {} | {} | - | - | - | ERROR: {e:#} |",
                scenario.name, scenario.expects
            ),
        }
    }
    println!("\n{passed}/{} passed", results.len());
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let selected: Vec<&Scenario> = SCENARIOS
        .iter()
        .filter(|s| args.only.is_empty() || args.only.iter().any(|o| o == s.name))
        .collect();
    if args.list {
        for s in SCENARIOS {
            println!("{:<20} {}", s.name, s.prompt);
        }
        return Ok(());
    }
    if selected.is_empty() {
        bail!("no scenario matches --only");
    }
    let coqod = match args.coqod.clone() {
        Some(path) => path,
        None => build_coqod()?,
    };
    let daemon = Daemon::start(&coqod, args.without_guide, args.keep)?;
    let client = daemon.connect().await?;
    let providers = client.request::<agm::Providers>(()).await?;
    let claude = providers.iter().find(|p| p.id == PROVIDER);
    if !claude.is_some_and(|p| p.available) {
        bail!("Claude Code is not ready in the eval daemon: {claude:?}");
    }
    // Play a UI that follows reveals.
    let reveals = Arc::new(Mutex::new(Vec::new()));
    let mut follow = client
        .subscribe::<edm::Reveals>(RevealsParams::default(), None)
        .await?;
    let seen = Arc::clone(&reveals);
    tokio::spawn(async move {
        while let Some(Ok(item)) = follow.next().await {
            let event = item.event;
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((event.workspace.0, event.path));
        }
    });
    let mut results = Vec::new();
    for scenario in selected {
        eprintln!("running {} ...", scenario.name);
        let result = run(&client, &daemon, scenario, &reveals).await;
        if let Err(e) = &result {
            eprintln!("  {e:#} (log: {})", daemon.log().display());
        }
        results.push((scenario, result));
    }
    print_table(&results, args.without_guide);
    drop(client);
    drop(daemon);
    Ok(())
}
