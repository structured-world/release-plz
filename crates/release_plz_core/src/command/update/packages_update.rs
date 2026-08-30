use cargo_metadata::{Package, semver::Version};
use tracing::warn;

use crate::{UpdateResult, semver_check::SemverCheck};

use super::ReleaseInfo;

pub type PackagesToUpdate = Vec<(Package, UpdateResult)>;

#[derive(Clone, Debug, Default)]
pub struct PackagesUpdate {
    updates: PackagesToUpdate,
    /// New workspace version. If None, the workspace version is not updated.
    /// See cargo [docs](https://doc.rust-lang.org/cargo/reference/workspaces.html#root-package).
    workspace_version: Option<Version>,
}

/// The title and notes describing `update` in the release notes.
///
/// Both are `None` when the package has no entry for this release. That case
/// matters: `update.changelog` holds the package's whole changelog file, so
/// parsing its top section returns whatever shipped LAST time. A package
/// carried along by a dependency bump has nothing written above that, and
/// reporting it here would present already-released work as part of this
/// release.
fn release_notes(update: &UpdateResult, package_name: &str) -> (Option<String>, Option<String>) {
    // Absent OR blank: a package carried along by a dependency bump gets an
    // entry that is present but empty, which is the same "nothing happened
    // here" the aggregate changelog filters out.
    let has_entry = update
        .new_changelog_entry
        .as_ref()
        .is_some_and(|e| !e.trim().is_empty());
    if !has_entry {
        return (None, None);
    }
    let from_entry = (None, update.new_changelog_entry.clone());
    match update.last_changes() {
        Err(e) => {
            warn!("can't determine changes in changelog of package {package_name}: {e:?}");
            from_entry
        }
        // Only when that section is THIS release. A package updated because a
        // dependency moved keeps its previous changelog in memory, so the top
        // section there names the version before this one; reporting it would
        // put the last release's notes under the new version. The entry
        // computed for this release is the truthful answer in that case.
        Ok(Some(c)) if c.title().contains(&update.version.to_string()) => {
            (Some(c.title().to_string()), Some(c.notes().to_string()))
        }
        Ok(Some(_)) => from_entry,
        Ok(None) => {
            warn!("no changes detected in changelog of package {package_name}");
            from_entry
        }
    }
}

impl PackagesUpdate {
    pub fn new(updates: PackagesToUpdate) -> Self {
        Self {
            updates,
            workspace_version: None,
        }
    }

    pub fn with_workspace_version(&mut self, workspace_version: Version) {
        self.workspace_version = Some(workspace_version);
    }

    pub fn updates(&self) -> &[(Package, UpdateResult)] {
        &self.updates
    }

    pub fn updates_clone(&self) -> PackagesToUpdate {
        self.updates.clone()
    }

    pub fn updates_mut(&mut self) -> &mut PackagesToUpdate {
        &mut self.updates
    }

    pub fn workspace_version(&self) -> Option<&Version> {
        self.workspace_version.as_ref()
    }

    pub fn summary(&self) -> String {
        let updates = self.updates_summary();
        let breaking_changes = self.breaking_changes();
        format!("{updates}\n{breaking_changes}")
    }

    fn updates_summary(&self) -> String {
        self.updates
            .iter()
            .map(|(package, update)| {
                // Use registry_version as previous_version when available
                // (version already bumped case), otherwise use package.version
                let previous_version = update.registry_version.as_ref().unwrap_or(&package.version);
                if previous_version == &update.version {
                    format!("\n* `{}`: {}", package.name, update.version)
                } else {
                    format!(
                        "\n* `{}`: {} -> {}{}",
                        package.name,
                        previous_version,
                        update.version,
                        update.semver_check.outcome_str()
                    )
                }
            })
            .collect()
    }

    pub fn breaking_changes(&self) -> String {
        self.updates
            .iter()
            .map(|(package, update)| match &update.semver_check {
                SemverCheck::Incompatible(incompatibilities) => {
                    format!(
                        "\n### ⚠️ `{}` breaking changes\n\n```{}```\n",
                        package.name, incompatibilities
                    )
                }
                SemverCheck::Compatible | SemverCheck::Skipped => String::new(),
            })
            .collect()
    }

