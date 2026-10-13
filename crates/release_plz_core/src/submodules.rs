//! Submodules of a release reconstructed in a worktree, where git leaves every
//! submodule directory empty.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command, Output};

use anyhow::Context;
use cargo_metadata::camino::Utf8Path;
use tracing::{debug, warn};

/// A submodule as a `.gitmodules` file declares it, keyed by its name.
struct Declared {
    path: String,
    url: Option<String>,
}

/// A submodule of the release to check out, with the URL to fetch it from.
struct Selected<'a> {
    name: &'a str,
    path: &'a str,
    url: String,
}

/// Check out in `release`, a worktree of a release commit, the submodules the
/// working copy at `working_copy` reads, at the commits the release records.
///
/// Submodules are matched by name, so one moved since the release is found.
/// Those checked out in the working copy are required. Those the working copy
/// no longer declares are tried, as only the release knows whether it needs
/// them: failing to fetch one only warns, since a release that needs it fails
/// later with Cargo's error anyway. Those the working copy declares but leaves
/// uninitialized stay so. Nested submodules follow the same rules against the
/// working copy's nested state. No git configuration is written: URLs are
/// passed to each command.
pub(crate) fn check_out(release: &Utf8Path, working_copy: &Utf8Path) -> anyhow::Result<()> {
    check_out_level(release, Some(working_copy))
}

/// One level of [`check_out`]: `working_copy` is the counterpart of `release`
/// in the working copy, if the working copy has it checked out.
fn check_out_level(release: &Utf8Path, working_copy: Option<&Utf8Path>) -> anyhow::Result<()> {
    let declared = declared(&release.join(".gitmodules"))?;
    if declared.is_empty() {
        return Ok(());
    }
    let gitlinks = gitlinks(release)?;
    let current = match working_copy {
        Some(dir) => declared_with_config(dir)?,
        None => BTreeMap::new(),
    };
    // Relative URLs resolve against the superproject's remote, as git does.
    let base = remote_url(working_copy.unwrap_or(release))?;

    let mut required = Vec::new();
    let mut removed = Vec::new();
    for (name, submodule) in &declared {
        // A `.gitmodules` entry without a gitlink is not a submodule.
        if !gitlinks.contains(&submodule.path) {
            continue;
        }
        let Some(released_url) = &submodule.url else {
            warn!("submodule {name} of the release has no URL; it is not checked out");
            continue;
        };
        match current.get(name.as_str()) {
            Some((current, _)) if !is_checked_out(working_copy, &current.path) => {}
            Some((current, configured)) => {
                // The working copy's URL can be a mirror or carry credentials: keep
                // it unless the release records another repository.
                let url = match configured {
                    Some(url) if current.url.as_ref() == Some(released_url) => url.clone(),
                    _ => resolve_url(&base, released_url)?,
                };
                required.push(Selected {
                    name,
                    path: &submodule.path,
                    url,
                });
            }
            None => removed.push(Selected {
                name,
                path: &submodule.path,
                url: resolve_url(&base, released_url)?,
            }),
        }
    }

    update(release, &required).context("check out the submodules of the release")?;
    if let Err(error) = update(release, &removed) {
        warn!("cannot check out submodules of the release removed since: {error:?}");
    }

    for submodule in &required {
        let nested_working_copy = working_copy.and_then(|dir| {
            let (current, _) = current.get(submodule.name)?;
            Some(dir.join(&current.path))
        });
        check_out_level(
            &release.join(submodule.path),
            nested_working_copy.as_deref(),
        )?;
    }
    for submodule in &removed {
        let dir = release.join(submodule.path);
        if dir.join(".git").exists() {
            check_out_level(&dir, None)?;
        }
    }
    Ok(())
}

fn is_checked_out(working_copy: Option<&Utf8Path>, path: &str) -> bool {
    working_copy.is_some_and(|dir| dir.join(path).join(".git").exists())
}

