// SPDX-License-Identifier: Apache-2.0

//! What keeps the daemon alive, and the idle policy (PLAN §4.3).
//!
//! [`Activity`] counts attached clients ([`ConnectionGuard`]) and busy work
//! ([`BusyGuard`]). Services hold a `BusyGuard` while they do something that
//! must outlive the UI, such as a running agent turn or a trace capture:
//!
//! ```ignore
//! let _busy = ctx.activity.busy("agent session 42");
//! // … the daemon will not idle-exit until `_busy` is dropped …
//! ```
//!
//! [`idle_monitor`] watches the activity. With no clients and nothing busy
//! it moves to [`IdlePhase::Idle`], after `stop_services_after` to
//! [`IdlePhase::ServicesStopped`] (services watching the phase release
//! resources, e.g. stop language servers), and after `exit_after` it
//! returns, which shuts the daemon down. Any client or busy guard resets it
//! to [`IdlePhase::Active`]. Timers use `tokio::time`, so tests drive them
//! with a paused (fake) clock.
//!
//! The phase lives in the [`Activity`] and changes under the same lock as
//! the counts: a new connection or busy guard sets it to `Active` at once
//! (not when the monitor next runs), and the monitor moves it on only while
//! nothing is attached or busy. So a snapshot never shows a client attached
//! with the daemon idle (issue #306).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

pub use coqo_protocol::daemon::IdlePhase;
use tokio::sync::watch;
use tokio::time::{Instant, sleep_until};

/// Default time idle before services stop (PLAN §4.3: N = 30 min).
pub const DEFAULT_STOP_SERVICES_AFTER: Duration = Duration::from_secs(30 * 60);
/// Default time idle before the daemon exits (PLAN §4.3: M).
pub const DEFAULT_EXIT_AFTER: Duration = Duration::from_secs(60 * 60);

