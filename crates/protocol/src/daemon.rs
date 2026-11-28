// SPDX-License-Identifier: Apache-2.0

//! `daemon/*`: the daemon's own lifecycle methods (PLAN §4.3).
//!
//! - [`Status`] (`daemon/status`): version, pid, uptime, attached clients,
//!   busy work, the idle policy and the memory it and its child processes
//!   use.
//! - [`Shutdown`] (`daemon/shutdown`): ask the daemon to exit. It refuses
//!   while work is busy unless `force` is set.
//! - [`RestartWhenIdle`] (`daemon/restartWhenIdle`): exit once no agent
//!   turn is running (or now), so the next client starts the daemon again,
//!   from the binary that is installed now (an update, ADR 0031).

use serde::{Deserialize, Serialize};

use crate::handshake::ProtocolVersion;
use crate::method::Method;

/// `daemon/status`: what the daemon is and what it is doing.
#[derive(Debug)]
pub enum Status {}
impl Method for Status {
    const NAME: &'static str = "daemon/status";
    type Params = ();
    type Result = DaemonStatus;
}

/// `daemon/shutdown`: stop the daemon.
#[derive(Debug)]
pub enum Shutdown {}
impl Method for Shutdown {
    const NAME: &'static str = "daemon/shutdown";
    type Params = ShutdownParams;
    type Result = ShutdownResult;
}

/// `daemon/restartWhenIdle`: exit cleanly once no agent turn is running,
/// or right away with `now`. Sessions resume in the next daemon (ADR
/// 0026). Calling it again only reports (or, with `now`, restarts at once).
#[derive(Debug)]
pub enum RestartWhenIdle {}
impl Method for RestartWhenIdle {
    const NAME: &'static str = "daemon/restartWhenIdle";
    type Params = RestartParams;
    type Result = RestartResult;
}

/// Params of `daemon/restartWhenIdle`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RestartParams {
    /// Restart now, stopping running agent turns (the UI warns first).
    #[serde(default)]
    pub now: bool,
}

/// Result of `daemon/restartWhenIdle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct RestartResult {
    /// Agent turns running now. The daemon restarts when this reaches 0.
    pub running: u32,
    /// Whether it is exiting now.
    pub restarting: bool,
}

/// Result of `daemon/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    /// The daemon's version, e.g. `0.1.0`.
    pub version: String,
    /// The daemon's build id (also in the handshake's `Welcome.build`).
    pub build: String,
    /// The protocol version the daemon speaks.
    pub protocol: ProtocolVersion,
    /// The daemon's process id.
    pub pid: u32,
    /// When the daemon started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// How long the daemon has been running, in milliseconds.
    pub uptime_ms: u64,
    /// Clients attached right now, including the one asking.
    pub connections: u32,
    /// What keeps the daemon busy (agent sessions, traces, …). Busy work
    /// keeps the daemon alive with no clients attached.
    pub busy: Vec<String>,
    /// The idle policy and where the daemon is in it.
    pub idle: IdleStatus,
    /// The memory the daemon and every process it started (language
    /// servers, agents, terminals, process mods) use. Absent when the
    /// platform can't tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryUsage>,
}

/// The memory of the daemon and its child processes (`daemon/status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct MemoryUsage {
    /// Their resident set size together, in bytes.
    pub rss_bytes: u64,
    /// How many processes: the daemon and its descendants.
    pub processes: u32,
}

/// The idle policy (PLAN §4.3) and the daemon's current idle phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct IdleStatus {
    /// The current phase.
    pub phase: IdlePhase,
    /// Seconds with no clients and nothing busy before idle services (such
    /// as language servers) stop. `null` means never.
    pub stop_services_after_secs: Option<u64>,
    /// Seconds with no clients and nothing busy before the daemon exits.
    /// `null` means never ("always on").
    pub exit_after_secs: Option<u64>,
}

/// Where the daemon is in its idle policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum IdlePhase {
    /// A client is attached or work is busy.
    Active,
    /// Nothing attached and nothing busy; timers are running.
    Idle,
    /// Idle long enough that services released their resources (language
    /// servers stopped). The daemon exits next, unless it is always on.
    ServicesStopped,
}

/// Params of `daemon/shutdown`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ShutdownParams {
    /// Stop even if work is busy.
    #[serde(default)]
    pub force: bool,
}

/// Result of `daemon/shutdown`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
pub struct ShutdownResult {
    /// Whether the daemon is stopping. It closes every connection and exits
    /// shortly after answering.
    pub stopping: bool,
    /// When not stopping: the busy work that kept it running.
    pub busy: Vec<String>,
}
