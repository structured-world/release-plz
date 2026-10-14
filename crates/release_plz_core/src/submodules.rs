//! Submodules of a release reconstructed in a worktree, where git leaves every
//! submodule directory empty.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use anyhow::Context;
use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use tracing::{debug, warn};

/// A submodule as a `.gitmodules` file declares it, keyed by its name.
struct Declared {
    path: String,
    url: Option<String>,
}

/// Where a submodule of the release can be fetched from.
enum Source {
    /// The working copy's checkout of the same repository: its history is
    /// already there, so nothing is fetched over the network.
    WorkingCopy(Utf8PathBuf),
    /// A repository URL.
    Url(String),
}

impl Source {
    fn describe(&self) -> &'static str {
        match self {
            Self::WorkingCopy(_) => "the working copy",
            Self::Url(_) => "its URL",
        }
    }
}

/// A submodule of the release to check out.
struct Selected<'a> {
    name: &'a str,
    path: &'a str,
    /// Tried in order until one has the commit the release records.
    sources: Vec<Source>,
    /// The working copy's checkout of the same repository, which decides
    /// the nested submodules.
    counterpart: Option<Utf8PathBuf>,
    /// Without a counterpart, the URL the release records for the
    /// submodule: its nested submodules' relative URLs resolve against it,
    /// whichever source the checkout came from.
    released_url: Option<String>,
}

/// Check out in `release`, a worktree of a release commit, the submodules the
/// working copy at `working_copy` reads, at the commits the release records.
///
/// Submodules are matched by name, so one moved since the release is found,
/// and one renamed since by its URL or path. Those checked out in the working
/// copy are required, and fetched from that checkout first. Those the working
/// copy no longer declares, or now fetches from another URL (from its
/// checkout first, in case that is a mirror), are tried, as only the release
/// knows whether it needs them: failing to fetch one only warns, since a
/// release that needs it fails later with Cargo's error anyway. Those the
/// working copy declares but leaves uninitialized stay so. Nested submodules
/// follow the same rules against the working copy's checkout of the same
/// repository. No git configuration is written.
pub(crate) fn check_out(release: &Utf8Path, working_copy: &Utf8Path) -> anyhow::Result<()> {
    check_out_level(release, Some(working_copy), None)
}

/// One level of [`check_out`]: `working_copy` is the working copy's checkout
/// of the repository `release` checks out, if there is one; otherwise
/// `released_url` is the URL the release records for it.
fn check_out_level(
    release: &Utf8Path,
    working_copy: Option<&Utf8Path>,
    released_url: Option<&str>,
) -> anyhow::Result<()> {
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
    // A checkout cloned from the working copy has that copy as its remote,
    // so without a counterpart the URL the release records stands in.
    let base = match (working_copy, released_url) {
        (Some(dir), _) => anchored(dir, remote_url(dir)?),
        (None, Some(url)) => url.to_owned(),
        (None, None) => remote_url(release)?,
    };

    let mut required = Vec::new();
    let mut optional = Vec::new();
    for (name, submodule) in &declared {
        // A `.gitmodules` entry without a gitlink is not a submodule.
        if !gitlinks.contains(&submodule.path) {
            continue;
        }
        // Git ignores such a name (and checks out nothing); it would also
        // place the module's repository outside `modules/`.
        if !is_valid_name(name) {
            warn!(
                "submodule name {name:?} of the release is refused by git; it is not checked out"
            );
            continue;
        }
        let Some(released_url) = &submodule.url else {
            warn!("submodule {name} of the release has no URL; it is not checked out");
            continue;
        };
        let released = || resolve_url(&base, released_url);
        let selected = |sources, counterpart, released_url| Selected {
            name,
            path: &submodule.path,
            sources,
            counterpart,
            released_url,
        };
        // Matched by name; a section renamed since is the same submodule when
        // it alone has the same URL, preferably at the same path, or else
        // alone is at the same path. A section whose name the release
        // declares is that submodule, not this one renamed.
        let matched = current.get(name.as_str()).or_else(|| {
            let renamed = || {
                current
                    .iter()
                    .filter(|(name, _)| !declared.contains_key(*name))
                    .map(|(_, section)| section)
            };
            let same_url =
                |(c, _): &&(Declared, Option<String>)| c.url.as_ref() == Some(released_url);
            let same_path = |(c, _): &&(Declared, Option<String>)| c.path == submodule.path;
            only(renamed().filter(|c| same_url(c) && same_path(c)))
                .or_else(|| only(renamed().filter(same_url)))
                .or_else(|| only(renamed().filter(same_path)))
        });
        let Some((current, configured)) = matched else {
            let released = released()?;
            optional.push(selected(
                vec![Source::Url(released.clone())],
                None,
                Some(released),
            ));
            continue;
        };
        let Some(checkout) = working_copy
            .map(|dir| dir.join(&current.path))
            .filter(|dir| dir.join(".git").exists())
        else {
            // Declared and left uninitialized: not needed.
            continue;
        };
        if current.url.as_ref() == Some(released_url) {
            // The working copy can lack the released commit, e.g. in a shallow
            // checkout; its configured URL can then still have it, and when
            // that overrides the recorded one, the recorded one can.
            let released = released()?;
            let mut sources = vec![Source::WorkingCopy(checkout.clone())];
            if let Some(url) = configured.as_ref().filter(|url| **url != released) {
                sources.push(Source::Url(url.clone()));
            }
            sources.push(Source::Url(released));
            required.push(selected(sources, Some(checkout), None));
        } else {
            // Another URL: a mirror of the same history, whose checkout can have
            // the released commit, or a replacement. Having the commit does not
            // tell them apart (a checkout fetched from the new repository keeps
            // the old objects), so its nested state does not apply either way.
            let released = released()?;
            let sources = vec![Source::WorkingCopy(checkout), Source::Url(released.clone())];
            optional.push(selected(sources, None, Some(released)));
        }
    }

    for submodule in &required {
        check_out_submodule(release, submodule)?;
        check_out_level(
            &release.join(submodule.path),
            submodule.counterpart.as_deref(),
            submodule.released_url.as_deref(),
        )?;
    }
    for submodule in &optional {
        match check_out_submodule(release, submodule) {
            Ok(()) => check_out_level(
                &release.join(submodule.path),
                None,
                submodule.released_url.as_deref(),
            )?,
            Err(error) => warn!("{error:?}"),
        }
    }
    Ok(())
}

