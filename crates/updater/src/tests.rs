// SPDX-License-Identifier: Apache-2.0

//! The updater end to end against a local fake release server: a fixture
//! release (`manifest.json`, an archive, their minisign signatures) signed
//! with a key made for the test run, served over plain HTTP on 127.0.0.1.
//! Nothing here contacts GitHub.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use blake2::Digest as _;
use flate2::Compression;
use flate2::write::GzEncoder;
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use semver::Version;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::check::{Offer, Updater, UpdaterConfig};
use crate::install::{self, Install, Os};
use crate::release::Channel;
use crate::verify::sha256_hex;
use crate::{TrustedKey, UpdateError};

const TARGET: &str = "aarch64-apple-darwin";
const LINUX: &str = "x86_64-unknown-linux-gnu";

// ---- a signing key for the test run ------------------------------------------

struct TestKey {
    pair: Ed25519KeyPair,
    id: [u8; 8],
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl TestKey {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let mut id = [0; 8];
        rng.fill(&mut id).unwrap();
        Self { pair, id }
    }

    /// The public key file, as `minisign -G` writes it.
    fn public_file(&self) -> String {
        let mut bin = b"Ed".to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(self.pair.public_key().as_ref());
        format!("untrusted comment: test key\n{}\n", b64(&bin))
    }

    fn trusted(&self) -> TrustedKey {
        TrustedKey::parse(&self.public_file()).unwrap()
    }

    /// A `.minisig` of `data`: prehashed (BLAKE2b-512, minisign's default)
    /// or legacy.
    fn sign(&self, data: &[u8], prehashed: bool) -> String {
        let (alg, message) = if prehashed {
            (b"ED", blake2::Blake2b512::digest(data).to_vec())
        } else {
            (b"Ed", data.to_vec())
        };
        let signature = self.pair.sign(&message);
        let trusted = "timestamp:1764288000\tfile:test";
        let mut global = signature.as_ref().to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let global = self.pair.sign(&global);
        let mut bin = alg.to_vec();
        bin.extend_from_slice(&self.id);
        bin.extend_from_slice(signature.as_ref());
        format!(
            "untrusted comment: signature from test key\n{}\ntrusted comment: {trusted}\n{}\n",
            b64(&bin),
            b64(global.as_ref())
        )
    }
}

// ---- the fake release server ---------------------------------------------------

#[derive(Clone)]
struct Route {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    etag: Option<String>,
}

impl Route {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
            etag: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Logged {
    path: String,
    /// Lowercase names.
    headers: Vec<(String, String)>,
}

#[derive(Clone)]
struct Server {
    base: String,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    log: Arc<Mutex<Vec<Logged>>>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = Self {
            base,
            routes: Arc::default(),
            log: Arc::default(),
        };
        let s = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let s = s.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let mut lines = text.split("\r\n");
                    let path = lines
                        .next()
                        .and_then(|l| l.split(' ').nth(1))
                        .unwrap_or_default()
                        .to_owned();
                    let headers: Vec<(String, String)> = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                        .collect();
                    let inm = headers
                        .iter()
                        .find(|(k, _)| k == "if-none-match")
                        .map(|(_, v)| v.clone());
                    s.log.lock().unwrap().push(Logged {
                        path: path.clone(),
                        headers,
                    });
                    let route = s.routes.lock().unwrap().get(&path).cloned();
                    let route = route.unwrap_or(Route {
                        status: 404,
                        ..Route::ok("not found")
                    });
                    let (status, body) = match &route.etag {
                        Some(etag) if inm.as_ref() == Some(etag) => (304, Vec::new()),
                        _ => (route.status, route.body.clone()),
                    };
                    let mut head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    if let Some(etag) = &route.etag {
                        head.push_str(&format!("ETag: {etag}\r\n"));
                    }
                    for (k, v) in &route.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        server
    }

