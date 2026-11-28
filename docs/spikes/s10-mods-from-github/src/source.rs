//! Parsing what the user types: `owner/repo[@ref][#subdir]`, with an
//! optional `github:` prefix, or a github.com URL (`.../tree/<ref>/<subdir>`).

use std::fmt;

/// A GitHub source as the user wrote it, before resolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub owner: String,
    pub repo: String,
    /// A tag, a branch or a commit SHA. `None` means the default branch.
    pub reference: Option<String>,
    /// A folder inside the repository, without leading or trailing `/`.
    pub subdir: Option<String>,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)?;
        if let Some(r) = &self.reference {
            write!(f, "@{r}")?;
        }
        if let Some(s) = &self.subdir {
            write!(f, "#{s}")?;
        }
        Ok(())
    }
}

/// Whether `s` looks like a full or abbreviated commit SHA.
pub fn is_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_ref(s: &str) -> bool {
    // A conservative subset of git's ref-name rules: enough for tags such
    // as `v1.2.0` and branches such as `release/1.x`.
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('-')
        && !s.contains("..")
        && !s.ends_with('/')
        && !s.ends_with(".lock")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// A sub-path inside the repository: relative, no `..`, no empty segments.
pub fn clean_subdir(s: &str) -> Result<Option<String>, String> {
    let s = s.trim_matches('/');
    if s.is_empty() || s == "." {
        return Ok(None);
    }
    let s = s.strip_prefix("./").unwrap_or(s);
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains('\\') {
            return Err(format!("`{s}` is not a folder inside the repository"));
        }
    }
    Ok(Some(s.to_owned()))
}

impl Source {
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        if let Some(rest) = input
            .strip_prefix("https://github.com/")
            .or_else(|| input.strip_prefix("github.com/"))
        {
            return Self::parse_url(rest);
        }
        let rest = input.strip_prefix("github:").unwrap_or(input);
        let (rest, subdir) = match rest.split_once('#') {
            Some((r, s)) => (r, clean_subdir(s)?),
            None => (rest, None),
        };
        let (path, reference) = match rest.split_once('@') {
            Some((p, r)) => (p, Some(r.to_owned())),
            None => (rest, None),
        };
        let (owner, repo) = path
            .split_once('/')
            .ok_or_else(|| format!("`{input}` is not owner/repo"))?;
        Self::new(owner, repo, reference, subdir)
    }

    fn parse_url(rest: &str) -> Result<Self, String> {
        let rest = rest.trim_end_matches('/');
        let mut parts = rest.splitn(3, '/');
        let owner = parts.next().unwrap_or_default();
        let repo = parts.next().unwrap_or_default();
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        match parts.next() {
            None => Self::new(owner, repo, None, None),
            Some(tail) => {
                // `tree/<ref>/<subdir>`: a ref with `/` in it is ambiguous
                // here, so the first segment is taken as the ref.
                let tail = tail
                    .strip_prefix("tree/")
                    .ok_or_else(|| format!("unsupported GitHub URL: {rest}"))?;
                let (reference, subdir) = match tail.split_once('/') {
                    Some((r, s)) => (r, clean_subdir(s)?),
                    None => (tail, None),
                };
                Self::new(owner, repo, Some(reference.to_owned()), subdir)
            }
        }
    }

    fn new(
        owner: &str,
        repo: &str,
        reference: Option<String>,
        subdir: Option<String>,
    ) -> Result<Self, String> {
        if !valid_name(owner) || !valid_name(repo) {
            return Err(format!("`{owner}/{repo}` is not a GitHub owner/repo"));
        }
        if let Some(r) = &reference {
            if !valid_ref(r) {
                return Err(format!("`{r}` is not a tag, branch or commit"));
            }
        }
        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
            reference,
            subdir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Source {
        Source::parse(s).unwrap()
    }

    #[test]
    fn short_forms() {
        let s = p("acme/mods");
        assert_eq!((s.owner.as_str(), s.repo.as_str()), ("acme", "mods"));
        assert_eq!((s.reference, s.subdir), (None, None));

        let s = p("github:acme/mods@v1.2.0#themes/night/");
        assert_eq!(s.reference.as_deref(), Some("v1.2.0"));
        assert_eq!(s.subdir.as_deref(), Some("themes/night"));
        assert_eq!(s.to_string(), "acme/mods@v1.2.0#themes/night");

        let s = p("acme/mods#./plugins/x");
        assert_eq!(s.subdir.as_deref(), Some("plugins/x"));
    }

    #[test]
    fn urls() {
        let s = p("https://github.com/acme/mods.git");
        assert_eq!(s.repo, "mods");
        let s = p("https://github.com/acme/mods/tree/main/plugins/lint");
        assert_eq!(s.reference.as_deref(), Some("main"));
        assert_eq!(s.subdir.as_deref(), Some("plugins/lint"));
        assert!(Source::parse("https://github.com/acme/mods/blob/main/x").is_err());
    }

    #[test]
    fn rejects_escapes_and_junk() {
        for bad in [
            "acme",
            "acme/mods#../etc",
            "acme/mods#a//b",
            "acme/mods@-x",
            "acme/mods@a..b",
            "ac me/mods",
            "../mods",
            "acme/..",
        ] {
            assert!(Source::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn shas() {
        assert!(is_sha("0fbfda5"));
        assert!(is_sha("0fbfda5a91f4adc311daca25acc99132f40f0b6e"));
        assert!(!is_sha("v1.2.0"));
        assert!(!is_sha("abc"));
    }
}
