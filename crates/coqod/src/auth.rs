// SPDX-License-Identifier: Apache-2.0

//! Who may attach to the daemon (PLAN §9, ADR 0007).
//!
//! Three checks, in order:
//!
//! 1. **The runtime directory** is the user's own and private
//!    ([`coqo_client::RuntimeDir`]), checked in [`daemon::acquire`].
//! 2. **Peer credentials:** every accepted connection must come from the
//!    daemon's own uid ([`admit_peer`]). Others are dropped before a byte
//!    is read.
//! 3. **Session tokens:** `coqo/hello` must carry a token [`Auth`] knows.
//!    The daemon token (random, per daemon start, in `coqod.token`) grants
//!    [`Scope::Daemon`]; tokens from [`Auth::mint_session`] grant one agent
//!    session in one workspace, for `coqod mcp`.
//!
//! [`daemon::acquire`]: crate::daemon::acquire

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, Result};
use coqo_client::{RuntimeDir, TOKEN_FILE};
use coqo_protocol::{Authenticator, Scope, SessionToken, Unauthorized};
use nix::fcntl::{OFlag, renameat};
use nix::unistd::{UnlinkatFlags, unlinkat};
use tokio::net::UnixStream;

/// Random bytes per token (256 bits).
const TOKEN_BYTES: usize = 32;

/// A fresh random token: 32 bytes from the OS, hex-encoded.
pub fn generate_token() -> io::Result<SessionToken> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)?;
    let mut hex = String::with_capacity(TOKEN_BYTES * 2);
    for b in bytes {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(SessionToken::new(hex))
}

/// The tokens the daemon accepts. Cheap to clone; clones share the table.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Mutex<Tokens>>,
}

struct Tokens {
    daemon: SessionToken,
    sessions: Vec<(SessionToken, Scope)>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("sessions", &self.lock().sessions.len())
            .finish_non_exhaustive()
    }
}

impl Auth {
    /// A table with a fresh random daemon token and no session tokens.
    pub fn generate() -> io::Result<Self> {
        Ok(Self::with_daemon_token(generate_token()?))
    }