/// When an idle daemon releases resources and exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdlePolicy {
    /// Idle time before services stop. `None`: never.
    pub stop_services_after: Option<Duration>,
    /// Idle time before the daemon exits. `None`: never ("always on").
    pub exit_after: Option<Duration>,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        Self {
            stop_services_after: Some(DEFAULT_STOP_SERVICES_AFTER),
            exit_after: Some(DEFAULT_EXIT_AFTER),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct State {
    connections: u32,
    busy: BTreeMap<u64, String>,
    next_busy: u64,
    /// Connections and busy guards ever taken: tells the idle monitor that
    /// a client came (and maybe went) since it last looked.
    attaches: u64,
}

impl State {
    fn is_idle(&self) -> bool {
        self.connections == 0 && self.busy.is_empty()
    }
}

fn count_turns(s: &State) -> u32 {
    let n = s
        .busy
        .values()
        .filter(|r| r.starts_with(coqo_agents::TURN_BUSY_PREFIX))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A snapshot of the [`Activity`], taken at one instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivitySnapshot {
    /// Attached clients.
    pub connections: u32,
    /// Reasons of the busy guards held, oldest first.
    pub busy: Vec<String>,
    /// The idle phase. `Active` whenever a client is attached or work is
    /// busy.
    pub phase: IdlePhase,
}

/// Attached clients, busy work and the idle phase. Cheap to clone.
///
/// The phase is written only while the `state` lock is held, so it always
/// agrees with the counts. Lock order: `state`, then `phase`.
#[derive(Clone)]
pub struct Activity {
    state: Arc<watch::Sender<State>>,
    phase: Arc<watch::Sender<IdlePhase>>,
}

impl fmt::Debug for Activity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Activity").field(&self.snapshot()).finish()
    }
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    /// No clients, nothing busy. The phase starts `Active` and turns
    /// `Idle` once [`idle_monitor`] runs.
    pub fn new() -> Self {
        Self {
            state: Arc::new(watch::Sender::new(State::default())),
            phase: Arc::new(watch::Sender::new(IdlePhase::Active)),
        }
    }

    /// Count a client as attached until the guard is dropped.
    pub fn connection(&self) -> ConnectionGuard {
        self.state.send_modify(|s| {
            s.connections += 1;
            s.attaches += 1;
            self.set_active();
        });
        ConnectionGuard {
            state: Arc::clone(&self.state),
        }
    }

    /// Mark work as busy until the guard is dropped. A busy daemon does not
    /// idle-exit, even with no clients attached. `reason` shows up in
    /// `daemon/status`.
    pub fn busy(&self, reason: impl Into<String>) -> BusyGuard {
        let reason = reason.into();
        let mut id = 0;
        self.state.send_modify(|s| {
            id = s.next_busy;
            s.next_busy += 1;
            s.busy.insert(id, reason);
            s.attaches += 1;
            self.set_active();
        });
        BusyGuard {
            state: Arc::clone(&self.state),
            id,
        }
    }

    /// The current counts and phase.
    pub fn snapshot(&self) -> ActivitySnapshot {
        let s = self.state.borrow();
        ActivitySnapshot {
            connections: s.connections,
            busy: s.busy.values().cloned().collect(),
            phase: *self.phase.borrow(),
        }
    }

    /// Agent turns running now: the busy guards whose reason starts with
    /// [`coqo_agents::TURN_BUSY_PREFIX`]. Terminals and other busy work do
    /// not count.
    pub fn running_turns(&self) -> u32 {
        count_turns(&self.state.borrow())
    }

    /// Wait until no agent turn is running ([`Self::running_turns`] is 0).
    pub async fn until_no_turns(&self) {
        let mut rx = self.subscribe();
        // The sender lives as long as `self`, so this only ends with 0.
        let _ = rx.wait_for(|s| count_turns(s) == 0).await;
    }

    /// The idle phase, for services that release resources when the
    /// daemon idles ([`IdlePhase::ServicesStopped`]).
    pub fn idle_phase(&self) -> watch::Receiver<IdlePhase> {
        self.phase.subscribe()
    }

    fn subscribe(&self) -> watch::Receiver<State> {
        self.state.subscribe()
    }

    /// Only with the `state` lock held.
    fn set_active(&self) {
        self.phase.send_if_modified(|p| {
            let changed = *p != IdlePhase::Active;
            *p = IdlePhase::Active;
            changed
        });
    }

    /// Whether nothing was attached or busy since the idle moment at which
    /// `attaches` was read.
    fn idle_since(&self, attaches: u64) -> bool {
        let s = self.state.borrow();
        s.is_idle() && s.attaches == attaches
    }

    /// Move to `phase` if the daemon stayed idle since `attaches` was read,
    /// under the `state` lock so a client attaching at the same time wins.
    /// Returns whether it did.
    fn set_idle_phase(&self, phase: IdlePhase, attaches: u64) -> bool {
        let mut idle = false;
        self.state.send_if_modified(|s| {
            idle = s.is_idle() && s.attaches == attaches;
            if idle {
                self.phase.send_if_modified(|p| {
                    let changed = *p != phase;
                    *p = phase;
                    changed
                });
            }
            // The counts did not change: no need to wake their watchers.
            false
        });
        idle
    }
}

/// An attached client; see [`Activity::connection`].
#[must_use = "the client counts as attached only while the guard lives"]
pub struct ConnectionGuard {
    state: Arc<watch::Sender<State>>,
}

impl fmt::Debug for ConnectionGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionGuard")
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.state.send_modify(|s| s.connections -= 1);
    }
}

/// Busy work; see [`Activity::busy`].
#[must_use = "the work counts as busy only while the guard lives"]
pub struct BusyGuard {
    state: Arc<watch::Sender<State>>,
    id: u64,
}

impl fmt::Debug for BusyGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BusyGuard").field(&self.id).finish()
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.state.send_modify(|s| {
            s.busy.remove(&self.id);
        });
    }
}