    fn route(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_owned(), route);
    }

    fn log(&self) -> Vec<Logged> {
        self.log.lock().unwrap().clone()
    }

    fn download_path(tag: &str, name: &str) -> String {
        format!("/happsie/coqo/releases/download/{tag}/{name}")
    }
}

// ---- fixture releases --------------------------------------------------------------

fn add_file(b: &mut tar::Builder<GzEncoder<Vec<u8>>>, path: &str, mode: u32, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(mode);
    h.set_entry_type(tar::EntryType::Regular);
    b.append_data(&mut h, path, data).unwrap();
}

fn add_dir(b: &mut tar::Builder<GzEncoder<Vec<u8>>>, path: &str) {
    let mut h = tar::Header::new_gnu();
    h.set_size(0);
    h.set_mode(0o755);
    h.set_entry_type(tar::EntryType::Directory);
    b.append_data(&mut h, path, &[][..]).unwrap();
}

/// `Coqo.app/` (macOS) or `coqo/bin/` (Linux) of `version`, plus `extra`.
fn archive(
    target: &str,
    version: &str,
    extra: impl FnOnce(&mut tar::Builder<GzEncoder<Vec<u8>>>),
) -> Vec<u8> {
    let mut b = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    if target == LINUX {
        add_dir(&mut b, "coqo/");
        add_dir(&mut b, "coqo/bin/");
        add_file(
            &mut b,
            "coqo/bin/coqo",
            0o755,
            format!("coqo {version}").as_bytes(),
        );
        add_file(
            &mut b,
            "coqo/bin/coqod",
            0o4755,
            format!("coqod {version}").as_bytes(),
        );
    } else {
        add_dir(&mut b, "./Coqo.app/");
        add_dir(&mut b, "./Coqo.app/Contents/");
        add_file(&mut b, "./Coqo.app/Contents/Info.plist", 0o666, b"<plist/>");
        add_file(
            &mut b,
            "./Coqo.app/Contents/MacOS/coqo",
            0o755,
            format!("coqo {version}").as_bytes(),
        );
        add_file(
            &mut b,
            "./Coqo.app/Contents/MacOS/coqod",
            0o755,
            format!("coqod {version}").as_bytes(),
        );
    }
    extra(&mut b);
    b.into_inner().unwrap().finish().unwrap()
}

/// What to get wrong in a published release.
#[derive(Default)]
struct Faults {
    /// The manifest is signed by another key.
    manifest_signed_by_other: bool,
    /// The archive is signed by another key.
    archive_signed_by_other: bool,
    /// The manifest's sha256 is of other bytes.
    wrong_sha: bool,
    /// The manifest gives a smaller size.
    smaller_size: bool,
    /// The manifest's version, if not the tag's.
    manifest_version: Option<String>,
    /// Sign legacy (not prehashed).
    legacy: bool,
    /// Mark it a pre-release on GitHub.
    prerelease: bool,
}