/// Check out `submodules` in `repo` at the commits it records, whatever update
/// procedure is configured for them.
fn update(repo: &Utf8Path, submodules: &[Selected<'_>]) -> anyhow::Result<()> {
    if submodules.is_empty() {
        return Ok(());
    }
    debug!(
        "checking out submodules {:?} in {repo}",
        submodules.iter().map(|s| s.path).collect::<Vec<_>>()
    );
    let mut command = git(repo);
    // Paths are literal, not patterns.
    command.arg("--literal-pathspecs");
    for submodule in submodules {
        // Without `--init`, git reads the URL and the active state from the
        // configuration instead of writing them into it.
        command
            .arg("-c")
            .arg(format!(
                "submodule.{}.url={}",
                submodule.name, submodule.url
            ))
            .arg("-c")
            .arg(format!("submodule.{}.active=true", submodule.name));
    }
    command.args(["submodule", "update", "--checkout", "--"]);
    command.args(submodules.iter().map(|s| s.path));
    run(command).map(drop)
}

/// The submodules a `.gitmodules` file declares, empty if there is none.
fn declared(gitmodules: &Utf8Path) -> anyhow::Result<BTreeMap<String, Declared>> {
    if !gitmodules.exists() {
        return Ok(BTreeMap::new());
    }
    let mut command = Command::new("git");
    command
        .args(["config", "-z", "-f"])
        .arg(gitmodules)
        .args(["--get-regexp", r"^submodule\..*\.(path|url)$"]);
    let Some(entries) = run_optional(command)? else {
        return Ok(BTreeMap::new());
    };
    let mut declared = BTreeMap::new();
    let mut urls = BTreeMap::new();
    for entry in entries.split_terminator('\0') {
        let (key, value) = entry
            .split_once('\n')
            .with_context(|| format!("invalid entry {entry:?} in {gitmodules}"))?;
        let key = key
            .strip_prefix("submodule.")
            .with_context(|| format!("invalid key {key:?} in {gitmodules}"))?;
        if let Some(name) = key.strip_suffix(".path") {
            declared.insert(
                name.to_owned(),
                Declared {
                    path: value.to_owned(),
                    url: None,
                },
            );
        } else if let Some(name) = key.strip_suffix(".url") {
            urls.insert(name.to_owned(), value.to_owned());
        }
    }
    for (name, url) in urls {
        if let Some(submodule) = declared.get_mut(&name) {
            submodule.url = Some(url);
        }
    }
    Ok(declared)
}

/// The submodules the repository at `dir` declares, each with the URL its
/// configuration holds for it.
fn declared_with_config(
    dir: &Utf8Path,
) -> anyhow::Result<BTreeMap<String, (Declared, Option<String>)>> {
    declared(&dir.join(".gitmodules"))?
        .into_iter()
        .map(|(name, submodule)| {
            let configured = config(dir, &format!("submodule.{name}.url"))?;
            Ok((name, (submodule, configured)))
        })
        .collect()
}

/// Paths of the gitlinks of the index of `repo`.
fn gitlinks(repo: &Utf8Path) -> anyhow::Result<BTreeSet<String>> {
    let mut command = git(repo);
    command.args(["ls-files", "--stage", "-z"]);
    let index = run(command)?;
    Ok(index
        .split_terminator('\0')
        .filter_map(|entry| {
            let (metadata, path) = entry.split_once('\t')?;
            metadata.starts_with("160000 ").then(|| path.to_owned())
        })
        .collect())
}

/// The URL of the remote git resolves relative submodule URLs against: the
/// current branch's remote, `origin` otherwise, or the repository itself
/// when that remote has no URL.
fn remote_url(repo: &Utf8Path) -> anyhow::Result<String> {
    let mut command = git(repo);
    command.args(["symbolic-ref", "-q", "--short", "HEAD"]);
    let branch_remote = match run_optional(command)? {
        Some(branch) => config(repo, &format!("branch.{}.remote", branch.trim_end()))?,
        None => None,
    };
    let remote = branch_remote.unwrap_or_else(|| "origin".to_owned());
    Ok(config(repo, &format!("remote.{remote}.url"))?.unwrap_or_else(|| repo.to_string()))
}

/// Resolve a submodule URL against the superproject's remote URL, as git
/// does: a URL starting with `./` or `../` is relative to `remote`, any other
/// is used as is.
fn resolve_url(remote: &str, url: &str) -> anyhow::Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return Ok(url.to_owned());
    }
    let mut base = remote.trim_end_matches('/').to_owned();
    let is_relative = is_local_not_ssh(&base) && !Utf8Path::new(&base).is_absolute();
    if is_relative && !base.starts_with("./") && !base.starts_with("../") {
        base.insert_str(0, "./");
    }
    let mut rest = url;
    let mut colon_separated = false;
    loop {
        if let Some(stripped) = rest.strip_prefix("../") {
            rest = stripped;
            if let Some(slash) = base.rfind('/') {
                base.truncate(slash);
            } else if let Some(colon) = base.rfind(':') {
                base.truncate(colon);
                colon_separated = true;
            } else if is_relative || base == "." {
                anyhow::bail!("cannot strip one component off url {base:?}");
            } else {
                base = ".".to_owned();
            }
        } else if let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
        } else {
            break;
        }
    }
    let separator = if colon_separated { ":" } else { "/" };
    let resolved = format!("{base}{separator}{}", rest.trim_end_matches('/'));
    Ok(resolved
        .strip_prefix("./")
        .map_or_else(|| resolved.clone(), str::to_owned))
}

