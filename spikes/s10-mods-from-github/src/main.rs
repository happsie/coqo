//! Spike S10 (#355): resolve a mod on GitHub to a pinned commit, validate
//! what is there, and print a dry-run install plan. It never executes,
//! builds or installs anything it downloads.
//!
//! ```sh
//! cargo run -- plan owner/repo[@ref][#subdir] [--pick <name>] [--releases]
//! cargo run -- local <folder> [#subdir] [--pick <name>]
//! ```

mod claude;
mod coqo;
mod plan;
mod repo;
mod source;

#[cfg(test)]
mod tests;

use std::path::Path;

use repo::{GitHub, Local};
use source::{Source, is_sha};

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pick = flag(&args, "--pick");
    let releases = args.iter().any(|a| a == "--releases");
    let result = match args.first().map(String::as_str) {
        Some("plan") => args
            .get(1)
            .ok_or_else(|| "usage: plan owner/repo[@ref][#subdir]".to_owned())
            .and_then(|spec| plan_github(spec, pick.as_deref(), releases)),
        Some("local") => args
            .get(1)
            .ok_or_else(|| "usage: local <folder> [#subdir]".to_owned())
            .and_then(|dir| {
                let sub = args
                    .get(2)
                    .and_then(|s| s.strip_prefix('#'))
                    .map(source::clean_subdir)
                    .transpose()?
                    .flatten()
                    .unwrap_or_default();
                let repo = Local::open(Path::new(dir))?;
                let p = plan::plan(&repo, &sub, pick.as_deref(), "local");
                Ok(plan::render(&repo, &sub, &p, &[]))
            }),
        _ => Err("usage: plan <owner/repo[@ref][#subdir]> | local <folder> [#subdir]".into()),
    };
    match result {
        Ok(text) => print!("{text}"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn plan_github(spec: &str, pick: Option<&str>, releases: bool) -> Result<String, String> {
    let source = Source::parse(spec)?;
    let repo = GitHub::open(&source.owner, &source.repo, source.reference.as_deref())?;
    let dir = source.subdir.clone().unwrap_or_default();
    let p = plan::plan(&repo, &dir, pick, &source.repo);

    let mut extra = Vec::new();
    extra.push(match &source.reference {
        None => "updates: no ref given, so Coqo would track the latest release tag if the repo has \
                 releases, else the default branch's head"
            .into(),
        Some(r) if is_sha(r) => format!("updates: pinned to commit {r}; never updated automatically"),
        Some(r) => format!(
            "updates: follows `{r}`: a newer semver tag (for a tag) or a new head commit (for a branch) \
             is offered, never applied without approval"
        ),
    });
    if releases {
        match repo.latest_release()? {
            Some(r) => extra.push(format!(
                "latest release {} with {} assets{}",
                r.tag_name,
                r.assets.len(),
                r.assets
                    .iter()
                    .take(4)
                    .map(|a| format!(" {} ({} B)", a.name, a.size))
                    .collect::<String>()
            )),
            None => extra.push("no releases".into()),
        }
    }
    Ok(format!(
        "{}full sha    {}\n",
        plan::render(&repo, &dir, &p, &extra),
        repo.sha
    ))
}