/// Publish `version` for `target` on `server`: the release JSON at
/// `releases/latest` (and in `releases`), the manifest and the archive
/// (behind a redirect, as GitHub's are) with their signatures.
fn publish(
    server: &Server,
    key: &TestKey,
    version: &str,
    target: &str,
    archive_bytes: Vec<u8>,
    faults: Faults,
) {
    let other = TestKey::new();
    let tag = format!("v{version}");
    let name = format!("coqo-{version}-{target}.tar.gz");
    let sha = if faults.wrong_sha {
        sha256_hex(b"something else")
    } else {
        sha256_hex(&archive_bytes)
    };
    let size = archive_bytes.len() as u64 - if faults.smaller_size { 10 } else { 0 };
    let manifest = serde_json::to_vec(&json!({
        "version": faults.manifest_version.clone().unwrap_or_else(|| version.to_owned()),
        "protocol": "0.1",
        "assets": { target: { "file": name, "sha256": sha, "size": size } }
    }))
    .unwrap();
    let prehashed = !faults.legacy;
    let manifest_sig = if faults.manifest_signed_by_other {
        other.sign(&manifest, prehashed)
    } else {
        key.sign(&manifest, prehashed)
    };
    let archive_sig = if faults.archive_signed_by_other {
        other.sign(&archive_bytes, prehashed)
    } else {
        key.sign(&archive_bytes, prehashed)
    };
    let url = |n: &str| format!("{}{}", server.base, Server::download_path(&tag, n));
    let release = json!({
        "tag_name": tag,
        "name": format!("Coqo {version}"),
        "body": format!("What's new in {version}."),
        "draft": false,
        "prerelease": faults.prerelease,
        "html_url": format!("https://github.com/happsie/coqo/releases/tag/{tag}"),
        "published_at": "2026-11-28T12:00:00Z",
        "assets": [
            {"name": "manifest.json", "browser_download_url": url("manifest.json"), "size": manifest.len()},
            {"name": "manifest.json.minisig", "browser_download_url": url("manifest.json.minisig"), "size": 1},
            {"name": name, "browser_download_url": url(&name), "size": archive_bytes.len()},
            {"name": format!("{name}.minisig"), "browser_download_url": url(&format!("{name}.minisig")), "size": 1},
        ],
    });
    let body = serde_json::to_vec(&release).unwrap();
    server.route(
        "/repos/happsie/coqo/releases/latest",
        Route {
            etag: Some(format!("\"{version}\"")),
            ..Route::ok(body)
        },
    );
    server.route(
        "/repos/happsie/coqo/releases?per_page=20",
        Route::ok(serde_json::to_vec(&json!([release])).unwrap()),
    );
    server.route(
        &Server::download_path(&tag, "manifest.json"),
        Route::ok(manifest),
    );
    server.route(
        &Server::download_path(&tag, "manifest.json.minisig"),
        Route::ok(manifest_sig),
    );
    // GitHub redirects asset downloads to its storage.
    let stored = format!("/objects/{name}");
    server.route(
        &Server::download_path(&tag, &name),
        Route {
            status: 302,
            headers: vec![("Location".into(), format!("{}{stored}", server.base))],
            ..Route::ok("")
        },
    );
    server.route(&stored, Route::ok(archive_bytes));
    server.route(
        &Server::download_path(&tag, &format!("{name}.minisig")),
        Route::ok(archive_sig),
    );
}

// ---- installs --------------------------------------------------------------------------

fn write_exe(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// An installed `version` under `home`, for `target`'s layout.
fn installed(home: &Path, target: &str, version: &str) -> Install {
    if target == LINUX {
        let bin = home.join(".local/share/coqo/bin");
        write_exe(&bin.join("coqo"), &format!("coqo {version}"));
        write_exe(&bin.join("coqod"), &format!("coqod {version}"));
        Install::detect_at(Os::Linux, &bin.join("coqod"), home)
    } else {
        let app = home.join("Applications/Coqo.app");
        write_exe(&app.join("Contents/MacOS/coqo"), &format!("coqo {version}"));
        write_exe(
            &app.join("Contents/MacOS/coqod"),
            &format!("coqod {version}"),
        );
        fs::write(app.join("Contents/Info.plist"), "<plist/>").unwrap();
        Install::detect_at(Os::Macos, &app.join("Contents/MacOS/coqod"), home)
    }
}

fn ui_text(install: &Install) -> String {
    fs::read_to_string(install.ui_binary().unwrap()).unwrap()
}

struct Env {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    data: std::path::PathBuf,
    server: Server,
    key: TestKey,
}

impl Env {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(tmp.path()).unwrap().join("home");
        let data = home.join("data");
        fs::create_dir_all(&data).unwrap();
        Self {
            _tmp: tmp,
            home,
            data,
            server: Server::start().await,
            key: TestKey::new(),
        }
    }

    fn updater(&self, install: Install, current: &str, target: &'static str) -> Updater {
        Updater::for_tests(
            UpdaterConfig {
                current: Version::parse(current).unwrap(),
                data_dir: self.data.clone(),
                install,
            },
            self.key.trusted(),
            target,
            &self.server.base,
        )
    }
}