    /// A table whose daemon token is `token`.
    pub fn with_daemon_token(token: SessionToken) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Tokens {
                daemon: token,
                sessions: Vec::new(),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Tokens> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The daemon token, which grants [`Scope::Daemon`]. Written to
    /// `coqod.token`; never log it.
    pub fn daemon_token(&self) -> SessionToken {
        self.lock().daemon.clone()
    }

    /// Mint a token for one agent session in one workspace: what the agents
    /// service hands `coqod mcp` (in its environment, never its `argv`; see
    /// `coqo_client::SESSION_TOKEN_ENV`). Calls through it get
    /// [`Scope::Session`]: the router refuses them on every route that does
    /// not open to session tokens, and keeps the rest to that workspace
    /// ([`coqo_protocol::Access`]).
    pub fn mint_session(
        &self,
        workspace: impl Into<String>,
        session: impl Into<String>,
    ) -> io::Result<SessionToken> {
        let token = generate_token()?;
        let scope = Scope::Session {
            workspace: workspace.into(),
            session: session.into(),
        };
        self.lock().sessions.push((token.clone(), scope));
        Ok(token)
    }

    /// Stop accepting `token` for new connections. Connections already
    /// attached with it keep their scope until they close. Returns whether
    /// it was known.
    pub fn revoke(&self, token: &SessionToken) -> bool {
        let mut tokens = self.lock();
        let before = tokens.sessions.len();
        tokens.sessions.retain(|(t, _)| t != token);
        tokens.sessions.len() != before
    }

    /// Revoke every token minted for `session` in `workspace` (when the
    /// agent session ends). Returns how many there were.
    pub fn revoke_session(&self, workspace: &str, session: &str) -> usize {
        let mut tokens = self.lock();
        let before = tokens.sessions.len();
        tokens.sessions.retain(|(_, scope)| {
            !matches!(scope, Scope::Session { workspace: w, session: s } if w == workspace && s == session)
        });
        before - tokens.sessions.len()
    }
}

impl Authenticator for Auth {
    fn authenticate(&self, token: Option<&SessionToken>) -> Result<Scope, Unauthorized> {
        let Some(token) = token else {
            return Err(Unauthorized(format!(
                "coqod requires a session token: send the one in `{TOKEN_FILE}` in its runtime \
                 directory (coqo-client does this; this client may be too old)"
            )));
        };
        let tokens = self.lock();
        // `==` on tokens is constant-time; check every entry so the time
        // does not depend on which one matches either.
        let mut scope = (*token == tokens.daemon).then_some(Scope::Daemon);
        for (t, s) in &tokens.sessions {
            if t == token && scope.is_none() {
                scope = Some(s.clone());
            }
        }
        scope.ok_or_else(|| {
            Unauthorized(
                "invalid session token (if coqod restarted, reconnect to pick up its new token)"
                    .to_owned(),
            )
        })
    }
}

/// Write the daemon token to `coqod.token` (`0600`): to a temporary file
/// first, then renamed into place, all relative to the verified directory.
/// Only call with the single-instance lock held.
pub fn write_token_file(dir: &RuntimeDir, token: &SessionToken) -> Result<()> {
    let tmp = format!("{TOKEN_FILE}.tmp");
    let path = dir.path().join(TOKEN_FILE);
    // A leftover from a daemon that crashed mid-write; we hold the lock.
    match unlinkat(dir, tmp.as_str(), UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
        Err(e) => return Err(e).with_context(|| format!("cannot replace {}", path.display())),
    }
    let mut file = dir
        .open_file(
            &tmp,
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            0o600,
        )
        .with_context(|| format!("cannot create {}", path.display()))?;
    // The umask can only remove bits, but be explicit.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| writeln!(file, "{}", token.expose()))
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", path.display()))?;
    renameat(dir, tmp.as_str(), dir, TOKEN_FILE)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Remove `coqod.token` on shutdown, so a stale token does not linger.
pub fn remove_token_file(dir: &RuntimeDir) {
    match unlinkat(dir, TOKEN_FILE, UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
        Err(e) => {
            tracing::warn!(file = %dir.path().join(TOKEN_FILE).display(), error = %e, "cannot remove the token file");
        }
    }
}

/// Why a connection was refused before the handshake.
#[derive(Debug, thiserror::Error)]
pub enum PeerRejected {
    /// The peer runs as another user.
    #[error("connection from uid {uid} (pid {pid:?}) rejected: coqod serves only uid {allowed}")]
    OtherUser {
        /// The peer's uid.
        uid: u32,
        /// The peer's pid, where the OS reports it.
        pid: Option<i32>,
        /// The daemon's uid.
        allowed: u32,
    },
    /// The OS could not say who the peer is.
    #[error("connection rejected: cannot read its peer credentials: {0}")]
    Unknown(io::Error),
}

/// Let in only peers running as `allowed` (`SO_PEERCRED` on Linux,
/// `getpeereid` on macOS).
pub fn admit_peer(stream: &UnixStream, allowed: u32) -> Result<(), PeerRejected> {
    let cred = stream.peer_cred().map_err(PeerRejected::Unknown)?;
    if cred.uid() == allowed {
        Ok(())
    } else {
        Err(PeerRejected::OtherUser {
            uid: cred.uid(),
            pid: cred.pid(),
            allowed,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use coqo_client::current_uid;

    use super::*;

    #[test]
    fn tokens_are_random_hex() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.expose().len(), 64);
        assert!(a.expose().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn missing_and_wrong_tokens_are_rejected() {
        let auth = Auth::generate().unwrap();
        let e = auth.authenticate(None).unwrap_err();
        assert!(e.0.contains(TOKEN_FILE), "{e:?}");
        assert!(auth.authenticate(Some(&SessionToken::new("x"))).is_err());
        let daemon = auth.daemon_token();
        assert!(!e.0.contains(daemon.expose()));
        assert_eq!(auth.authenticate(Some(&daemon)), Ok(Scope::Daemon));
    }

    #[test]
    fn session_tokens_are_scoped_and_revocable() {
        let auth = Auth::generate().unwrap();
        let a = auth.mint_session("ws-a", "s-1").unwrap();
        let b = auth.mint_session("ws-b", "s-2").unwrap();
        let scope = auth.authenticate(Some(&a)).unwrap();
        assert!(scope.allows_workspace("ws-a"));
        assert!(!scope.allows_workspace("ws-b"));
        assert!(scope.require_daemon("daemon/shutdown").is_err());
        assert!(auth.revoke(&a));
        assert!(!auth.revoke(&a));
        assert!(auth.authenticate(Some(&a)).is_err());
        assert_eq!(auth.revoke_session("ws-b", "s-2"), 1);
        assert!(auth.authenticate(Some(&b)).is_err());
        // The daemon token is not revocable this way.
        assert!(!auth.revoke(&auth.daemon_token()));
        assert!(auth.authenticate(Some(&auth.daemon_token())).is_ok());
    }

    #[test]
    fn debug_does_not_print_tokens() {
        let auth = Auth::generate().unwrap();
        let _ = auth.mint_session("ws", "s").unwrap();
        let debug = format!("{auth:?}");
        assert!(!debug.contains(auth.daemon_token().expose()), "{debug}");
    }

    #[test]
    fn token_file_is_private_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = RuntimeDir::create(&tmp.path().join("run")).unwrap();
        let first = generate_token().unwrap();
        write_token_file(&dir, &first).unwrap();
        let meta = std::fs::metadata(dir.path().join(TOKEN_FILE)).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o600);
        assert_eq!(meta.uid(), current_uid());
        assert_eq!(dir.read_token().unwrap(), Some(first));

        let second = generate_token().unwrap();
        write_token_file(&dir, &second).unwrap();
        assert_eq!(dir.read_token().unwrap(), Some(second));

        remove_token_file(&dir);
        assert_eq!(dir.read_token().unwrap(), None);
        remove_token_file(&dir);
    }

    #[tokio::test]
    async fn peers_of_another_uid_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();

        admit_peer(&stream, current_uid()).expect("our own uid is let in");
        // Connecting as another uid needs root; pretend the daemon runs as
        // someone else instead, which exercises the same comparison.
        let other = current_uid().wrapping_add(1);
        match admit_peer(&stream, other) {
            Err(PeerRejected::OtherUser { uid, allowed, .. }) => {
                assert_eq!(uid, current_uid());
                assert_eq!(allowed, other);
            }
            res => panic!("expected OtherUser, got {res:?}"),
        }
    }
}