/// Run the idle policy. Moves the activity's phase to `Idle` and
/// `ServicesStopped`, and returns when the daemon has been idle for
/// `policy.exit_after`. Never returns when the daemon is always on.
pub async fn idle_monitor(activity: Activity, policy: IdlePolicy) {
    let mut rx = activity.subscribe();
    loop {
        let Ok(attaches) = rx.wait_for(State::is_idle).await.map(|s| s.attaches) else {
            return std::future::pending().await;
        };
        let since = Instant::now();
        if !activity.set_idle_phase(IdlePhase::Idle, attaches) {
            // A client attached since: wait for the next idle moment.
            continue;
        }
        tracing::debug!("idle: no clients and nothing busy");
        let stop_at = policy.stop_services_after.map(|d| since + d);
        let exit_at = policy.exit_after.map(|d| since + d);

        loop {
            let stopped = *activity.phase.borrow() == IdlePhase::ServicesStopped;
            let next = [stop_at.filter(|_| !stopped), exit_at]
                .into_iter()
                .flatten()
                .min();
            let timer = async {
                match next {
                    Some(at) => sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return std::future::pending().await;
                    }
                    // A client that attached (even one that has left again)
                    // set `Active` and resets the timers.
                    if rx.borrow_and_update().attaches != attaches {
                        tracing::debug!("active again");
                        break;
                    }
                }
                () = timer => {
                    let now = Instant::now();
                    if exit_at.is_some_and(|at| now >= at) {
                        if !activity.idle_since(attaches) {
                            break;
                        }
                        tracing::info!(after = ?policy.exit_after, "idle timeout reached; shutting down");
                        return;
                    }
                    if stop_at.is_some_and(|at| now >= at) {
                        if !activity.set_idle_phase(IdlePhase::ServicesStopped, attaches) {
                            break;
                        }
                        tracing::info!(after = ?policy.stop_services_after, "idle: stopping services");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task::JoinHandle;
    use tokio::time::advance;

    const MIN: Duration = Duration::from_secs(60);

    fn policy(stop: Option<u64>, exit: Option<u64>) -> IdlePolicy {
        IdlePolicy {
            stop_services_after: stop.map(|m| MIN * m as u32),
            exit_after: exit.map(|m| MIN * m as u32),
        }
    }

    fn start(
        activity: &Activity,
        policy: IdlePolicy,
    ) -> (JoinHandle<()>, watch::Receiver<IdlePhase>) {
        let rx = activity.idle_phase();
        (tokio::spawn(idle_monitor(activity.clone(), policy)), rx)
    }

    /// Let spawned tasks run without moving the (paused) clock.
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn guards_count_and_release() {
        let activity = Activity::new();
        let c1 = activity.connection();
        let c2 = activity.connection();
        let b = activity.busy("agent session 1");
        assert_eq!(
            activity.snapshot(),
            ActivitySnapshot {
                connections: 2,
                busy: vec!["agent session 1".into()],
                phase: IdlePhase::Active,
            }
        );
        drop((c1, b));
        assert_eq!(activity.snapshot().connections, 1);
        assert!(activity.snapshot().busy.is_empty());
        drop(c2);
        assert_eq!(activity.snapshot().connections, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_daemon_stops_services_then_exits() {
        let activity = Activity::new();
        let conn = activity.connection();
        let (monitor, phase) = start(&activity, policy(Some(30), Some(60)));
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Active);

        advance(MIN * 120).await;
        settle().await;
        assert!(!monitor.is_finished(), "a client is attached");

        drop(conn);
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Idle);

        advance(MIN * 29).await;
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Idle);
        advance(MIN).await;
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::ServicesStopped);
        assert!(!monitor.is_finished());

        advance(MIN * 29).await;
        settle().await;
        assert!(!monitor.is_finished());
        advance(MIN).await;
        settle().await;
        assert!(monitor.is_finished(), "exits after 60 idle minutes");
    }

    #[tokio::test(start_paused = true)]
    async fn closing_the_last_client_does_not_stop_a_busy_daemon() {
        let activity = Activity::new();
        let conn = activity.connection();
        let busy = activity.busy("agent session 7");
        let (monitor, phase) = start(&activity, policy(Some(1), Some(2)));
        settle().await;

        drop(conn);
        advance(MIN * 600).await;
        settle().await;
        assert!(!monitor.is_finished(), "busy work keeps it alive");
        assert_eq!(*phase.borrow(), IdlePhase::Active);

        drop(busy);
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Idle);
        advance(MIN * 2).await;
        settle().await;
        assert!(monitor.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_client_resets_the_timers() {
        let activity = Activity::new();
        let (monitor, phase) = start(&activity, policy(Some(10), Some(20)));
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Idle);

        advance(MIN * 15).await;
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::ServicesStopped);

        let conn = activity.connection();
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::Active);
        drop(conn);
        settle().await;

        // The 20 minutes start over from the disconnect.
        advance(MIN * 19).await;
        settle().await;
        assert!(!monitor.is_finished());
        assert_eq!(*phase.borrow(), IdlePhase::ServicesStopped);
        advance(MIN).await;
        settle().await;
        assert!(monitor.is_finished());
    }

    /// #306: `daemon/status` on a just-accepted connection saw `Idle`
    /// because the monitor had not run since the client attached.
    #[tokio::test(start_paused = true)]
    async fn a_client_is_active_before_the_monitor_runs() {
        let activity = Activity::new();
        let (_monitor, phase) = start(&activity, policy(Some(10), Some(20)));
        settle().await;
        assert_eq!(activity.snapshot().phase, IdlePhase::Idle);

        // No yield: the monitor has not seen the client yet.
        let conn = activity.connection();
        let snapshot = activity.snapshot();
        assert_eq!(
            (snapshot.connections, snapshot.phase),
            (1, IdlePhase::Active)
        );
        assert_eq!(*phase.borrow(), IdlePhase::Active);
        drop(conn);

        let busy = activity.busy("agent session 3");
        assert_eq!(activity.snapshot().phase, IdlePhase::Active);
        drop(busy);
        settle().await;
        assert_eq!(activity.snapshot().phase, IdlePhase::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_comes_and_goes_unseen_resets_the_timers() {
        let activity = Activity::new();
        let (monitor, phase) = start(&activity, policy(Some(10), Some(20)));
        settle().await;
        advance(MIN * 9).await;
        settle().await;

        // Attach and detach before the monitor runs.
        drop(activity.connection());
        settle().await;
        assert_eq!(
            *phase.borrow(),
            IdlePhase::Idle,
            "idle again, not stuck at Active"
        );

        advance(MIN * 9).await;
        settle().await;
        assert_eq!(
            *phase.borrow(),
            IdlePhase::Idle,
            "the 10 minutes start over"
        );
        advance(MIN).await;
        settle().await;
        assert_eq!(*phase.borrow(), IdlePhase::ServicesStopped);
        advance(MIN * 10).await;
        settle().await;
        assert!(monitor.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn always_on_never_exits() {
        let activity = Activity::new();
        let (monitor, phase) = start(&activity, policy(Some(30), None));
        settle().await;
        advance(MIN * 60 * 24 * 7).await;
        settle().await;
        assert!(!monitor.is_finished());
        assert_eq!(*phase.borrow(), IdlePhase::ServicesStopped);
    }

    #[tokio::test(start_paused = true)]
    async fn exit_before_stop_services_exits() {
        let activity = Activity::new();
        let (monitor, _phase) = start(&activity, policy(Some(30), Some(5)));
        settle().await;
        advance(MIN * 5).await;
        settle().await;
        assert!(monitor.is_finished());
    }
}