/// Every request carries only a `Host`, the updater's `User-Agent` and, on
/// a release poll, an `If-None-Match`.
fn assert_only_the_user_agent(log: &[Logged], version: &str) {
    assert!(!log.is_empty());
    for request in log {
        for (name, value) in &request.headers {
            match name.as_str() {
                "host" => {}
                "user-agent" => assert_eq!(value, &format!("coqo-updater/{version}")),
                "if-none-match" => assert!(request.path.contains("/releases"), "{request:?}"),
                other => panic!("{} sent the header {other}: {value}", request.path),
            }
        }
        assert!(
            request.headers.iter().any(|(k, _)| k == "user-agent"),
            "{request:?}"
        );
    }
}

// ---- tests ---------------------------------------------------------------------------------

/// Acceptance: detects an update, downloads, verifies and swaps, keeping
/// the previous version; polling again is a `304`.
#[tokio::test]
async fn detects_downloads_verifies_and_swaps_an_app() {
    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.1.0");
    let app = install.root().unwrap().to_owned();
    assert_eq!(install, Install::App { app: app.clone() });
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        TARGET,
        archive(TARGET, "0.2.0", |_| {}),
        Faults::default(),
    );
    let updater = env.updater(install.clone(), "0.1.0", TARGET);

    let offer = updater
        .check(Channel::Stable)
        .await
        .unwrap()
        .expect("an update");
    assert_eq!(offer.version, Version::parse("0.2.0").unwrap());
    let prepared = updater.prepare(&offer).await.unwrap();
    assert_eq!(prepared.notes, "What's new in 0.2.0.");
    let staged = install.staged().unwrap();
    assert_eq!(staged, app.with_file_name("Coqo.app.new"));
    assert_eq!(
        fs::read_to_string(staged.join("Contents/MacOS/coqo")).unwrap(),
        "coqo 0.2.0"
    );
    // The archive's 0666 became 0644; executables are 0755.
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&staged.join("Contents/Info.plist")), 0o644);
    assert_eq!(mode(&staged.join("Contents/MacOS/coqod")), 0o755);
    assert_eq!(ui_text(&install), "coqo 0.1.0", "nothing swapped yet");
    assert!(
        env.data
            .join("updates/0.2.0/coqo-0.2.0-aarch64-apple-darwin.tar.gz")
            .is_file()
    );

    updater.install(&prepared).unwrap();
    assert_eq!(ui_text(&install), "coqo 0.2.0");
    let old = install.previous().unwrap();
    assert_eq!(
        fs::read_to_string(old.join("Contents/MacOS/coqo")).unwrap(),
        "coqo 0.1.0"
    );
    assert!(!staged.exists());
    let pending = updater.dir().pending().unwrap();
    assert_eq!(
        (pending.from.as_str(), pending.to.as_str()),
        ("0.1.0", "0.2.0")
    );
    assert_eq!(pending.install, app);

    // Polling again sends the ETag and gets a 304, with the same answer.
    let again = updater.check(Channel::Stable).await.unwrap().unwrap();
    assert_eq!(again.version, offer.version);
    let log = env.server.log();
    let polls: Vec<_> = log
        .iter()
        .filter(|r| r.path.ends_with("/releases/latest"))
        .collect();
    assert_eq!(polls.len(), 2);
    assert!(
        polls[1]
            .headers
            .contains(&("if-none-match".into(), "\"0.2.0\"".into()))
    );
    assert!(
        log.iter().any(|r| r.path.starts_with("/objects/")),
        "followed the redirect"
    );
    assert_only_the_user_agent(&log, "0.1.0");
}