/// Check out one submodule of `repo` at the commit `repo` records, from the
/// first of its sources that has it.
fn check_out_submodule(repo: &Utf8Path, submodule: &Selected<'_>) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for source in &submodule.sources {
        match update(repo, submodule, source) {
            Ok(()) => return Ok(()),
            Err(error) => {
                debug!("{error:?}");
                failures.push(format!("from {}: {error:#}", source.describe()));
                discard_clone(repo, submodule)?;
            }
        }
    }
    anyhow::bail!(
        "cannot check out submodule {} of the release at {repo}:\n{}",
        submodule.path,
        failures.join("\n")
    )
}

/// Check out `submodule` in `repo` from `source`, whatever update procedure is
/// configured for it.
fn update(repo: &Utf8Path, submodule: &Selected<'_>, source: &Source) -> anyhow::Result<()> {
    debug!(
        "checking out submodule {} in {repo} from {}",
        submodule.path,
        source.describe()
    );
    let mut command = git(repo);
    // Without `--init`, git reads the URL and the active state from this
    // configuration instead of writing them into the repository's. Keys and
    // values go separately through the environment: a name can contain `=`,
    // and the URL can carry credentials.
    let url_key = format!("submodule.{}.url", submodule.name);
    let active_key = format!("submodule.{}.active", submodule.name);
    let mut config = vec![(active_key.as_str(), "true")];
    match source {
        Source::WorkingCopy(dir) => {
            config.push((url_key.as_str(), dir.as_str()));
            // A path to the user's own checkout, chosen here, not by `.gitmodules`.
            config.push(("protocol.file.allow", "always"));
        }
        Source::Url(url) => config.push((url_key.as_str(), url)),
    }
    add_config(&mut command, &config)?;
    // Paths are literal, not patterns.
    command.args([
        "--literal-pathspecs",
        "submodule",
        "update",
        "--checkout",
        "--",
    ]);
    command.arg(submodule.path);
    // A URL can carry a token in its path or query, which only it reveals.
    let urls = match source {
        Source::WorkingCopy(_) => Vec::new(),
        Source::Url(url) => vec![url.as_str()],
    };
    run_with(
        command,
        &format!("git submodule update of {}", submodule.path),
        &urls,
    )
    .map(drop)
}

/// Remove what a failed checkout of `submodule` left in `repo`, so the next
/// source starts afresh instead of reusing a clone of the previous one.
fn discard_clone(repo: &Utf8Path, submodule: &Selected<'_>) -> anyhow::Result<()> {
    let mut command = git(repo);
    command.args([
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        &format!("modules/{}", submodule.name),
    ]);
    let module = run(command, "git rev-parse --git-path")?;
    let module = Utf8Path::new(module.trim_end_matches('\n'));
    if module.exists() {
        fs_err::remove_dir_all(module)?;
    }
    let checkout = repo.join(submodule.path);
    if checkout.exists() {
        fs_err::remove_dir_all(&checkout)?;
    }
    fs_err::create_dir_all(&checkout)?;
    Ok(())
}

