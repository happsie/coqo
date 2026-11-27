// SPDX-License-Identifier: Apache-2.0

//! The adapter's command line and environment, and the npm install.

use std::ffi::OsString;
use std::path::Path;

use coqo_acp_host::AgentCommand;

use crate::detect::{Claude, Node, adapter_bin};
use crate::{ADAPTER_PACKAGE, ADAPTER_VERSION};

/// `CLAUDE_CODE_*` variables that are the user's own configuration (cloud
/// providers, output limits, client certificates, opt-outs), kept for the
/// adapter. Every other `CLAUDE_CODE_*` variable is a marker of a running
/// Claude Code session and is removed.
const USER_CONFIG_PREFIXES: &[&str] = &[
    "CLAUDE_CODE_USE_",
    "CLAUDE_CODE_SKIP_",
    "CLAUDE_CODE_DISABLE_",
    "CLAUDE_CODE_CLIENT_",
];

const USER_CONFIG: &[&str] = &[
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_API_KEY_HELPER_TTL_MS",
    "CLAUDE_CODE_SUBAGENT_MODEL",
    "CLAUDE_CODE_EFFORT_LEVEL",
];

/// Markers outside the `CLAUDE_CODE_` prefix.
const OTHER_MARKERS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_AGENT_SDK_VERSION",
    "AI_AGENT",
];

/// Whether `key` marks a Claude Code session rather than configuring one.
///
/// coqod may have been started from inside a Claude Code session (a
/// developer's terminal, or Claude Code running Coqo). Its markers would
/// make the user's `claude` think it is a child of that session: for one,
/// `CLAUDE_CODE_CHILD_SESSION` turns transcript saving off, so resume
/// breaks (ADR 0005). `CLAUDE_CODE_EXECUTABLE` is removed too, and set
/// again to the `claude` Coqo found.
pub fn is_session_marker(key: &str) -> bool {
    if OTHER_MARKERS.contains(&key) {
        return true;
    }
    key.starts_with("CLAUDE_CODE_")
        && !USER_CONFIG.contains(&key)
        && !USER_CONFIG_PREFIXES.iter().any(|p| key.starts_with(p))
}

/// The session markers among `vars` (the daemon's environment).
pub fn session_markers(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<OsString> {
    let mut keys: Vec<OsString> = vars
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| k.to_str().is_some_and(is_session_marker))
        .collect();
    keys.sort();
    keys
}

/// How the adapter runs for one workspace: `<node> <dir>/node_modules/.bin/
/// claude-agent-acp`, with `CLAUDE_CODE_EXECUTABLE` set to the user's own,
/// unmodified `claude` (symlinks resolved) and the daemon's environment
/// minus Claude Code session markers. Without plan logins the adapter gets
/// `--hide-claude-auth`.
///
/// Node runs the adapter explicitly, so a `node` found on `PATH` is used
/// even when the script's `#!/usr/bin/env node` would find another one.
pub fn adapter_command(
    adapter_dir: &Path,
    node: &Node,
    claude: &Claude,
    plan_logins: bool,
    daemon_env: impl IntoIterator<Item = (OsString, OsString)>,
) -> AgentCommand {
    let mut command = AgentCommand::new(&node.path).arg(adapter_bin(adapter_dir));
    if !plan_logins {
        command = command.arg("--hide-claude-auth");
    }
    for key in session_markers(daemon_env) {
        command = command.env_remove(key);
    }
    command.env("CLAUDE_CODE_EXECUTABLE", &claude.real)
}

/// `npm install` of the pinned adapter into `adapter_dir`, without optional
/// dependencies: those are the Agent SDK's bundled `claude` binaries, and
/// the adapter runs the user's own `claude` instead.
pub fn npm_install_args(adapter_dir: &Path) -> Vec<OsString> {
    vec![
        "install".into(),
        "--prefix".into(),
        adapter_dir.into(),
        "--omit=optional".into(),
        "--save-exact".into(),
        "--no-fund".into(),
        "--no-audit".into(),
        "--loglevel=error".into(),
        format!("{ADAPTER_PACKAGE}@{ADAPTER_VERSION}").into(),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn vars(keys: &[&str]) -> Vec<(OsString, OsString)> {
        keys.iter().map(|k| ((*k).into(), "x".into())).collect()
    }

    #[test]
    fn session_markers_go_and_configuration_stays() {
        let env = vars(&[
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_SSE_PORT",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_EXECUTABLE",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_PID",
            "CLAUDE_EFFORT",
            "AI_AGENT",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_SKIP_BEDROCK_AUTH",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "CLAUDE_CODE_CLIENT_CERT",
            "CLAUDE_CONFIG_DIR",
        ]);
        let removed: Vec<String> = session_markers(env)
            .into_iter()
            .map(|k| k.into_string().unwrap())
            .collect();
        assert_eq!(
            removed,
            [
                "AI_AGENT",
                "CLAUDECODE",
                "CLAUDE_CODE_CHILD_SESSION",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_EXECPATH",
                "CLAUDE_CODE_EXECUTABLE",
                "CLAUDE_CODE_MESSAGING_TOKEN",
                "CLAUDE_CODE_SESSION_ID",
                "CLAUDE_CODE_SSE_PORT",
                "CLAUDE_EFFORT",
                "CLAUDE_PID",
            ]
        );
    }

    #[test]
    fn the_adapter_runs_the_users_claude_with_node() {
        let node = Node {
            path: "/opt/node/bin/node".into(),
            version: Some((24, 1, 0)),
        };
        let claude = Claude {
            on_path: "/home/u/.local/bin/claude".into(),
            real: "/home/u/.local/share/claude/versions/2.1.0".into(),
        };
        let dir = Path::new("/data/coqo/claude-acp");
        let env = vars(&["PATH", "CLAUDECODE", "CLAUDE_CODE_EXECUTABLE"]);
        let command = adapter_command(dir, &node, &claude, true, env.clone());
        assert_eq!(command.program, PathBuf::from("/opt/node/bin/node"));
        assert_eq!(
            command.args,
            [OsString::from(
                "/data/coqo/claude-acp/node_modules/.bin/claude-agent-acp"
            )]
        );
        assert_eq!(
            command.env,
            [(
                OsString::from("CLAUDE_CODE_EXECUTABLE"),
                OsString::from("/home/u/.local/share/claude/versions/2.1.0")
            )]
        );
        assert_eq!(
            command.env_remove,
            [
                OsString::from("CLAUDECODE"),
                OsString::from("CLAUDE_CODE_EXECUTABLE")
            ]
        );

        let hidden = adapter_command(dir, &node, &claude, false, env);
        assert_eq!(hidden.args[1], "--hide-claude-auth");
    }

    #[test]
    fn npm_installs_the_pinned_adapter_without_optional_dependencies() {
        let args = npm_install_args(Path::new("/data/claude-acp"));
        let args: Vec<_> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(&args[..3], ["install", "--prefix", "/data/claude-acp"]);
        assert!(args.contains(&"--omit=optional"));
        assert_eq!(
            args.last().unwrap(),
            &"@agentclientprotocol/claude-agent-acp@0.113.0"
        );
    }
}