/// The same on Linux's bin folder, with legacy (non-prehashed) signatures.
#[tokio::test]
async fn detects_downloads_verifies_and_swaps_a_bin_folder() {
    let env = Env::new().await;
    let install = installed(&env.home, LINUX, "0.1.0");
    assert!(matches!(install, Install::BinDir { .. }), "{install:?}");
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        LINUX,
        archive(LINUX, "0.2.0", |_| {}),
        Faults {
            legacy: true,
            ..Faults::default()
        },
    );
    let updater = env.updater(install.clone(), "0.1.0", LINUX);
    let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
    let prepared = updater.prepare(&offer).await.unwrap();
    updater.install(&prepared).unwrap();
    assert_eq!(ui_text(&install), "coqo 0.2.0");
    let daemon = install.daemon_binary().unwrap();
    assert_eq!(fs::read_to_string(&daemon).unwrap(), "coqod 0.2.0");
    // The archive's set-uid bit is gone.
    assert_eq!(
        fs::metadata(&daemon).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert!(env.home.join(".local/share/coqo/bin.old/coqo").is_file());
    assert_only_the_user_agent(&env.server.log(), "0.1.0");
}

async fn refused(faults: Faults) -> (UpdateError, Install) {
    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.1.0");
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        TARGET,
        archive(TARGET, "0.2.0", |_| {}),
        faults,
    );
    let updater = env.updater(install.clone(), "0.1.0", TARGET);
    let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
    let e = updater.prepare(&offer).await.unwrap_err();
    // Refused: nothing staged, nothing swapped.
    assert!(!install.staged().unwrap().exists(), "{e}");
    assert_eq!(ui_text(&install), "coqo 0.1.0");
    assert!(updater.dir().pending().is_none());
    (e, install)
}

/// Acceptance: a bad signature, a sha mismatch and a downgrade are refused,
/// and say why.
#[tokio::test]
async fn refuses_bad_signatures_and_mismatches() {
    let (e, _) = refused(Faults {
        manifest_signed_by_other: true,
        ..Faults::default()
    })
    .await;
    assert!(
        matches!(&e, UpdateError::BadSignature { what } if what == "manifest.json"),
        "{e}"
    );
    assert!(
        e.to_string()
            .contains("does not verify against Coqo's release key"),
        "{e}"
    );

    let (e, _) = refused(Faults {
        archive_signed_by_other: true,
        ..Faults::default()
    })
    .await;
    assert!(
        matches!(&e, UpdateError::BadSignature { what } if what.ends_with(".tar.gz")),
        "{e}"
    );

    let (e, _) = refused(Faults {
        wrong_sha: true,
        ..Faults::default()
    })
    .await;
    assert!(
        matches!(&e, UpdateError::Mismatch { detail, .. } if detail.contains("sha256")),
        "{e}"
    );

    let (e, _) = refused(Faults {
        smaller_size: true,
        ..Faults::default()
    })
    .await;
    assert!(matches!(&e, UpdateError::Mismatch { .. }), "{e}");

    // A signed manifest of another version under this tag.
    let (e, _) = refused(Faults {
        manifest_version: Some("0.0.5".into()),
        ..Faults::default()
    })
    .await;
    assert!(e.to_string().contains("manifest.json is for v0.0.5"), "{e}");
}

