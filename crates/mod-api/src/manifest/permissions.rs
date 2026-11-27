// SPDX-License-Identifier: Apache-2.0

//! The `[permissions]` table: what a mod may touch outside its own sandbox.

use serde::{Deserialize, Serialize};

/// What a mod asks to be allowed to do. Shown to the user on install; the
/// WASM host enforces it through WASI capabilities, and process and core
/// mods are trusted to keep to it (PLAN §9).
///
/// ```toml
/// [permissions]
/// fs.read = ["$workspace"]
/// fs.write = ["$workspace/target"]
/// network = ["api.github.com", "*.githubusercontent.com:443"]
/// process = ["git", "mvn"]
/// secrets = ["github.token"]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    /// File system access.
    #[serde(default, skip_serializing_if = "FsPermissions::is_empty")]
    pub fs: FsPermissions,
    /// Hosts the mod may connect to: `host`, `host:port`, `*.domain` for any
    /// subdomain, or `*` for any host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network: Vec<String>,
    /// Programs the mod may spawn, or `*` for any.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process: Vec<String>,
    /// Names of keychain secrets the mod may read, such as `github.token`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

impl Permissions {
    /// Whether the mod asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.fs.is_empty()
            && self.network.is_empty()
            && self.process.is_empty()
            && self.secrets.is_empty()
    }
}

/// File system scopes. Each entry starts with a location variable and may
/// add a sub-path: `$workspace`, `$workspace/target`, `$data/cache`.
///
/// | Variable | Location |
/// |---|---|
/// | `$workspace` | the open workspace's root |
/// | `$mod` | the mod's own install folder |
/// | `$data` | the mod's private data folder |
/// | `$cache` | the mod's private cache folder |
/// | `$home` | the user's home folder |
/// | `$tmp` | the system temp folder |
///
/// Absolute paths are allowed too. `..` is not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsPermissions {
    /// Paths the mod may read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// Paths the mod may read and write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
}

impl FsPermissions {
    /// Whether no paths are listed.
    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }
}

/// Location variables an fs scope may start with.
pub(crate) const FS_VARIABLES: &[&str] =
    &["$workspace", "$mod", "$data", "$cache", "$home", "$tmp"];

/// Checks one fs scope entry.
pub(crate) fn check_fs_scope(scope: &str) -> Result<(), String> {
    if scope.is_empty() {
        return Err("path is empty".into());
    }
    if scope.contains('\\') {
        return Err(format!("`{scope}`: use `/` as the path separator"));
    }
    if scope.split('/').any(|seg| seg == "..") {
        return Err(format!("`{scope}`: `..` is not allowed"));
    }
    if let Some(rest) = scope.strip_prefix('$') {
        let var = rest.split('/').next().unwrap_or_default();
        if !FS_VARIABLES.contains(&format!("${var}").as_str()) {
            return Err(format!(
                "unknown location variable `${var}`; expected one of {}",
                FS_VARIABLES.join(", ")
            ));
        }
        return Ok(());
    }
    if scope.starts_with('/') {
        return Ok(());
    }
    Err(format!(
        "`{scope}` is relative; start it with a location variable such as `$workspace`, or use an absolute path"
    ))
}

/// Checks one network host pattern.
pub(crate) fn check_host(pattern: &str) -> Result<(), String> {
    if pattern == "*" {
        return Ok(());
    }
    if pattern.contains("://") {
        return Err(format!(
            "`{pattern}` is a URL; write just the host, such as `api.github.com`"
        ));
    }
    let (host, port) = match pattern.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (pattern, None),
    };
    if let Some(port) = port
        && !matches!(port.parse::<u16>(), Ok(p) if p > 0)
    {
        return Err(format!(
            "`{pattern}`: port `{port}` is not a number from 1 to 65535"
        ));
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    if host.is_empty() || !host.split('.').all(label_ok) {
        return Err(format!(
            "`{pattern}` is not a host name; use lowercase labels separated by dots, optionally `*.` in front and `:port` after"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_scopes() {
        for ok in ["$workspace", "$workspace/target", "$data/x", "/opt/jdk"] {
            check_fs_scope(ok).unwrap();
        }
        assert!(
            check_fs_scope("$workpsace")
                .unwrap_err()
                .contains("unknown location variable `$workpsace`")
        );
        assert!(check_fs_scope("src").unwrap_err().contains("relative"));
        assert!(
            check_fs_scope("$workspace/../x")
                .unwrap_err()
                .contains("..")
        );
        assert!(check_fs_scope("").is_err());
    }

    #[test]
    fn hosts() {
        for ok in ["*", "api.github.com", "*.github.com", "localhost:8080"] {
            check_host(ok).unwrap();
        }
        assert!(
            check_host("https://api.github.com")
                .unwrap_err()
                .contains("URL")
        );
        assert!(check_host("api.github.com:0").unwrap_err().contains("port"));
        assert!(
            check_host("API.github.com")
                .unwrap_err()
                .contains("host name")
        );
        assert!(check_host("a..b").is_err());
    }
}
