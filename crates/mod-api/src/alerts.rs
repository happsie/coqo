// SPDX-License-Identifier: Apache-2.0

//! `coqo.alerts@1`: raise alerts in the alert center (PLAN §7.9).
//!
//! A mod with something the user should know about (a review requested, CI
//! failed, a trace run finished) declares an alert source on the
//! `alert_sources` point ([`AlertSource`](crate::contrib::AlertSource), in
//! its manifest or with [`ModContext::contribute`](crate::ModContext)), then
//! raises alerts under it through this service, which the daemon provides:
//!
//! ```
//! # use std::sync::Arc;
//! # use coqo_mod_api::{ModContext, ModError};
//! use coqo_mod_api::alerts::{AlertLevel, Alerts, RaiseAlert};
//!
//! fn ci_failed(cx: &ModContext, pr: u64) -> Result<(), ModError> {
//!     let alerts = cx.consume::<dyn Alerts>()?;
//!     alerts.raise(
//!         RaiseAlert::new("ci.checks".parse()?, format!("pr-{pr}"), AlertLevel::Error,
//!             format!("CI failed on #{pr}"))
//!             .with_body("2 of 14 checks failed"),
//!     )
//! }
//! ```
//!
//! One alert stands per source and key: raising again replaces it (a new
//! level or link makes it unread again), and [`Alerts::resolve`] marks it
//! resolved when the condition clears. The user reads, dismisses and mutes
//! sources in the alert center.

use crate::id::ContributionId;
use crate::service::Service;
use crate::{ModError, ServiceVersion};

/// How much an alert wants the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertLevel {
    /// Something waits for the user.
    NeedsYou,
    /// Something failed.
    Error,
    /// Something finished.
    Done,
    /// Worth knowing.
    Info,
}

/// Where opening an alert takes the user.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AlertLink {
    /// An agent session's chat.
    Session {
        /// The session id.
        session: String,
    },
    /// A file in the alert's workspace.
    File {
        /// The workspace root's id.
        root: u32,
        /// `/`-separated path relative to the root.
        path: String,
        /// The 0-based line, if a line is meant.
        line: Option<u32>,
    },
}

/// An alert to raise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaiseAlert {
    /// The alert source, contributed to `alert_sources`.
    pub source: ContributionId,
    /// The source's key: raising under the same key replaces the alert.
    pub key: String,
    /// The workspace it is about (its id), if any.
    pub workspace: Option<String>,
    /// How much it wants the user.
    pub level: AlertLevel,
    /// One line.
    pub title: String,
    /// More detail, plain text.
    pub body: Option<String>,
    /// Where opening it goes.
    pub link: Option<AlertLink>,
}

impl RaiseAlert {
    /// An alert of `source` under `key`.
    pub fn new(
        source: ContributionId,
        key: impl Into<String>,
        level: AlertLevel,
        title: impl Into<String>,
    ) -> Self {
        Self {
            source,
            key: key.into(),
            workspace: None,
            level,
            title: title.into(),
            body: None,
            link: None,
        }
    }

    /// About workspace `id`.
    pub fn in_workspace(mut self, id: impl Into<String>) -> Self {
        self.workspace = Some(id.into());
        self
    }

    /// With more detail.
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Opening it goes to `link`.
    pub fn with_link(mut self, link: AlertLink) -> Self {
        self.link = Some(link);
        self
    }
}

/// The alert center, `coqo.alerts@1`. See the module docs.
pub trait Alerts: Send + Sync {
    /// Raise (or replace) the alert of its source and key. Fails if the
    /// source is not contributed to `alert_sources`. A muted source raises
    /// nothing.
    fn raise(&self, alert: RaiseAlert) -> Result<(), ModError>;

    /// The condition of `source`'s alert under `key` cleared.
    fn resolve(&self, source: &ContributionId, key: &str) -> Result<(), ModError>;
}

impl Service for dyn Alerts {
    const ID: &'static str = "coqo.alerts";
    const VERSION: ServiceVersion = ServiceVersion::new(1, 0);
}