#[tokio::test]
async fn refuses_downgrades_and_pre_releases_off_the_pre_channel() {
    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.2.0");
    let updater = env.updater(install.clone(), "0.2.0", TARGET);
    // No release yet: GitHub's 404.
    assert!(updater.check(Channel::Stable).await.unwrap().is_none());

    for version in ["0.1.0", "0.2.0"] {
        publish(
            &env.server,
            &env.key,
            version,
            TARGET,
            archive(TARGET, version, |_| {}),
            Faults::default(),
        );
        assert!(
            updater.check(Channel::Stable).await.unwrap().is_none(),
            "{version}"
        );
        assert!(
            updater.check(Channel::Pre).await.unwrap().is_none(),
            "{version}"
        );
    }
    // Asked to prepare a downgrade anyway, it refuses.
    publish(
        &env.server,
        &env.key,
        "0.1.0",
        TARGET,
        archive(TARGET, "0.1.0", |_| {}),
        Faults::default(),
    );
    let raw: crate::release::GithubRelease = serde_json::from_slice(
        &env.server.routes.lock().unwrap()["/repos/happsie/coqo/releases/latest"].body,
    )
    .unwrap();
    let e = updater
        .prepare(&Offer {
            version: Version::parse("0.1.0").unwrap(),
            release: raw,
        })
        .await
        .unwrap_err();
    assert!(matches!(e, UpdateError::NotNewer { .. }), "{e}");
    assert!(!install.staged().unwrap().exists());

    // A pre-release only on the pre channel.
    publish(
        &env.server,
        &env.key,
        "0.3.0-rc.1",
        TARGET,
        archive(TARGET, "0.3.0-rc.1", |_| {}),
        Faults {
            prerelease: true,
            ..Faults::default()
        },
    );
    assert!(updater.check(Channel::Stable).await.unwrap().is_none());
    let offer = updater.check(Channel::Pre).await.unwrap().unwrap();
    assert_eq!(offer.version.to_string(), "0.3.0-rc.1");
    updater.prepare(&offer).await.unwrap();
    assert_only_the_user_agent(&env.server.log(), "0.2.0");
}

/// Acceptance: rolling back restores the previous version, once.
#[tokio::test]
async fn rolls_back_once() {
    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.1.0");
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        TARGET,
        archive(TARGET, "0.2.0", |_| {}),
        Faults::default(),
    );
    let updater = env.updater(install.clone(), "0.1.0", TARGET);
    let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
    updater
        .install(&updater.prepare(&offer).await.unwrap())
        .unwrap();
    assert_eq!(ui_text(&install), "coqo 0.2.0");

    install::roll_back(&install).unwrap();
    assert_eq!(ui_text(&install), "coqo 0.1.0");
    assert!(!install.previous().unwrap().exists());
    let e = install::roll_back(&install).unwrap_err();
    assert!(e.to_string().contains("no previous version"), "{e}");
    assert_eq!(ui_text(&install), "coqo 0.1.0");
}

/// Acceptance: when the new version's daemon exits during start-up, the
/// launcher goes back to the previous version once and says why.
#[tokio::test]
async fn a_new_daemon_that_does_not_start_rolls_back_once_with_its_reason() {
    use crate::records::roll_back_failed_start;

    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.1.0");
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        TARGET,
        archive(TARGET, "0.2.0", |_| {}),
        Faults::default(),
    );
    let updater = env.updater(install.clone(), "0.1.0", TARGET);
    let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
    updater
        .install(&updater.prepare(&offer).await.unwrap())
        .unwrap();
    let dir = updater.dir();

    // Another version's failure (the old daemon, a dev build) is not ours.
    assert_eq!(
        roll_back_failed_start(dir, &install, "0.1.0", "x").unwrap(),
        None
    );
    let elsewhere = Install::App {
        app: env.home.join("Other/Coqo.app"),
    };
    assert_eq!(
        roll_back_failed_start(dir, &elsewhere, "0.2.0", "x").unwrap(),
        None
    );
    assert_eq!(ui_text(&install), "coqo 0.2.0");

    let reason = "cannot open coqo.db: database disk image is malformed";
    let record = roll_back_failed_start(dir, &install, "0.2.0", reason)
        .unwrap()
        .expect("rolled back");
    assert_eq!(
        (
            record.version.as_str(),
            record.restored.as_str(),
            record.reason.as_str()
        ),
        ("0.2.0", "0.1.0", reason)
    );
    assert_eq!(ui_text(&install), "coqo 0.1.0");
    assert_eq!(dir.rolled_back(), Some(record));
    assert_eq!(dir.pending(), None);
    // Once: failing again does nothing.
    assert_eq!(
        roll_back_failed_start(dir, &install, "0.2.0", reason).unwrap(),
        None
    );
    assert_eq!(ui_text(&install), "coqo 0.1.0");
}