/// Add `entries` to the configuration git reads from the environment, after
/// any the caller's environment already sets.
fn add_config(command: &mut Command, entries: &[(&str, &str)]) -> anyhow::Result<()> {
    let inherited = match std::env::var("GIT_CONFIG_COUNT") {
        Ok(count) => count
            .parse::<usize>()
            .with_context(|| format!("invalid GIT_CONFIG_COUNT {count:?}"))?,
        Err(_) => 0,
    };
    for (index, &(key, value)) in (inherited..).zip(entries) {
        command
            .env(format!("GIT_CONFIG_KEY_{index}"), key)
            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    command.env("GIT_CONFIG_COUNT", (inherited + entries.len()).to_string());
    Ok(())
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
    let Some(entries) = run_optional(command, &format!("reading {gitmodules}"))? else {
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

/// The submodules the repository at `dir` declares and has a gitlink for,
/// each with the URL its configuration holds for it.
fn declared_with_config(
    dir: &Utf8Path,
) -> anyhow::Result<BTreeMap<String, (Declared, Option<String>)>> {
    let gitlinks = gitlinks(dir)?;
    declared(&dir.join(".gitmodules"))?
        .into_iter()
        // A section left behind by a removed gitlink is not a submodule.
        .filter(|(_, submodule)| gitlinks.contains(&submodule.path))
        .map(|(name, submodule)| {
            let configured = config(dir, &format!("submodule.{name}.url"))?
                .map(|url| anchored(dir, url));
            Ok((name, (submodule, configured)))
        })
        .collect()
}

/// The only item of `items`, if there is exactly one.
fn only<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let item = items.next()?;
    items.next().is_none().then_some(item)
}

/// Paths of the gitlinks of the index of `repo`.
fn gitlinks(repo: &Utf8Path) -> anyhow::Result<BTreeSet<String>> {
    let mut command = git(repo);
    command.args(["ls-files", "--stage", "-z"]);
    let index = run(command, "git ls-files")?;
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
    let branch_remote = match run_optional(command, "git symbolic-ref")? {
        Some(branch) => config(repo, &format!("branch.{}.remote", branch.trim_end()))?,
        None => None,
    };
    let remote = branch_remote.unwrap_or_else(|| "origin".to_owned());
    Ok(config(repo, &format!("remote.{remote}.url"))?.unwrap_or_else(|| repo.to_string()))
}

/// Resolve a submodule URL against the superproject's remote URL, as git's
/// `relative_url` does: a URL starting with `./` or `../` is relative to
/// `remote`, any other is used as is.
fn resolve_url(remote: &str, url: &str) -> anyhow::Result<String> {
    if !url.starts_with("./") && !url.starts_with("../") {
        return Ok(url.to_owned());
    }
    // Git drops a single trailing separator; any other is a path component.
    let mut base = remote.strip_suffix('/').unwrap_or(remote).to_owned();
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
    let resolved = format!(
        "{base}{separator}{}",
        rest.strip_suffix('/').unwrap_or(rest)
    );
    Ok(resolved
        .strip_prefix("./")
        .map_or_else(|| resolved.clone(), str::to_owned))
}

/// `url`, read from the configuration of the repository at `dir`, usable from
/// any directory: git reads a relative local path from the repository's
/// directory, and the release is checked out elsewhere.
fn anchored(dir: &Utf8Path, url: String) -> String {
    if is_local_not_ssh(&url) && Utf8Path::new(&url).is_relative() {
        dir.join(url).into_string()
    } else {
        url
    }
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
    let value = run_optional(command, &format!("reading {key}"))?;
    Ok(value.map(|value| value.trim_end_matches('\n').to_owned()))
}

fn git(repo: &Utf8Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    command
}

/// Whether git accepts `name` as a submodule name: not empty, and without a
/// `..` component on either separator, so that its repository stays under
/// `modules/` (git's `check_submodule_name`).
fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && !name.split(['/', '\\']).any(|component| component == "..")
}

/// The authority of the `scheme://` URL `after` starts right after: it ends
/// at the path, query or fragment (RFC 3986 3.2), and no URL holds
/// whitespace. Userinfo can hold quotes (RFC 3986 3.2.1).
fn authority_len(after: &str) -> usize {
    after
        .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
        .unwrap_or(after.len())
}

/// `url` without its userinfo, as git shows some URLs.
fn without_userinfo(url: &str) -> Option<String> {
    let scheme_end = url.find("://")? + 3;
    let authority = &url[scheme_end..scheme_end + authority_len(&url[scheme_end..])];
    let at = authority.rfind('@')?;
    Some(format!(
        "{}{}",
        &url[..scheme_end],
        &url[scheme_end + at + 1..]
    ))
}

/// `text` with credentials removed from the URLs it names, as git repeats
/// URLs in its errors: each of `urls` (the URLs given to git, which can
/// carry a token in their path) wholly, with or without its userinfo, and
/// the userinfo and query of any other `scheme://` URL.
fn redact(text: &str, urls: &[&str]) -> String {
    let mut text = text.to_owned();
    // Any form git takes, scp-like ones included, which it repeats verbatim.
    for url in urls.iter().filter(|url| !url.is_empty()) {
        for form in std::iter::once((*url).to_owned()).chain(without_userinfo(url)) {
            text = text.replace(&form, "<submodule URL>");
        }
    }
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(scheme_end) = rest.find("://") {
        let (before, after) = rest.split_at(scheme_end + 3);
        redacted.push_str(before);
        let (authority, tail) = after.split_at(authority_len(after));
        match authority.rfind('@') {
            Some(at) => {
                redacted.push_str("***");
                redacted.push_str(&authority[at..]);
            }
            None => redacted.push_str(authority),
        }
        // A query can carry a token, as a signed URL does.
        let path_len = tail
            .find(|c: char| matches!(c, '?' | '\'' | '"') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (path, tail) = tail.split_at(path_len);
        redacted.push_str(path);
        rest = match tail.strip_prefix('?') {
            Some(query) => {
                redacted.push_str("?***");
                &query[query.find(char::is_whitespace).unwrap_or(query.len())..]
            }
            None => tail,
        };
    }
    redacted.push_str(rest);
    redacted
}

/// The stderr of `output`, safe to show (see [`redact`]).
fn stderr(output: &std::process::Output, urls: &[&str]) -> String {
    redact(String::from_utf8_lossy(&output.stderr).trim_end(), urls)
}

/// Run `command`, failing on any non-zero exit. Errors name the command by
/// `description` only, its arguments and environment can hold credentials,
/// and carry git's message with the credentials of the URLs it names
/// removed.
fn run(command: Command, description: &str) -> anyhow::Result<String> {
    run_with(command, description, &[])
}

/// [`run`] a command given `urls`, which its errors withhold wholly.
fn run_with(mut command: Command, description: &str, urls: &[&str]) -> anyhow::Result<String> {
    let output = command
        .output()
        .with_context(|| format!("cannot run {description}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed: {}",
        stderr(&output, urls)
    );
    String::from_utf8(output.stdout).with_context(|| format!("non-UTF-8 output of {description}"))
}

/// Run `command`, whose exit code 1 means nothing was found.
fn run_optional(mut command: Command, description: &str) -> anyhow::Result<Option<String>> {
    let output = command
        .output()
        .with_context(|| format!("cannot run {description}"))?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(Some)
            .with_context(|| format!("non-UTF-8 output of {description}")),
        Some(1) => Ok(None),
        _ => anyhow::bail!("{description} failed: {}", stderr(&output, &[])),
    }
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

    /// Git drops one trailing separator of the remote and of the URL, so the
    /// others count as directory components.
    #[test]
    fn only_one_trailing_separator_is_dropped() {
        for (remote, url, expected) in [
            (
                "https://example.com/group/app//",
                "../dep",
                "https://example.com/group/app/dep",
            ),
            (
                "https://example.com/group/app",
                "../dep//",
                "https://example.com/group/dep/",
            ),
        ] {
            assert_eq!(
                resolve_url(remote, url).unwrap(),
                expected,
                "{remote} + {url}"
            );
        }
    }

    /// Git repeats a URL in its errors with any credentials it carries: they
    /// do not reach the error a failed command returns.
    #[test]
    fn credentials_in_git_errors_are_redacted() {
        let dir = crate::fs_utils::Utf8TempDir::new().unwrap();
        let mut command = git(dir.path());
        command.args(["init", "-q"]);
        run(command, "git init").unwrap();
        // Git repeats the unmatched pathspec verbatim.
        let mut command = git(dir.path());
        command.args(["checkout", "https://user:s3cret@example.com/dep.git"]);
        let error = format!("{:#}", run(command, "git checkout").unwrap_err());
        assert!(!error.contains("s3cret"), "{error}");
        assert!(error.contains("example.com/dep.git"), "{error}");
    }

    #[test]
    fn userinfo_is_redacted_from_every_url() {
        assert_eq!(
            redact(
                "clone of 'https://user:pw@host/a' and ssh://git@host:22/b, not a@b or file:///c",
                &[]
            ),
            "clone of 'https://***@host/a' and ssh://***@host:22/b, not a@b or file:///c"
        );
    }

    /// Userinfo can hold a quote (RFC 3986 3.2.1): the authority runs to the
    /// path, query or fragment, not to the quote.
    #[test]
    fn userinfo_with_a_quote_is_redacted() {
        assert_eq!(
            redact("'https://user:pa'ss@host/x'", &[]),
            "'https://***@host/x'"
        );
    }

    /// A query can carry a token, as a signed URL does.
    #[test]
    fn queries_are_redacted_from_every_url() {
        assert_eq!(
            redact("access to 'https://host/a?token=s3cret' denied", &[]),
            "access to 'https://host/a?*** denied"
        );
    }

    /// A URL given to git can carry a token in its path: it is withheld
    /// wherever git repeats it, with or without the userinfo git may strip.
    #[test]
    fn urls_given_to_git_are_withheld() {
        let url = "https://user:pw@host/T0KEN/dep.git";
        for text in [
            "fatal: repository 'https://user:pw@host/T0KEN/dep.git/' not found",
            "fatal: unable to access 'https://host/T0KEN/dep.git/': 403",
        ] {
            let redacted = redact(text, &[url]);
            assert!(!redacted.contains("T0KEN"), "{redacted}");
            assert!(!redacted.contains("pw"), "{redacted}");
        }
    }

    /// Git repeats an scp-like URL verbatim, so one given to it is withheld
    /// as well, its user and path being able to carry a token.
    #[test]
    fn scp_like_urls_given_to_git_are_withheld() {
        let url = "t0ken@host:SECRET/dep.git";
        let redacted = redact(
            "fatal: 't0ken@host:SECRET/dep.git' does not appear to be a git repository",
            &[url],
        );
        assert_eq!(
            redacted,
            "fatal: '<submodule URL>' does not appear to be a git repository"
        );
    }

    /// An empty URL given to git withholds nothing, rather than matching
    /// everywhere.
    #[test]
    fn an_empty_url_given_to_git_withholds_nothing() {
        assert_eq!(redact("fatal: no", &[""]), "fatal: no");
    }

    /// A name with a `..` component, which git refuses, names a directory
    /// outside the repository's metadata: the checkout skips it and removes
    /// nothing there.
    #[test]
    fn a_name_climbing_out_of_the_metadata_removes_nothing() {
        let dir = crate::fs_utils::Utf8TempDir::new().unwrap();
        let git_ok = |repo: &Utf8Path, args: &[&str]| {
            // The identity is set here: a runner has no global one.
            let mut command = git(repo);
            command
                .args(["-c", "protocol.file.allow=always"])
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args);
            run(command, "git").unwrap();
        };
        let victim = dir.path().join("victim");
        fs_err::create_dir_all(&victim).unwrap();
        fs_err::write(victim.join("keep"), "").unwrap();
        let dep = dir.path().join("dep");
        fs_err::create_dir_all(&dep).unwrap();
        git_ok(&dep, &["init", "-q"]);
        git_ok(&dep, &["commit", "-q", "--allow-empty", "-m", "dep"]);
        let superproject = dir.path().join("super");
        fs_err::create_dir_all(&superproject).unwrap();
        git_ok(&superproject, &["init", "-q"]);
        git_ok(
            &superproject,
            &["submodule", "add", "-q", dep.as_str(), "dep"],
        );
        git_ok(
            &superproject,
            &[
                "config",
                "-f",
                ".gitmodules",
                "--rename-section",
                "submodule.dep",
                "submodule.../../../victim",
            ],
        );
        git_ok(&superproject, &["commit", "-qam", "release"]);
        let release = dir.path().join("release");
        git_ok(
            &superproject,
            &["worktree", "add", "-q", "--detach", release.as_str()],
        );

        check_out(&release, &superproject).unwrap();
        assert!(victim.join("keep").exists(), "removed outside the metadata");
    }

    #[test]
    fn git_refuses_names_with_a_parent_component() {
        for name in ["../x", "a/../x", "a\\..\\x", "..", ""] {
            assert!(!is_valid_name(name), "{name:?}");
        }
        for name in ["a..b", "..a", "a/..b", "vendor/dep", "a=b"] {
            assert!(is_valid_name(name), "{name:?}");
        }
    }

    #[test]
    fn a_relative_url_cannot_climb_above_a_relative_remote() {
        assert!(resolve_url("app", "../../dep").is_err());
    }
}