/// Whether `url` is a local path rather than a URL or an scp-like address.
fn is_local_not_ssh(url: &str) -> bool {
    match url.find(':') {
        None => true,
        Some(colon) => url.find('/').is_some_and(|slash| slash < colon),
    }
}

/// The value of `key` in the configuration of `repo`, if set.
fn config(repo: &Utf8Path, key: &str) -> anyhow::Result<Option<String>> {
    let mut command = git(repo);
    command.args(["config", "--get", key]);
    Ok(run_optional(command)?.map(|value| value.trim_end_matches('\n').to_owned()))
}

fn git(repo: &Utf8Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command
}

/// Run `command`, failing on any non-zero exit.
fn run(command: Command) -> anyhow::Result<String> {
    let (output, description) = output(command)?;
    anyhow::ensure!(
        output.status.success(),
        "`{description}` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).with_context(|| format!("non-UTF-8 output of `{description}`"))
}

/// Run `command`, whose exit code 1 means nothing was found.
fn run_optional(command: Command) -> anyhow::Result<Option<String>> {
    let (output, description) = output(command)?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(Some)
            .with_context(|| format!("non-UTF-8 output of `{description}`")),
        Some(1) => Ok(None),
        _ => anyhow::bail!(
            "`{description}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn output(mut command: Command) -> anyhow::Result<(Output, String)> {
    let description = format!("{command:?}");
    let output = command
        .output()
        .with_context(|| format!("cannot run `{description}`"))?;
    Ok((output, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls_resolve_as_git_resolves_them() {
        for (remote, url, expected) in [
            (
                "https://example.com/group/app.git",
                "../dep.git",
                "https://example.com/group/dep.git",
            ),
            (
                "https://example.com/group/app.git/",
                "../dep.git",
                "https://example.com/group/dep.git",
            ),
            (
                "https://example.com/group/app",
                "../../other/dep",
                "https://example.com/other/dep",
            ),
            (
                "https://example.com/group/app",
                "./dep",
                "https://example.com/group/app/dep",
            ),
            (
                "git@example.com:group/app.git",
                "../dep.git",
                "git@example.com:group/dep.git",
            ),
            (
                "git@example.com:app.git",
                "../dep.git",
                "git@example.com:dep.git",
            ),
            ("/srv/git/app", "../dep", "/srv/git/dep"),
            ("repos/app", "../dep", "repos/dep"),
            (
                "https://example.com/group/app",
                "https://other.com/dep",
                "https://other.com/dep",
            ),
            ("https://example.com/group/app", "/srv/dep", "/srv/dep"),
        ] {
            assert_eq!(
                resolve_url(remote, url).unwrap(),
                expected,
                "{remote} + {url}"
            );
        }
    }

    #[test]
    fn a_relative_url_cannot_climb_above_a_relative_remote() {
        assert!(resolve_url("app", "../../dep").is_err());
    }
}