/// Acceptance: a development build never installs, and is told so.
#[test]
fn a_development_build_is_detected_and_never_installs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let checkout = home.join("code/coqo/target/debug/coqod");
    write_exe(&checkout, "dev");
    let release = home.join("code/coqo/target/release/coqo");
    write_exe(&release, "dev");
    for os in [Os::Macos, Os::Linux] {
        assert!(Install::detect_at(os, &release, &home).is_dev(), "{os:?}");
        let install = Install::detect_at(os, &checkout, &home);
        assert!(install.is_dev(), "{os:?}: {install:?}");
        assert!(install.staged().is_none() && install.ui_binary().is_none());
        assert!(matches!(
            install::swap(&install),
            Err(UpdateError::DevBuild)
        ));
        assert!(matches!(
            install::roll_back(&install),
            Err(UpdateError::DevBuild)
        ));
        assert!(matches!(
            install::unpack(&install, &home.join("x.tar.gz")),
            Err(UpdateError::DevBuild)
        ));
        let e = Updater::new(UpdaterConfig {
            current: Version::parse("0.1.0").unwrap(),
            data_dir: home.join("data"),
            install,
        })
        .unwrap_err();
        assert!(matches!(e, UpdateError::DevBuild), "{e}");
        assert_eq!(
            e.to_string(),
            "running a development build: updates install only into an installed Coqo"
        );
    }
    // A bundle of another name, or one outside an Applications folder, is
    // not an install either; nor is a Linux layout on macOS.
    let elsewhere = home.join("Downloads/Coqo.app/Contents/MacOS/coqo");
    write_exe(&elsewhere, "x");
    assert!(Install::detect_at(Os::Macos, &elsewhere, &home).is_dev());
    let renamed = home.join("Applications/Coqo 2.app/Contents/MacOS/coqo");
    write_exe(&renamed, "x");
    assert!(Install::detect_at(Os::Macos, &renamed, &home).is_dev());
    let linux = home.join(".local/share/coqo/bin/coqo");
    write_exe(&linux, "x");
    assert!(Install::detect_at(Os::Macos, &linux, &home).is_dev());
    assert!(!Install::detect_at(Os::Linux, &linux, &home).is_dev());
}

/// Linux: `~/.local/bin/coqo` is a symlink into the install (as
/// `scripts/install.sh` makes it); started through it, Coqo is the install,
/// and after a swap the same symlink starts the new version. A symlink to a
/// checkout's `target/release` is still a development build.
#[test]
fn a_linux_symlink_in_local_bin_resolves_to_the_install() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let install = installed(&home, LINUX, "0.1.0");
    let bin = home.join(".local/share/coqo/bin");
    assert_eq!(install, Install::BinDir { bin: bin.clone() });
    let local_bin = home.join(".local/bin");
    fs::create_dir_all(&local_bin).unwrap();
    for name in ["coqo", "coqod"] {
        symlink(bin.join(name), local_bin.join(name)).unwrap();
    }
    for name in ["coqo", "coqod"] {
        assert_eq!(
            Install::detect_at(Os::Linux, &local_bin.join(name), &home),
            install,
            "{name}"
        );
    }
    // The swap renames the folder, so the symlinks now reach the new one.
    let staged = install.staged().unwrap();
    fs::create_dir_all(&staged).unwrap();
    write_exe(&staged.join("coqo"), "coqo 0.2.0");
    write_exe(&staged.join("coqod"), "coqod 0.2.0");
    install::swap(&install).unwrap();
    assert_eq!(
        fs::read_to_string(local_bin.join("coqo")).unwrap(),
        "coqo 0.2.0"
    );
    assert!(install.previous().unwrap().join("coqod").is_file());
    // A symlink to a checkout's build is not an install.
    let release = home.join("code/coqo/target/release/coqo");
    write_exe(&release, "dev");
    let dev = home.join(".local/bin/coqo-dev");
    symlink(&release, &dev).unwrap();
    assert!(Install::detect_at(Os::Linux, &dev, &home).is_dev());
}

