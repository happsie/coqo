// SPDX-License-Identifier: Apache-2.0

//! `coqod`: the Coqo background daemon (PLAN §4.3).
//!
//! - [`daemon`]: runtime directory, single-instance lock, socket, accept
//!   loop and shutdown; [`DaemonContext`] is the state services get.
//! - [`lifecycle`]: attached clients, busy guards and the idle policy.
//! - [`routes`]: **where services register their protocol routes.**
//! - [`methods`]: `daemon/status` and `daemon/shutdown`.
//! - [`cli`]: command-line arguments.
//!
//! Clients reach the daemon through `coqo-client`, never this crate.

pub mod cli;
pub mod daemon;
pub mod lifecycle;
pub mod methods;
pub mod routes;

pub use daemon::DaemonContext;
pub use lifecycle::{Activity, BusyGuard, IdlePhase, IdlePolicy};
