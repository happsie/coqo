// SPDX-License-Identifier: Apache-2.0

//! The updater's HTTPS client.
//!
//! Every request is a plain `GET` with a `Host` and a `User-Agent` of
//! `coqo-updater/<version>`, plus an `If-None-Match` when polling the
//! release (so an unchanged release costs a `304`). Nothing else: no
//! cookies, no tokens, no `Accept-*`, nothing about the user or the
//! machine. Only `https` URLs are fetched (tests serve plain HTTP, which
//! only a `cfg(test)` client accepts), redirects are followed a few times
//! and only to `https`, and every body has a size cap and a deadline.

use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Request, Response, StatusCode, Uri, header};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::UpdateError;
use crate::verify::hex;

/// Redirects followed per request (GitHub's asset downloads take one).
const MAX_REDIRECTS: usize = 5;
/// How long a small request (the release, the manifest) may take.
const SMALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an archive download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The `User-Agent` of every request.
pub fn user_agent(version: &str) -> String {
    format!("coqo-updater/{version}")
}

/// A response read into memory.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// `200`, or `304` for an unchanged `If-None-Match`.
    pub status: StatusCode,
    /// The response's `ETag`, for the next poll.
    pub etag: Option<String>,
    /// The body.
    pub body: Vec<u8>,
}

/// The HTTPS client. Cheap to clone.
#[derive(Clone)]
pub struct Http {
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    user_agent: HeaderValue,
    allow_http: bool,
}

impl std::fmt::Debug for Http {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Http")
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl Http {
    /// A client that fetches `https` only, trusting the OS's root
    /// certificates. `version` goes into the `User-Agent`.
    pub fn new(version: &str) -> Result<Self, UpdateError> {
        Self::build(version, false)
    }

    /// A client that also fetches plain `http`: tests' local release
    /// server.
    #[cfg(test)]
    pub(crate) fn for_tests(version: &str) -> Self {
        Self::build(version, true).unwrap()
    }

    fn build(version: &str, allow_http: bool) -> Result<Self, UpdateError> {
        let tls = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(rustls::crypto::ring::default_provider())
            .map_err(|e| UpdateError::Network {
                what: "the system's root certificates".into(),
                message: e.to_string(),
            })?;
        let tls = if allow_http {
            tls.https_or_http()
        } else {
            tls.https_only()
        };
        let connector = tls.enable_http1().build();
        let user_agent = HeaderValue::from_str(&user_agent(version))
            .map_err(|e| UpdateError::Format(format!("bad User-Agent: {e}")))?;
        Ok(Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
            user_agent,
            allow_http,
        })
    }