/// This repository's key file is the placeholder until the maintainer
/// commits the real key; with it, no update is ever verified.
#[test]
fn without_a_release_key_nothing_updates() {
    if TrustedKey::has_release_key() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap();
    let install = installed(&home, LINUX, "0.1.0");
    let e = Updater::new(UpdaterConfig {
        current: Version::parse("0.1.0").unwrap(),
        data_dir: home.join("data"),
        install,
    })
    .unwrap_err();
    assert!(matches!(e, UpdateError::NoReleaseKey), "{e}");
}

/// An archive with a link, a path out of the folder, an absolute path, a
/// file beside the app or a file twice is refused before anything is
/// staged.
#[tokio::test]
async fn unsafe_archives_are_refused() {
    let symlink = |b: &mut tar::Builder<GzEncoder<Vec<u8>>>| {
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "./Coqo.app/Contents/link", "/etc/passwd")
            .unwrap();
    };
    let raw_name = |name: &'static [u8]| {
        move |b: &mut tar::Builder<GzEncoder<Vec<u8>>>| {
            let mut h = tar::Header::new_old();
            h.as_old_mut().name[..name.len()].copy_from_slice(name);
            h.set_size(4);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append(&h, &b"evil"[..]).unwrap();
        }
    };
    let beside = |b: &mut tar::Builder<GzEncoder<Vec<u8>>>| add_file(b, "Other.app/x", 0o644, b"x");
    let twice = |b: &mut tar::Builder<GzEncoder<Vec<u8>>>| {
        add_file(b, "Coqo.app/Contents/MacOS/coqo", 0o755, b"again");
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("a Symlink", archive(TARGET, "0.2.0", symlink)),
        (
            "leaves the install folder",
            archive(TARGET, "0.2.0", raw_name(b"Coqo.app/../../evil")),
        ),
        (
            "leaves the install folder",
            archive(TARGET, "0.2.0", raw_name(b"/tmp/evil")),
        ),
        ("is outside Coqo.app", archive(TARGET, "0.2.0", beside)),
        ("is in it twice", archive(TARGET, "0.2.0", twice)),
    ];
    for (why, bytes) in cases {
        let env = Env::new().await;
        let install = installed(&env.home, TARGET, "0.1.0");
        publish(
            &env.server,
            &env.key,
            "0.2.0",
            TARGET,
            bytes,
            Faults::default(),
        );
        let updater = env.updater(install.clone(), "0.1.0", TARGET);
        let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
        let e = updater.prepare(&offer).await.unwrap_err();
        assert!(matches!(e, UpdateError::UnsafeArchive(_)), "{why}: {e}");
        assert!(e.to_string().contains(why), "{why}: {e}");
        assert!(!install.staged().unwrap().exists());
        assert!(!env.home.join("Applications/Coqo.app.staging").exists());
        assert!(!env.home.join("evil").exists());
        assert_eq!(ui_text(&install), "coqo 0.1.0");
    }
}

/// Asset URLs must be this repository's release downloads.
#[tokio::test]
async fn assets_come_only_from_the_repositorys_releases() {
    let env = Env::new().await;
    let install = installed(&env.home, TARGET, "0.1.0");
    publish(
        &env.server,
        &env.key,
        "0.2.0",
        TARGET,
        archive(TARGET, "0.2.0", |_| {}),
        Faults::default(),
    );
    let path = "/repos/happsie/coqo/releases/latest";
    let mut release: serde_json::Value =
        serde_json::from_slice(&env.server.routes.lock().unwrap()[path].body).unwrap();
    release["assets"][0]["browser_download_url"] = json!("https://example.com/manifest.json");
    env.server
        .route(path, Route::ok(serde_json::to_vec(&release).unwrap()));
    let updater = env.updater(install, "0.1.0", TARGET);
    let offer = updater.check(Channel::Stable).await.unwrap().unwrap();
    let e = updater.prepare(&offer).await.unwrap_err();
    assert!(
        e.to_string()
            .contains("not a happsie/coqo release download"),
        "{e}"
    );
}