    /// Return info about releases of the updated packages
    pub fn releases(&self) -> Vec<ReleaseInfo> {
        self.updates
            .iter()
            .map(|(package, update)| {
                let (changelog_title, changelog_notes) =
                    release_notes(update, package.name.as_str());

                let (semver_check, breaking_changes) = match &update.semver_check {
                    SemverCheck::Incompatible(incompatibilities) => {
                        ("incompatible", Some(incompatibilities.clone()))
                    }
                    SemverCheck::Compatible => ("compatible", None),
                    SemverCheck::Skipped => ("skipped", None),
                };

                // Use registry_version as previous_version when available
                // (version already bumped case), otherwise use package.version
                let previous_version = update
                    .registry_version
                    .as_ref()
                    .unwrap_or(&package.version)
                    .to_string();

                ReleaseInfo {
                    package: package.name.to_string(),
                    title: changelog_title,
                    changelog: changelog_notes,
                    next_version: update.version.to_string(),
                    previous_version,
                    breaking_changes,
                    semver_check: semver_check.to_string(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod release_notes_tests {
    use super::release_notes;
    use crate::UpdateResult;
    use crate::semver_check::SemverCheck;
    use cargo_metadata::semver::Version;

    /// A package's changelog file as it stands after the previous release.
    const EXISTING_CHANGELOG: &str = "\
# Changelog

## [0.5.2](https://example.com/compare/v0.5.1...v0.5.2) - 2026-08-30

### Fixed

- something that shipped last time
";

    fn update(new_entry: Option<&str>) -> UpdateResult {
        UpdateResult {
            version: Version::new(0, 5, 3),
            changelog: Some(EXISTING_CHANGELOG.to_string()),
            semver_check: SemverCheck::Skipped,
            new_changelog_entry: new_entry.map(str::to_string),
            registry_version: None,
        }
    }

    /// A package bumped only because a dependency moved has nothing to say in
    /// the release notes. Reporting the top of its changelog would republish
    /// the previous release's entries under the new version.
    #[test]
    fn a_package_without_a_new_entry_contributes_no_notes() {
        let (title, notes) = release_notes(&update(None), "coordinode-core");
        assert_eq!(title, None, "no title for a package with no changes");
        assert_eq!(notes, None, "no notes for a package with no changes");
    }

    /// A package bumped only by a dependency gets an entry that exists but is
    /// blank. It is the same "nothing happened here" as no entry at all, and it
    /// is the shape that actually occurs: every unchanged crate in a workspace
    /// release arrives this way.
    #[test]
    fn a_package_with_a_blank_entry_contributes_no_notes() {
        let (title, notes) = release_notes(&update(Some("\n  \n")), "coordinode-core");
        assert_eq!(title, None, "no title for a blank entry");
        assert_eq!(notes, None, "no notes for a blank entry");
    }

    /// A package updated because a dependency moved carries a real entry for
    /// this release, but its in-memory changelog still starts at the PREVIOUS
    /// version. The notes must come from the entry, not from that section: this
    /// is how the server crate ended up showing 0.5.2 inside the 0.5.3 release.
    #[test]
    fn a_stale_top_section_does_not_describe_this_release() {
        let (title, notes) = release_notes(
            &update(Some("### Fixed\n\n- the thing this release fixes\n")),
            "coordinode-server",
        );
        assert_eq!(
            title, None,
            "a section for an older version must not title this release",
        );
        let notes = notes.expect("the entry computed for this release describes it");
        assert!(
            notes.contains("the thing this release fixes"),
            "notes must come from this release's entry, got: {notes}",
        );
        assert!(
            !notes.contains("shipped last time"),
            "the previous release's notes must not appear, got: {notes}",
        );
    }

    /// A package that did change is described by its changelog, as before.
    #[test]
    fn a_package_with_a_new_entry_is_described() {
        let mut u = update(Some("### Fixed\n\n- the thing this release fixes\n"));
        // Its changelog was regenerated, so the top section IS this release.
        u.changelog = Some(
            "# Changelog\n\n## [0.5.3](https://example.com/compare/v0.5.2...v0.5.3) - 2026-08-30\n\n### Fixed\n\n- the thing this release fixes\n"
                .to_string(),
        );
        let (title, notes) = release_notes(&u, "coordinode-server");
        assert!(
            title.is_some_and(|t| t.contains("0.5.3")),
            "a changed package is titled with the version being released",
        );
        assert!(
            notes.is_some_and(|n| n.contains("the thing this release fixes")),
            "a changed package keeps its notes",
        );
    }
}