    /// Parse `url` and check that it may be fetched: absolute, with a host,
    /// and `https`.
    pub fn check_url(&self, url: &str) -> Result<Uri, UpdateError> {
        let uri: Uri = url
            .parse()
            .map_err(|_| UpdateError::Format(format!("not a URL: {url}")))?;
        let scheme_ok = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => self.allow_http,
            _ => false,
        };
        if !scheme_ok || uri.host().is_none() {
            return Err(UpdateError::Format(format!(
                "refusing to fetch {url}: only https URLs are fetched"
            )));
        }
        Ok(uri)
    }

    /// `GET url`, following redirects. `etag` goes out as `If-None-Match`.
    async fn send(
        &self,
        url: &str,
        etag: Option<&str>,
        what: &str,
    ) -> Result<Response<Incoming>, UpdateError> {
        let net = |e: &dyn std::fmt::Display| UpdateError::Network {
            what: what.to_owned(),
            message: e.to_string(),
        };
        let mut uri = self.check_url(url)?;
        for hop in 0..=MAX_REDIRECTS {
            let mut request =
                Request::get(uri.clone()).header(header::USER_AGENT, self.user_agent.clone());
            if hop == 0
                && let Some(etag) = etag
                && let Ok(etag) = HeaderValue::from_str(etag)
            {
                request = request.header(header::IF_NONE_MATCH, etag);
            }
            let request = request.body(Empty::new()).map_err(|e| net(&e))?;
            let response = self.client.request(request).await.map_err(|e| net(&e))?;
            let status = response.status();
            if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .ok_or_else(|| UpdateError::Status {
                        what: what.to_owned(),
                        status: status.as_u16(),
                    })?;
                uri = self.check_url(location)?;
                continue;
            }
            return Ok(response);
        }
        Err(UpdateError::Network {
            what: what.to_owned(),
            message: "too many redirects".into(),
        })
    }

    /// `GET url` into memory: a `200`, or a `304` when `etag` still holds.
    /// A body over `limit` bytes is refused.
    pub async fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        limit: usize,
        what: &str,
    ) -> Result<Fetched, UpdateError> {
        let fetch = async {
            let response = self.send(url, etag, what).await?;
            let status = response.status();
            if status != StatusCode::OK && status != StatusCode::NOT_MODIFIED {
                return Err(UpdateError::Status {
                    what: what.to_owned(),
                    status: status.as_u16(),
                });
            }
            let etag = response
                .headers()
                .get(header::ETAG)
                .and_then(|e| e.to_str().ok())
                .map(str::to_owned);
            let mut body = Vec::new();
            let mut incoming = response.into_body();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|e| UpdateError::Network {
                    what: what.to_owned(),
                    message: e.to_string(),
                })?;
                if let Ok(data) = frame.into_data() {
                    if body.len() + data.len() > limit {
                        return Err(UpdateError::Format(format!(
                            "{what} is larger than {limit} bytes"
                        )));
                    }
                    body.extend_from_slice(&data);
                }
            }
            Ok(Fetched { status, etag, body })
        };
        tokio::time::timeout(SMALL_TIMEOUT, fetch)
            .await
            .map_err(|_| UpdateError::Network {
                what: what.to_owned(),
                message: "timed out".into(),
            })?
    }

    /// Download `url` to `dest` (through `dest.partial`), refusing more
    /// than `size` bytes. Returns the sha256 (hex) and the size of what was
    /// written.
    pub async fn download(
        &self,
        url: &str,
        dest: &Path,
        size: u64,
        what: &str,
    ) -> Result<(String, u64), UpdateError> {
        let partial = dest.with_extension("partial");
        let fetch = async {
            let response = self.send(url, None, what).await?;
            if response.status() != StatusCode::OK {
                return Err(UpdateError::Status {
                    what: what.to_owned(),
                    status: response.status().as_u16(),
                });
            }
            let mut file = tokio::fs::File::create(&partial)
                .await
                .map_err(UpdateError::io(format!(
                    "cannot write {}",
                    partial.display()
                )))?;
            let mut hasher = Sha256::new();
            let mut written = 0u64;
            let mut incoming = response.into_body();
            while let Some(frame) = incoming.frame().await {
                let frame = frame.map_err(|e| UpdateError::Network {
                    what: what.to_owned(),
                    message: e.to_string(),
                })?;
                if let Ok(data) = frame.into_data() {
                    written += data.len() as u64;
                    if written > size {
                        return Err(UpdateError::Mismatch {
                            what: what.to_owned(),
                            detail: format!("larger than the signed {size} bytes"),
                        });
                    }
                    hasher.update(&data);
                    file.write_all(&data)
                        .await
                        .map_err(UpdateError::io(format!(
                            "cannot write {}",
                            partial.display()
                        )))?;
                }
            }
            file.flush().await.map_err(UpdateError::io(format!(
                "cannot write {}",
                partial.display()
            )))?;
            file.sync_all().await.map_err(UpdateError::io(format!(
                "cannot write {}",
                partial.display()
            )))?;
            Ok((hex(&hasher.finalize()), written))
        };
        let result = tokio::time::timeout(DOWNLOAD_TIMEOUT, fetch)
            .await
            .map_err(|_| UpdateError::Network {
                what: what.to_owned(),
                message: "timed out".into(),
            })
            .and_then(|r| r);
        match result {
            Ok(done) => {
                tokio::fs::rename(&partial, dest)
                    .await
                    .map_err(UpdateError::io(format!("cannot write {}", dest.display())))?;
                Ok(done)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&partial).await;
                Err(e)
            }
        }
    }
}
