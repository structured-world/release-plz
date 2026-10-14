use cargo_metadata::camino::{Utf8Path, Utf8PathBuf};
use git_cmd::Repo;
use release_plz_core::fs_utils::Utf8TempDir;

use crate::helpers::{assert_locked_versions, cmd::release_plz_cmd, locked_metadata};

#[test]
fn update_workspace_without_publishable_packages() {
    let (temp_dir, repo) = released_workspace(
        &[("one", "version = \"1.0.0\"\npublish = false\n")],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    change_package(&repo, "one", "feat: update private package");
    let original_commit = repo.current_commit_hash().unwrap();

    run_workspace_update(&temp_dir, &repo, None);

    repo.is_clean().unwrap();
    assert_eq!(repo.current_commit_hash().unwrap(), original_commit);
    assert!(!repo.directory().join("one/CHANGELOG.md").exists());
}

#[test]
fn update_changelog_includes_distinct_author_and_committer() {
    for (body, expected) in [
        (
            "{{ commit.author.name }} <{{ commit.author.email }}> committed by {{ commit.committer.name }} <{{ commit.committer.email }}>",
            "Author Person <author@example.com> committed by Committer Person <committer@example.com>",
        ),
        ("{{ commit.author.email }}", "author@example.com"),
    ] {
        let config = format!(
            r#"
[workspace]
semver_check = false
[changelog]
body = """
{{% for commit in commits %}}
{body}
{{% endfor %}}
"""
"#
        );
        let (temp_dir, repo) = released_workspace(&[("one", "version = \"1.0.0\"\n")], "", &config);
        fs_err::write(repo.directory().join("one/src/lib.rs"), "// Updated\n").unwrap();
        repo.git(&["add", "."]).unwrap();
        repo.git(&[
            "-c",
            "user.name=Committer Person",
            "-c",
            "user.email=committer@example.com",
            "commit",
            "--author=Author Person <author@example.com>",
            "-m",
            "fix: update package",
        ])
        .unwrap();

        run_workspace_update(&temp_dir, &repo, None);

        let changelog = fs_err::read_to_string(repo.directory().join("one/CHANGELOG.md")).unwrap();
        assert!(changelog.contains(expected), "{changelog}");
    }
}

#[test]
fn changelog_include_uses_only_the_requested_packages_original_commits() {
    let config = r#"
[workspace]
semver_check = false
[[package]]
name = "one"
changelog_include = ["two", "two"]
[[package]]
name = "two"
changelog_include = ["three"]
"#;
    let (temp_dir, repo) = released_workspace(
        &[
            ("one", "version = \"1.0.0\"\n"),
            ("two", "version = \"1.0.0\"\n"),
            ("three", "version = \"1.0.0\"\n"),
        ],
        "",
        config,
    );
    for name in ["one", "two", "three"] {
        change_package(&repo, name, &format!("fix: update {name}"));
    }

    run_workspace_update(&temp_dir, &repo, None);

    for (name, included) in [
        ("one", vec!["one", "two"]),
        ("two", vec!["two", "three"]),
        ("three", vec!["three"]),
    ] {
        let changelog =
            fs_err::read_to_string(repo.directory().join(name).join("CHANGELOG.md")).unwrap();
        for source in ["one", "two", "three"] {
            assert_eq!(
                changelog.matches(&format!("update {source}")).count(),
                usize::from(included.contains(&source)),
                "{name}: {changelog}",
            );
        }
    }
}

#[tokio::test]
async fn update_relocated_workspace_preserves_original_manifests() {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.0\"\n"),
            (
                "consumer",
                "version = \"1.0.0\"\n[dependencies]\nsupport = { path = \"../support\", version = \"=1.0.0\" }\n",
            ),
        ],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    change_package(&repo, "support", "feat: update support");
    let metadata = locked_metadata(repo.directory());
    let copied = release_plz_core::copy_to_temp_dir(repo.directory()).unwrap();
    let copied_project = copied.path().join("project");
    let request = release_plz_core::update_request::UpdateRequest::new(metadata)
        .unwrap()
        .set_local_manifest(copied_project.join("Cargo.toml"))
        .unwrap()
        .with_registry_manifest_path(&temp_dir.path().join("registry/Cargo.toml"))
        .unwrap()
        .with_default_package_config(
            release_plz_core::UpdateConfig::default().with_semver_check(false),
        );

    release_plz_core::update(&request).await.unwrap();

    // release-pr retains the original metadata while updating the copied checkout.
    assert_locked_versions(
        &copied_project,
        &[("support", "1.1.0"), ("consumer", "1.0.1")],
    );
    repo.is_clean().unwrap();
    assert_locked_versions(
        repo.directory(),
        &[("support", "1.0.0"), ("consumer", "1.0.0")],
    );
}

#[test]
fn update_reuses_package_files_after_source_content_changes() {
    // The three historical snapshots differ only in Rust source contents. Cargo
    // is invoked once for the local history and once for the released source tree.
    assert_package_file_listings_with_change("source contents", |_| {}, 2, 2);
}

#[test]
fn update_lists_package_files_again_after_changes_beyond_source_contents() {
    // Only some cases change Cargo's file list. The others, such as the
    // lockfile, the build configuration or the workspace license, show that
    // any tree change beyond the contents of Rust sources makes Cargo list the
    // files again.
    for (path, contents) in [
        (
            "one/Cargo.toml",
            "[package]\nname = \"one\"\nversion = \"1.0.0\"\nedition = \"2024\"\ninclude = [\"src/**\"]\n",
        ),
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"one\"]\nresolver = \"3\"\n[workspace.package]\nlicense = \"MIT\"\n",
        ),
        ("one/.gitignore", "ignored\n"),
        (".cargo/config.toml", "[build]\njobs = 1\n"),
        ("one/src/extra.rs", "// Another module\n"),
        (
            "one/nested/Cargo.toml",
            "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n[workspace]\n",
        ),
        (
            "Cargo.lock",
            "# Changed lockfile\nversion = 4\n[[package]]\nname = \"one\"\nversion = \"1.0.0\"\n",
        ),
    ] {
        // HEAD and its parent must each ask Cargo. The parent and initial
        // release differ only in Rust contents and share the second list.
        assert_package_file_listings_with_change(
            path,
            |repo| {
                let changed = repo.directory().join(path);
                fs_err::create_dir_all(changed.parent().unwrap()).unwrap();
                fs_err::write(changed, contents).unwrap();
            },
            3,
            1,
        );
    }
}

#[cfg(unix)]
#[test]
fn update_lists_package_files_again_after_mode_changes() {
    // Executable scripts can be Cargo or rustc wrappers. As for other changes
    // beyond source contents, HEAD and its parent must each ask Cargo.
    assert_package_file_listings_with_change(
        "executable source",
        |repo| crate::helpers::make_executable(&repo.directory().join("one/src/lib.rs")),
        3,
        1,
    );
}

#[test]
fn update_lists_package_files_again_after_changes_in_skipped_commits() {
    // Both fixes change only Rust sources, but the path-limited walk skips the
    // build configuration commit between them, so the first fix differs from
    // HEAD, the previous checkout, beyond source contents. HEAD and the first
    // fix each ask Cargo, and the release snapshot reuses the second list.
    assert_package_file_listings_with_change(
        "skipped commits",
        |repo| {
            let config = repo.directory().join(".cargo/config.toml");
            fs_err::create_dir_all(config.parent().unwrap()).unwrap();
            fs_err::write(config, "[build]\njobs = 1\n").unwrap();
            repo.add_all_and_commit("chore: build config").unwrap();
        },
        3,
        1,
    );
}

#[cfg(unix)]
#[test]
fn update_lists_package_files_when_checkout_hooks_change_files() {
    for (event, configured) in [
        ("post-checkout", false),
        ("post-index-change", false),
        ("reference-transaction", false),
        ("post-checkout", true),
    ] {
        let case = format!("{event}, configured={configured}");
        let (temp_dir, repo) = single_package_workspace();
        change_package(&repo, "one", "fix: first change");
        change_package(&repo, "one", "fix: second change");
        // Some checkout hooks also run when committing, so install the hook
        // after the setup commits.
        if configured {
            // Git 2.54 and later also run hooks defined in the configuration.
            // Older Git ignores them, but they still disable reuse.
            let script = ".git/generate-package-file";
            install_checkout_hook(&repo.directory().join(script));
            repo.git(&["config", "hook.generate.event", event]).unwrap();
            repo.git(&["config", "hook.generate.command", script])
                .unwrap();
        } else {
            install_checkout_hook(&repo.directory().join(".git/hooks").join(event));
        }

        // The hook removes the generated file at the released snapshot. A list
        // reused from its successor would hide the equal released snapshot and
        // bring the initial commit into the changelog.
        assert_package_file_listings(&temp_dir, &repo, &case, 4, 0);
    }
}

#[cfg(unix)]
#[test]
fn update_lists_package_files_when_older_snapshots_track_a_checkout_hook() {
    let (temp_dir, repo) = single_package_workspace();
    repo.git(&["config", "core.hooksPath", ".hooks"]).unwrap();
    let hook = repo.directory().join(".hooks/post-checkout");
    install_checkout_hook(&hook);
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "--no-edit"]).unwrap();
    change_package(&repo, "one", "fix: first change");
    // The hook is absent at HEAD but runs at older snapshots. Checking for it
    // only when the walk starts would still reuse a stale list.
    fs_err::remove_file(&hook).unwrap();
    change_package(&repo, "one", "fix: second change");

    assert_package_file_listings(&temp_dir, &repo, "tracked hook", 4, 0);
}

/// Install at `hook` an executable script that removes the untracked file
/// `one/generated.txt` when `one/src/lib.rs` still has the contents written by
/// [`init_workspace`], and creates it otherwise.
#[cfg(unix)]
fn install_checkout_hook(hook: &Utf8Path) {
    fs_err::create_dir_all(hook.parent().unwrap()).unwrap();
    fs_err::write(
        hook,
        "#!/bin/sh\n\
         if grep -q 'Initial release' one/src/lib.rs; then\n\
             rm -f one/generated.txt\n\
         else\n\
             echo 'generated data' > one/generated.txt\n\
         fi\n",
    )
    .unwrap();
    crate::helpers::make_executable(hook);
}

#[cfg(unix)]
#[test]
fn update_lists_package_files_with_a_symlinked_manifest() {
    let (temp_dir, repo) = init_workspace(
        &[("one", "version = \"1.0.0\"\ninclude = [\"src/**\"]\n")],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    for dir in [
        repo.directory().to_path_buf(),
        temp_dir.path().join("registry"),
    ] {
        fs_err::write(dir.join("one/src/extra.rs"), "// Initial extra module\n").unwrap();
    }
    let package = repo.directory().join("one");
    let manifest = package.join("manifest.rs");
    fs_err::rename(package.join("Cargo.toml"), &manifest).unwrap();
    std::os::unix::fs::symlink("manifest.rs", package.join("Cargo.toml")).unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "--amend", "-m", "feat!: initial API"])
        .unwrap();

    fs_err::write(package.join("src/extra.rs"), "// Extra capability\n").unwrap();
    repo.add_all_and_commit("feat: extra capability").unwrap();
    let contents = fs_err::read_to_string(&manifest).unwrap();
    fs_err::write(&manifest, contents.replace("src/**", "src/lib.rs")).unwrap();
    change_package(&repo, "one", "fix: select sources");

    let logs = run_workspace_update_with_logs(&temp_dir, &repo);

    // The symlink itself is unchanged, but its regular .rs target changes
    // Cargo's file selection. Reusing HEAD's list also hides the equal released
    // snapshot, bringing the already released breaking change into this update.
    assert_locked_versions(repo.directory(), &[("one", "1.1.0")]);
    let changelog = fs_err::read_to_string(package.join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("extra capability"), "{changelog}");
    assert!(changelog.contains("select sources"), "{changelog}");
    assert!(!changelog.contains("initial API"), "{changelog}");
    assert!(
        !logs.contains("reusing historical package file list"),
        "{logs}"
    );
}

/// Run [`assert_package_file_listings`] on a [`single_package_workspace`]
/// after the commits `fix: first change` and `fix: second change` to the Rust
/// source of `one`. `change` runs before the second source change is committed.
/// Changes it leaves uncommitted are committed with that source change, so they
/// land in a walked commit, while a change it commits on its own outside the
/// package directory is skipped by the path-limited walk.
fn assert_package_file_listings_with_change(
    case: &str,
    change: impl FnOnce(&Repo),
    cargo_runs: usize,
    reuses: usize,
) {
    let (temp_dir, repo) = single_package_workspace();
    change_package(&repo, "one", "fix: first change");
    change(&repo);
    change_package(&repo, "one", "fix: second change");
    assert_package_file_listings(&temp_dir, &repo, case, cargo_runs, reuses);
}

/// Assert that updating the [`single_package_workspace`] of `repo` runs
/// `cargo package --list` `cargo_runs` times, including once for the released
/// source tree, and reuses a historical package file list `reuses` times, while
/// the update still finds exactly the package changes `fix: first change` and
/// `fix: second change`.
fn assert_package_file_listings(
    temp_dir: &Utf8TempDir,
    repo: &Repo,
    case: &str,
    cargo_runs: usize,
    reuses: usize,
) {
    let logs = run_workspace_update_with_logs(temp_dir, repo);

    assert_eq!(
        logs.matches("Run `cargo package --list").count(),
        cargo_runs,
        "{case}: {logs}"
    );
    assert_eq!(
        logs.matches("reusing historical package file list").count(),
        reuses,
        "{case}: {logs}"
    );
    assert_patch_release(repo, case, &["first change", "second change"]);
}

/// Creates a workspace with [`init_workspace`] whose only package `one` is at 1.0.0.
fn single_package_workspace() -> (Utf8TempDir, Repo) {
    let (temp_dir, repo) = init_workspace(
        &[("one", "version = \"1.0.0\"\n")],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    // Converted line endings would make every snapshot differ from the release.
    repo.git(&["config", "core.autocrlf", "false"]).unwrap();
    // Ignore a global hooks path: its hooks would disable package file list
    // reuse, and it would hide the hooks that tests install in .git/hooks.
    repo.git(&["config", "core.hooksPath", ".git/hooks"])
        .unwrap();
    (temp_dir, repo)
}

/// Assert that the update released `one` as 1.0.1 with a changelog that lists
/// `messages` but not the initial commit, "add README".
fn assert_patch_release(repo: &Repo, case: &str, messages: &[&str]) {
    assert_locked_versions(repo.directory(), &[("one", "1.0.1")]);
    let changelog = fs_err::read_to_string(repo.directory().join("one/CHANGELOG.md")).unwrap();
    for message in messages {
        assert!(changelog.contains(message), "{case}: {changelog}");
    }
    assert!(!changelog.contains("add README"), "{case}: {changelog}");
}

#[test]
fn update_lists_package_files_of_replaced_commits() {
    let (temp_dir, repo) = single_package_workspace();
    let release = repo.current_commit_hash().unwrap();
    repo.git(&["commit", "--allow-empty", "-m", "fix: add extra"])
        .unwrap();
    let replaced = repo.current_commit_hash().unwrap();
    change_package(&repo, "one", "fix: second change");
    // Checkouts of the empty commit follow its replacement, which adds a
    // packaged file. The empty commit's own tree differs from HEAD's only in a
    // Rust source.
    repo.git(&["checkout", "--detach", &release]).unwrap();
    fs_err::write(repo.directory().join("one/extra.txt"), "extra\n").unwrap();
    repo.add_all_and_commit("fix: add extra").unwrap();
    let replacement = repo.current_commit_hash().unwrap();
    repo.git(&["checkout", "-"]).unwrap();
    repo.git(&["replace", &replaced, &replacement]).unwrap();

    let logs = run_workspace_update_with_logs(&temp_dir, &repo);

    // Reusing HEAD's file list would miss the added file and make the replaced
    // commit equal the release, stopping the walk there.
    assert!(
        !logs.contains("reusing historical package file list"),
        "{logs}"
    );
    assert_patch_release(&repo, "replaced commits", &["add extra", "second change"]);
}

#[test]
fn update_refreshes_package_files_for_each_historical_snapshot() {
    let (temp_dir, repo) = init_workspace(
        &[("one", "version = \"1.0.0\"\n")],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    let package = repo.directory().join("one");
    fs_err::write(package.join("src/old.rs"), "pub fn added() {}\n").unwrap();
    repo.add_all_and_commit("feat: add module").unwrap();
    fs_err::rename(package.join("src/old.rs"), package.join("src/new.rs")).unwrap();
    repo.add_all_and_commit("fix: rename module").unwrap();

    run_workspace_update(&temp_dir, &repo, None);

    assert_locked_versions(repo.directory(), &[("one", "1.1.0")]);
    let changelog = fs_err::read_to_string(package.join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("add module"), "{changelog}");
    assert!(changelog.contains("rename module"), "{changelog}");
    // There is no release tag: the initial snapshot bounds the walk by package
    // equality. Reusing HEAD's file list would miss that boundary and include
    // the initial commit, whose message Repo::init sets to "add README".
    assert!(!changelog.contains("add README"), "{changelog}");
}

#[test]
fn update_detects_readme_changes_outside_the_package() {
    let (temp_dir, repo) = init_workspace(
        &[("one", "version = \"1.0.0\"\nreadme = \"../README.md\"\n")],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    fs_err::copy(
        repo.directory().join("README.md"),
        temp_dir.path().join("registry/README.md"),
    )
    .unwrap();
    run_workspace_update(&temp_dir, &repo, None);
    repo.is_clean().unwrap();

    // Cargo lists the external README under a generated package-relative name.
    // Package contents can compare equal, so the separate README check matters.
    fs_err::write(
        repo.directory().join("README.md"),
        "# Updated documentation\n",
    )
    .unwrap();
    repo.add_all_and_commit("fix: improve documentation")
        .unwrap();

    run_workspace_update(&temp_dir, &repo, None);

    assert_locked_versions(repo.directory(), &[("one", "1.0.1")]);
    let changelog = fs_err::read_to_string(repo.directory().join("one/CHANGELOG.md")).unwrap();
    assert!(changelog.contains("improve documentation"), "{changelog}");
    assert!(!changelog.contains("add README"), "{changelog}");
}

#[test]
fn update_workspace_with_detached_head() {
    update_detached_workspace(None);
}

#[test]
fn update_workspace_with_detached_head_and_explicit_repo_url() {
    update_detached_workspace(Some("https://github.com/test/explicit"));
}

fn update_detached_workspace(repo_url: Option<&str>) {
    let (temp_dir, repo) = init_workspace(
        &[
            ("one", "version = \"0.1.0\"\n"),
            ("two", "version = \"0.1.0\"\n"),
        ],
        "",
        "[workspace]\nsemver_check = false\n",
    );
    if repo_url.is_none() {
        repo.git(&["remote", "add", "origin", "https://github.com/test/project"])
            .unwrap();
    }
    // Model a colocated jj repository without requiring jj to be installed in CI.
    repo.git(&["checkout", "--detach"]).unwrap();
    // Both packages must be updated even though comparing the first walks older commits.
    for name in ["one", "two"] {
        fs_err::write(
            repo.directory().join(name).join("src/lib.rs"),
            format!("// Fix {name}\n"),
        )
        .unwrap();
        repo.add_all_and_commit(&format!("fix: update {name}"))
            .unwrap();
    }
    let original_commit = repo.current_commit_hash().unwrap();

    run_workspace_update(&temp_dir, &repo, repo_url);

    for name in ["one", "two"] {
        let package_dir = repo.directory().join(name);
        let manifest = fs_err::read_to_string(package_dir.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("version = \"0.1.1\""), "{manifest}");
        let changelog = fs_err::read_to_string(package_dir.join("CHANGELOG.md")).unwrap();
        assert!(changelog.contains(&format!("update {name}")), "{changelog}");
        let url = repo_url.unwrap_or("https://github.com/test/project");
        assert!(
            changelog.contains(&format!("{url}/compare/{name}-v0.1.0...{name}-v0.1.1")),
            "{changelog}"
        );
    }
    assert_eq!(repo.current_commit_hash().unwrap(), original_commit);
    assert!(repo.is_head_detached().unwrap());
}

#[test]
fn release_commits_leaves_filtered_workspace_unchanged() {
    let (temp_dir, repo) = workspace_with_feat_release_commits_filter(&[
        ("one", "version.workspace = true\n"),
        ("two", "version.workspace = true\n"),
    ]);
    change_package(&repo, "one", "fix: update one");
    change_package(&repo, "two", "chore: update two");

    run_workspace_update(&temp_dir, &repo, None);

    // Everything was committed before the update, so a clean repository proves
    // that neither Cargo.toml nor Cargo.lock was touched.
    repo.is_clean().unwrap();
    assert_locked_versions(repo.directory(), &[("one", "1.0.0"), ("two", "1.0.0")]);
}

#[test]
fn release_commits_preserves_shared_workspace_version_calculation() {
    let (temp_dir, repo) = workspace_with_feat_release_commits_filter(&[
        ("one", "version.workspace = true\n"),
        ("two", "version.workspace = true\n"),
    ]);
    change_package(&repo, "one", "feat: update one");
    change_package(&repo, "two", "fix!: update two");

    run_workspace_update(&temp_dir, &repo, None);

    // Changing the shared version also changes the filtered sibling's version,
    // so its breaking change still determines the workspace version.
    assert_locked_versions(repo.directory(), &[("one", "2.0.0"), ("two", "2.0.0")]);
    let changelog = fs_err::read_to_string(repo.directory().join("one/CHANGELOG.md")).unwrap();
    assert!(changelog.contains("## [2.0.0]"), "{changelog}");
}

#[test]
fn release_commits_does_not_bump_workspace_for_independent_release() {
    let (temp_dir, repo) = workspace_with_feat_release_commits_filter(&[
        ("one", "version.workspace = true\n"),
        ("two", "version = \"1.0.0\"\n"),
    ]);
    change_package(&repo, "one", "fix: update one");
    change_package(&repo, "two", "feat: update two");

    run_workspace_update(&temp_dir, &repo, None);

    // `one` inherits the workspace version, so it staying at 1.0.0 proves the
    // workspace version was not bumped.
    assert_locked_versions(repo.directory(), &[("one", "1.0.0"), ("two", "1.1.0")]);
    assert!(!repo.directory().join("one/CHANGELOG.md").exists());
    assert!(repo.directory().join("two/CHANGELOG.md").exists());
}

#[test]
fn release_commits_keeps_workspace_bump_for_dependency_updates() {
    let (temp_dir, repo) = workspace_with_feat_release_commits_filter(&[
        (
            "one",
            "version.workspace = true\n[dependencies]\ntwo = { path = \"../two\", version = \"=1.0.0\" }\n",
        ),
        ("two", "version = \"1.0.0\"\n"),
    ]);
    change_package(&repo, "one", "fix: update one");
    change_package(&repo, "two", "feat: update two");

    run_workspace_update(&temp_dir, &repo, None);

    // Although its own commits are filtered out, `one` must be updated because
    // its dependency requirement changes when `two` is released.
    assert_locked_versions(repo.directory(), &[("one", "1.0.1"), ("two", "1.1.0")]);
    let changelog = fs_err::read_to_string(repo.directory().join("one/CHANGELOG.md")).unwrap();
    assert!(changelog.contains("## [1.0.1]"), "{changelog}");
}

#[test]
fn dependency_updates_match_renamed_packages_among_unrelated_updates() {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.0\"\n"),
            ("unrelated", "version = \"1.0.0\"\n"),
            (
                "consumer",
                "version = \"1.0.0\"\n[target.'cfg(unix)'.build-dependencies]\nshared.workspace = true\n",
            ),
        ],
        "[workspace.dependencies]\nshared = { package = \"support\", path = \"support\", version = \"=1.0.0\" }\n",
        "[workspace]\nsemver_check = false\n",
    );
    change_package(&repo, "support", "feat: update support");
    change_package(&repo, "unrelated", "fix: update unrelated");

    run_workspace_update(&temp_dir, &repo, None);

    assert_locked_versions(
        repo.directory(),
        &[
            ("support", "1.1.0"),
            ("unrelated", "1.0.1"),
            ("consumer", "1.0.1"),
        ],
    );
    let changelog = fs_err::read_to_string(repo.directory().join("consumer/CHANGELOG.md")).unwrap();
    assert!(
        changelog.contains("updated the following local packages: support"),
        "{changelog}"
    );
    assert!(!changelog.contains("unrelated"), "{changelog}");
}

#[test]
fn dependency_updates_preserve_shared_changelog_entries_and_previous_versions() {
    // Both packages are released as 1.0.1, the version of the entry that support
    // has just added to the shared changelog.
    update_shared_changelog(
        "fix: update support",
        "",
        &[
            "/compare/support-v1.0.0...support-v1.0.1",
            "/compare/consumer-v1.0.0...consumer-v1.0.1",
        ],
    );
}

#[test]
fn dependency_updates_preserve_shared_changelog_previous_versions_when_versions_differ() {
    update_shared_changelog(
        "feat: update support",
        "",
        &[
            "/compare/support-v1.0.0...support-v1.1.0",
            "/compare/consumer-v1.0.0...consumer-v1.0.1",
        ],
    );
}

#[test]
fn dependency_updates_pass_previous_versions_to_shared_changelog_template() {
    update_shared_changelog(
        "fix: update support",
        PREVIOUS_VERSION_CHANGELOG_CONFIG,
        &[
            "## [1.0.1] support previous=1.0.0",
            "## [1.0.1] consumer previous=1.0.0",
        ],
    );
}

#[test]
fn dependency_updates_pass_previous_versions_to_shared_changelog_template_when_versions_differ() {
    update_shared_changelog(
        "feat: update support",
        PREVIOUS_VERSION_CHANGELOG_CONFIG,
        &[
            "## [1.1.0] support previous=1.0.0",
            "## [1.0.1] consumer previous=1.0.0",
        ],
    );
}

#[test]
fn shared_changelog_previous_version_of_bumped_package_is_its_registry_version() {
    let (temp_dir, repo) = released_workspace(
        &[
            ("published", "version.workspace = true\n"),
            ("unpublished", "version.workspace = true\n"),
        ],
        "\n[workspace.package]\nversion = \"1.0.0\"\n",
        &format!(
            "[workspace]\nsemver_check = false\nchangelog_path = \"CHANGELOG.md\"\n{PREVIOUS_VERSION_CHANGELOG_CONFIG}"
        ),
    );
    // Bump the workspace version, but only publish `published` at 1.1.0.
    let path = repo.directory().join("Cargo.toml");
    let manifest = fs_err::read_to_string(&path).unwrap();
    fs_err::write(path, manifest.replace("1.0.0", "1.1.0")).unwrap();
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: bump workspace version")
        .unwrap();
    repo.tag_lightweight("published-v1.1.0").unwrap();
    let registry_dir = temp_dir.path().join("registry");
    let path = registry_dir.join("published/Cargo.toml");
    let manifest = fs_err::read_to_string(&path).unwrap();
    fs_err::write(
        path,
        manifest.replace("version.workspace = true", "version = \"1.1.0\""),
    )
    .unwrap();
    generate_lockfile(&registry_dir);
    change_package(&repo, "published", "fix: update published");
    change_package(&repo, "unpublished", "fix: update unpublished");

    let summary = run_workspace_update(&temp_dir, &repo, None);

    assert!(
        summary.contains("`unpublished`: 1.0.0 -> 1.1.1"),
        "{summary}"
    );
    let changelog = fs_err::read_to_string(repo.directory().join("CHANGELOG.md")).unwrap();
    // `unpublished` never released 1.1.0, its local version.
    for line in [
        "## [1.1.1] published previous=1.1.0",
        "## [1.1.1] unpublished previous=1.0.0",
    ] {
        assert!(changelog.contains(line), "{line}: {changelog}");
    }
}

/// Renders the package name and its previous version in each release header.
const PREVIOUS_VERSION_CHANGELOG_CONFIG: &str = r#"
[changelog]
body = """
## [{{ version }}] {{ package }} previous={{ previous.version | default(value="none") }}
{% for commit in commits %}- {{ commit.message }}
{% endfor %}
"""
"#;

/// Releases `support` with `support_commit` and its dependent `consumer`, which
/// share a changelog, and checks that the changelog keeps the entries of both
/// packages and contains every `expected` line.
fn update_shared_changelog(support_commit: &str, changelog_config: &str, expected: &[&str]) {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.0\"\n"),
            (
                "consumer",
                "version = \"1.0.0\"\n[dependencies]\nsupport = { path = \"../support\", version = \"=1.0.0\" }\n",
            ),
        ],
        "",
        &format!(
            "[workspace]\nsemver_check = false\nchangelog_path = \"CHANGELOG.md\"\n{changelog_config}"
        ),
    );
    change_package(&repo, "support", support_commit);

    run_workspace_update(&temp_dir, &repo, None);

    let changelog = fs_err::read_to_string(repo.directory().join("CHANGELOG.md")).unwrap();
    assert!(changelog.contains("update support"), "{changelog}");
    assert!(
        changelog.contains("updated the following local packages: support"),
        "{changelog}"
    );
    for line in expected {
        assert!(changelog.contains(line), "{line}: {changelog}");
    }
}

#[test]
fn shared_changelog_keeps_entry_matching_another_package_release() {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.1\"\n"),
            (
                "consumer",
                "version = \"1.0.0\"\n[dependencies]\nsupport = { path = \"../support\", version = \"1.0.1\" }\n",
            ),
        ],
        "",
        "[workspace]\nsemver_check = false\nchangelog_path = \"CHANGELOG.md\"\n",
    );
    // The latest entry of the shared changelog belongs to `support`, but it has
    // the version that `consumer` is about to release.
    fs_err::write(
        repo.directory().join("CHANGELOG.md"),
        "# Changelog\n\n## [Unreleased]\n\n## [1.0.1] - 2026-01-01\n\n- release support\n",
    )
    .unwrap();
    repo.add_all_and_commit("chore: release").unwrap();
    change_package(&repo, "support", "fix: update support");
    change_package(&repo, "consumer", "fix: update consumer");

    let summary = run_workspace_update(&temp_dir, &repo, None);

    assert!(summary.contains("`consumer`: 1.0.0 -> 1.0.1"), "{summary}");
    let changelog = fs_err::read_to_string(repo.directory().join("CHANGELOG.md")).unwrap();
    for entry in ["update support", "update consumer", "release support"] {
        assert!(changelog.contains(entry), "{changelog}");
    }
}

#[test]
fn dependency_updates_propagate_through_the_shared_workspace_version() {
    update_through_shared_workspace_version("", None, "1.0.1", true);
}

#[test]
fn dependency_updates_propagate_through_a_release_commits_filtered_sibling() {
    update_through_shared_workspace_version("release_commits = \"^feat:\"\n", None, "1.0.1", false);
}

#[test]
fn dependency_updates_propagate_through_a_breaking_filtered_sibling() {
    // A filtered breaking change still determines the shared version once
    // the consumer needs a dependency-only release.
    let (_temp_dir, repo) = update_through_shared_workspace_version(
        "release_commits = \"^feat:\"\n",
        Some("fix!: update sibling"),
        "2.0.0",
        false,
    );
    let changelog = fs_err::read_to_string(repo.directory().join("consumer/CHANGELOG.md")).unwrap();
    assert!(!changelog.contains("## [1.0.1]"), "{changelog}");
}

#[test]
fn dependency_updates_propagate_through_a_release_disabled_sibling() {
    update_through_shared_workspace_version(
        "[[package]]\nname = \"sibling\"\nrelease = false\n",
        None,
        "1.0.1",
        false,
    );
}

/// Updates a workspace after a `feat:` commit to `support` and, if given,
/// `sibling_commit` to `sibling`. `consumer` depends on `support` and shares the
/// workspace version with `sibling`, which `downstream` depends on, so the
/// release of `support` must reach `downstream` even when `release_sibling` is
/// false.
fn update_through_shared_workspace_version(
    release_plz_toml: &str,
    sibling_commit: Option<&str>,
    shared_version: &str,
    release_sibling: bool,
) -> (Utf8TempDir, Repo) {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.0\"\n"),
            (
                "consumer",
                "version.workspace = true\n[dependencies]\nsupport = { path = \"../support\", version = \"=1.0.0\" }\n",
            ),
            ("sibling", "version.workspace = true\n"),
            (
                "downstream",
                "version = \"1.0.0\"\n[dependencies]\nsibling = { path = \"../sibling\", version = \"=1.0.0\" }\n",
            ),
        ],
        "\n[workspace.package]\nversion = \"1.0.0\"\n",
        &format!("[workspace]\nsemver_check = false\n{release_plz_toml}"),
    );
    change_package(&repo, "support", "feat: update support");
    if let Some(commit_message) = sibling_commit {
        change_package(&repo, "sibling", commit_message);
    }

    let summary = run_workspace_update(&temp_dir, &repo, None);

    assert_locked_versions(
        repo.directory(),
        &[
            ("support", "1.1.0"),
            ("consumer", shared_version),
            ("sibling", shared_version),
            ("downstream", "1.0.1"),
        ],
    );
    let mut released = vec![("consumer", shared_version), ("downstream", "1.0.1")];
    if release_sibling {
        released.push(("sibling", shared_version));
    } else {
        // Skipping the sibling's release doesn't stop its inherited version from
        // changing, which must still propagate to downstream.
        assert!(!repo.directory().join("sibling/CHANGELOG.md").exists());
        assert!(!summary.contains("`sibling`"), "{summary}");
    }
    for (name, version) in released {
        let changelog =
            fs_err::read_to_string(repo.directory().join(name).join("CHANGELOG.md")).unwrap();
        assert!(
            changelog.contains(&format!("## [{version}]")),
            "{name}: {changelog}"
        );
        assert!(
            summary.contains(&format!("`{name}`: 1.0.0 -> {version}")),
            "{summary}"
        );
    }
    (temp_dir, repo)
}

#[test]
fn dependency_updates_respect_prerelease_workspace_versions() {
    update_workspace_version_consumer("", "1.0.0-alpha.1", None, "1.0.0-alpha.2");
}

#[test]
fn dependency_updates_respect_pre_bumped_workspace_versions() {
    update_workspace_version_consumer("", "1.0.0", Some("1.1.0"), "1.1.0");
}

#[test]
fn dependency_updates_respect_pre_bumped_workspace_versions_with_release_commits_filter() {
    update_workspace_version_consumer(
        "release_commits = \"^feat:\"\n",
        "1.0.0",
        Some("1.1.0"),
        "1.1.0",
    );
}

/// Releases `support` in a workspace published at `published_version`, whose
/// `consumer` depends on `support` and inherits the workspace version, which is
/// manually set to `bumped_version` after the release, if any.
fn update_workspace_version_consumer(
    release_plz_toml: &str,
    published_version: &str,
    bumped_version: Option<&str>,
    next_version: &str,
) {
    let (temp_dir, repo) = released_workspace(
        &[
            ("support", "version = \"1.0.0\"\n"),
            (
                "consumer",
                "version.workspace = true\n[dependencies]\nsupport = { path = \"../support\", version = \"=1.0.0\" }\n",
            ),
        ],
        &format!("\n[workspace.package]\nversion = \"{published_version}\"\n"),
        &format!("[workspace]\nsemver_check = false\n{release_plz_toml}"),
    );
    if let Some(bumped_version) = bumped_version {
        let path = repo.directory().join("Cargo.toml");
        let manifest = fs_err::read_to_string(&path).unwrap();
        fs_err::write(path, manifest.replace(published_version, bumped_version)).unwrap();
        generate_lockfile(repo.directory());
        repo.add_all_and_commit("chore: bump workspace version")
            .unwrap();
    }
    change_package(&repo, "support", "feat: update support");

    let summary = run_workspace_update(&temp_dir, &repo, None);

    assert_locked_versions(
        repo.directory(),
        &[("consumer", next_version), ("support", "1.1.0")],
    );
    assert!(
        summary.contains(&format!(
            "`consumer`: {published_version} -> {next_version}"
        )),
        "{summary}"
    );
    let changelog = fs_err::read_to_string(repo.directory().join("consumer/CHANGELOG.md")).unwrap();
    assert!(
        changelog.contains(&format!("## [{next_version}]")),
        "{changelog}"
    );
}

/// Creates a workspace at `1.0.0` whose packages are already tagged as released
/// and whose config only treats `feat:` commits as release commits.
fn workspace_with_feat_release_commits_filter(packages: &[(&str, &str)]) -> (Utf8TempDir, Repo) {
    released_workspace(
        packages,
        "\n[workspace.package]\nversion = \"1.0.0\"\n",
        "[workspace]\nsemver_check = false\nrelease_commits = \"^feat:\"\n",
    )
}

/// A `git_only` package with a path dependency inside a submodule: the
/// reconstructed release needs the submodule's files, as the working copy has
/// them, to list the files of the released package.
#[test]
fn git_only_update_reconstructs_initialized_submodules() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    release_then_fix(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule path is used verbatim, so one with repeated spaces and glob
/// characters is still checked out in the release.
#[test]
fn git_only_update_reconstructs_submodules_at_unusual_paths() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(
        temp_dir.path(),
        "dep = { path = \"vendor/two  spaces[1]\" }",
    );
    add_submodule(&repo, dep.directory(), "vendor/two  spaces[1]");
    release_then_fix(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// The release's gitlink decides what is checked out, whatever update
/// procedure the working copy configures for the submodule.
#[test]
fn git_only_update_reconstructs_submodules_whose_update_is_none() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    repo.git(&["config", "submodule.vendor/dep.update", "none"])
        .unwrap();
    release_then_fix(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A nested submodule the working copy leaves uninitialized is not fetched,
/// so its repository being unreachable does not stop the update.
#[test]
fn git_only_update_skips_uninitialized_nested_submodules() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let inner_dir = temp_dir.path().join("inner");
    crate_repo(&inner_dir, "inner", "");
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    add_submodule(&dep, &inner_dir, "inner");
    dep.add_all_and_commit("add inner").unwrap();
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    fs_err::remove_dir_all(&inner_dir).unwrap();
    release_then_fix(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A nested submodule the working copy has checked out is checked out in the
/// release too, as a dependency can live in it.
#[test]
fn git_only_update_reconstructs_initialized_nested_submodules() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let inner_dir = temp_dir.path().join("inner");
    crate_repo(&inner_dir, "inner", "");
    let dep = crate_repo(
        &temp_dir.path().join("dep"),
        "dep",
        "[dependencies]\ninner = { path = \"inner\" }\n",
    );
    add_submodule(&dep, &inner_dir, "inner");
    dep.add_all_and_commit("add inner").unwrap();
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "update",
        "--init",
        "--recursive",
    ])
    .unwrap();
    release_then_fix(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule whose repository was replaced after the release is fetched
/// from the repository the release records, which has the released commit.
#[test]
fn git_only_update_reconstructs_submodules_from_the_released_repository() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let old = crate_repo(&temp_dir.path().join("old"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, old.directory(), "vendor/dep");
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    // An unrelated history: it lacks the released commit.
    let new = crate_repo(&temp_dir.path().join("new"), "dep", "");
    repo.git(&[
        "submodule",
        "set-url",
        "vendor/dep",
        new.directory().as_str(),
    ])
    .unwrap();
    let submodule = repo.directory().join("vendor/dep");
    git_cmd::git_in_dir(
        &submodule,
        &[
            ALLOW_FILE[0],
            ALLOW_FILE[1],
            "fetch",
            new.directory().as_str(),
        ],
    )
    .unwrap();
    git_cmd::git_in_dir(&submodule, &["checkout", "FETCH_HEAD"]).unwrap();
    repo.add_all_and_commit("chore: replace dep repository")
        .unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule moved after the release is the same submodule, under the
/// same name: it is checked out at the path the release records.
#[test]
fn git_only_update_reconstructs_moved_submodules() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    // `app` inherits the dependency, so moving it leaves `app`'s manifest as
    // released and its files are compared with the release's.
    let dir = temp_dir.path().join("project");
    fs_err::create_dir_all(dir.join("app/src")).unwrap();
    fs_err::write(
        dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n\
         [dependencies]\ndep = { workspace = true }\n",
    )
    .unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Initial release\n").unwrap();
    let workspace = |path: &str| {
        format!(
            "[workspace]\nmembers = [\"app\"]\nexclude = [\"vendor\", \"third\"]\nresolver = \"3\"\n\n\
             [workspace.dependencies]\ndep = {{ path = \"{path}\" }}\n"
        )
    };
    fs_err::write(dir.join("Cargo.toml"), workspace("vendor/dep")).unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    let repo = Repo::init(&dir);
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    fs_err::create_dir(dir.join("third")).unwrap();
    repo.git(&["mv", "vendor/dep", "third/dep"]).unwrap();
    fs_err::write(dir.join("Cargo.toml"), workspace("third/dep")).unwrap();
    repo.add_all_and_commit("chore: move dep").unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// A submodule removed after the release is still checked out in the release,
/// which can need it: here another workspace member depended on it.
#[test]
fn git_only_update_reconstructs_submodules_removed_since() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let dir = temp_dir.path().join("project");
    for (member, dependencies) in [("app", ""), ("b", "dep = { path = \"../vendor/dep\" }")] {
        fs_err::create_dir_all(dir.join(member).join("src")).unwrap();
        fs_err::write(
            dir.join(member).join("Cargo.toml"),
            format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 publish = false\n\n[dependencies]\n{dependencies}\n"
            ),
        )
        .unwrap();
        fs_err::write(dir.join(member).join("src/lib.rs"), "// Initial release\n").unwrap();
    }
    fs_err::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"b\"]\nexclude = [\"vendor\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    let repo = Repo::init(&dir);
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "vendor/dep"]).unwrap();
    fs_err::write(
        dir.join("b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n",
    )
    .unwrap();
    generate_lockfile(&dir);
    repo.add_all_and_commit("fix: drop dep").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.vendor/dep"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/vendor")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// A submodule removed after the release that nothing needs can be
/// unreachable: the update still succeeds.
#[test]
fn git_only_update_tolerates_unreachable_submodules_removed_since() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let extra_dir = temp_dir.path().join("extra");
    crate_repo(&extra_dir, "extra", "");
    let dir = temp_dir.path().join("project");
    fs_err::create_dir_all(dir.join("app/src")).unwrap();
    fs_err::write(
        dir.join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n",
    )
    .unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Initial release\n").unwrap();
    fs_err::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\nexclude = [\"extra\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    let repo = Repo::init(&dir);
    add_submodule(&repo, &extra_dir, "extra");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add extra").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();
    repo.git(&["rm", "extra"]).unwrap();
    repo.add_all_and_commit("chore: remove extra").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.extra"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/extra")).unwrap();
    fs_err::remove_dir_all(&extra_dir).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// The working copy already holds the submodule's history: it is checked out
/// from there, so an unreachable remote, or one needing credentials only the
/// working copy's own fetch had, does not matter.
#[test]
fn git_only_update_reconstructs_submodules_from_the_working_copy() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    release_then_fix(&repo);
    fs_err::remove_dir_all(dep.directory()).unwrap();

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule name can contain `=`, which a `name=value` configuration
/// argument would split at.
#[test]
fn git_only_update_reconstructs_submodules_whose_name_has_an_equals_sign() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "add",
        "--name",
        "a=b",
        dep.directory().as_str(),
        "vendor/dep",
    ])
    .unwrap();
    release_then_fix(&repo);
    fs_err::remove_dir_all(dep.directory()).unwrap();

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// Submodules removed since the release are fetched one by one: an
/// unreachable one does not stop another the release needs.
#[test]
fn git_only_update_isolates_failures_of_submodules_removed_since() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let extra_dir = temp_dir.path().join("extra");
    crate_repo(&extra_dir, "extra", "");
    let dir = temp_dir.path().join("project");
    for (member, dependencies) in [("app", ""), ("b", "dep = { path = \"../vendor/dep\" }")] {
        fs_err::create_dir_all(dir.join(member).join("src")).unwrap();
        fs_err::write(
            dir.join(member).join("Cargo.toml"),
            format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 publish = false\n\n[dependencies]\n{dependencies}\n"
            ),
        )
        .unwrap();
        fs_err::write(dir.join(member).join("src/lib.rs"), "// Initial release\n").unwrap();
    }
    fs_err::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"b\"]\nexclude = [\"vendor\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    let repo = Repo::init(&dir);
    // Sorted before `vendor/dep`, so a single command would try it first.
    add_submodule(&repo, &extra_dir, "vendor/a-extra");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add submodules").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "vendor/a-extra", "vendor/dep"]).unwrap();
    fs_err::write(
        dir.join("b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n",
    )
    .unwrap();
    generate_lockfile(&dir);
    repo.add_all_and_commit("fix: drop the submodules").unwrap();
    // As in a fresh clone, which has no trace of the removed submodules.
    for name in ["vendor/a-extra", "vendor/dep"] {
        repo.git(&["config", "--remove-section", &format!("submodule.{name}")])
            .unwrap();
    }
    fs_err::remove_dir_all(dir.join(".git/modules/vendor")).unwrap();
    fs_err::remove_dir_all(&extra_dir).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// Once a submodule's repository was replaced, the working copy's checkout
/// of it says nothing about the released repository's nested submodules:
/// one the release needs is fetched although the replacement leaves its
/// namesake uninitialized.
#[test]
fn git_only_update_reconstructs_nested_submodules_of_a_replaced_repository() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let inner_dir = temp_dir.path().join("inner");
    crate_repo(&inner_dir, "inner", "");
    let old = crate_repo(
        &temp_dir.path().join("old"),
        "dep",
        "[dependencies]\ninner = { path = \"inner\" }\n",
    );
    add_submodule(&old, &inner_dir, "inner");
    old.add_all_and_commit("add inner").unwrap();
    let new = crate_repo(&temp_dir.path().join("new"), "dep", "");
    add_submodule(&new, &inner_dir, "inner");
    new.add_all_and_commit("add inner").unwrap();

    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, old.directory(), "vendor/dep");
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "update",
        "--init",
        "--recursive",
    ])
    .unwrap();
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    repo.git(&[
        "submodule",
        "set-url",
        "vendor/dep",
        new.directory().as_str(),
    ])
    .unwrap();
    let submodule = repo.directory().join("vendor/dep");
    git_cmd::git_in_dir(&submodule, &["submodule", "deinit", "-f", "inner"]).unwrap();
    git_cmd::git_in_dir(
        &submodule,
        &[
            ALLOW_FILE[0],
            ALLOW_FILE[1],
            "fetch",
            new.directory().as_str(),
        ],
    )
    .unwrap();
    git_cmd::git_in_dir(&submodule, &["checkout", "FETCH_HEAD"]).unwrap();
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: replace dep repository")
        .unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule moved to a mirror of the same history is still checked out
/// from the working copy, which has the released commit, once the original
/// URL no longer answers.
#[test]
fn git_only_update_reconstructs_submodules_moved_to_a_mirror() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    let mirror = temp_dir.path().join("mirror");
    git_cmd::git_in_dir(
        temp_dir.path(),
        &["clone", "--bare", dep.directory().as_str(), mirror.as_str()],
    )
    .unwrap();
    repo.git(&["submodule", "set-url", "vendor/dep", mirror.as_str()])
        .unwrap();
    repo.add_all_and_commit("chore: fetch dep from its mirror")
        .unwrap();
    fs_err::remove_dir_all(dep.directory()).unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule whose `.gitmodules` section was renamed after the release is
/// the same repository at the same URL: it is still checked out from the
/// working copy once that URL no longer answers.
#[test]
fn git_only_update_reconstructs_submodules_renamed_since() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    repo.git(&[
        "config",
        "-f",
        ".gitmodules",
        "--rename-section",
        "submodule.vendor/dep",
        "submodule.dep",
    ])
    .unwrap();
    repo.add_all_and_commit("chore: rename dep").unwrap();
    fs_err::remove_dir_all(dep.directory()).unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule fetched from the working copy, after its URL moved to a
/// mirror, resolves the relative URLs of its own submodules against the
/// repository the release records, not against the checkout it was cloned
/// from.
#[test]
fn git_only_update_resolves_nested_urls_against_the_released_repository() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let repos = temp_dir.path().join("repos");
    crate_repo(&repos.join("inner"), "inner", "");
    let dep = crate_repo(
        &repos.join("dep"),
        "dep",
        "[dependencies]\ninner = { path = \"inner\" }\n",
    );
    dep.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "add",
        "../inner",
        "inner",
    ])
    .unwrap();
    dep.add_all_and_commit("add inner").unwrap();
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "update",
        "--init",
        "--recursive",
    ])
    .unwrap();
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    let mirror = temp_dir.path().join("mirrors/dep");
    git_cmd::git_in_dir(
        temp_dir.path(),
        &["clone", "--bare", dep.directory().as_str(), mirror.as_str()],
    )
    .unwrap();
    repo.git(&["submodule", "set-url", "vendor/dep", mirror.as_str()])
        .unwrap();
    repo.add_all_and_commit("chore: fetch dep from its mirror")
        .unwrap();
    fs_err::remove_dir_all(dep.directory()).unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// A submodule whose URL the working copy overrides locally with an
/// unrelated repository, its checkout cloned from there, is still fetched
/// from the URL the release records.
#[test]
fn git_only_update_reconstructs_submodules_whose_url_is_overridden() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let old = crate_repo(&temp_dir.path().join("old"), "dep", "");
    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, old.directory(), "vendor/dep");
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    // An unrelated history: it lacks the released commit.
    let new = crate_repo(&temp_dir.path().join("new"), "dep", "");
    repo.git(&["submodule", "deinit", "-f", "vendor/dep"])
        .unwrap();
    fs_err::remove_dir_all(repo.directory().join(".git/modules/vendor")).unwrap();
    repo.git(&[
        "config",
        "submodule.vendor/dep.url",
        new.directory().as_str(),
    ])
    .unwrap();
    git_cmd::git_in_dir(
        repo.directory(),
        &["clone", "-q", new.directory().as_str(), "vendor/dep"],
    )
    .unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// The checkout of a repository the working copy overrides a submodule's URL
/// with says nothing about the released repository's nested submodules: one
/// the release needs is fetched although the override leaves its namesake
/// uninitialized.
#[test]
fn git_only_update_reconstructs_nested_submodules_under_an_overridden_url() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let inner_dir = temp_dir.path().join("inner");
    crate_repo(&inner_dir, "inner", "");
    let old = crate_repo(
        &temp_dir.path().join("old"),
        "dep",
        "[dependencies]\ninner = { path = \"inner\" }\n",
    );
    add_submodule(&old, &inner_dir, "inner");
    old.add_all_and_commit("add inner").unwrap();
    let new = crate_repo(&temp_dir.path().join("new"), "dep", "");
    add_submodule(&new, &inner_dir, "inner");
    new.add_all_and_commit("add inner").unwrap();

    let repo = git_only_app(temp_dir.path(), "dep = { path = \"vendor/dep\" }");
    add_submodule(&repo, old.directory(), "vendor/dep");
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "update",
        "--init",
        "--recursive",
    ])
    .unwrap();
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();

    repo.git(&["submodule", "deinit", "-f", "vendor/dep"])
        .unwrap();
    fs_err::remove_dir_all(repo.directory().join(".git/modules/vendor")).unwrap();
    repo.git(&[
        "config",
        "submodule.vendor/dep.url",
        new.directory().as_str(),
    ])
    .unwrap();
    git_cmd::git_in_dir(
        repo.directory(),
        &["clone", "-q", new.directory().as_str(), "vendor/dep"],
    )
    .unwrap();
    change_app(&repo);

    assert_git_only_patch_release(&temp_dir, &repo);
}

/// Of two submodules of the release with one URL, the one removed since is
/// not mistaken for the other, which the working copy leaves uninitialized:
/// it is still fetched, as the release needs it.
#[test]
fn git_only_update_tells_apart_submodules_sharing_a_url() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let dir = released_with_member_b(temp_dir.path(), "../vendor/a");
    let repo = Repo::init(&dir);
    add_submodule(&repo, dep.directory(), "vendor/a");
    add_submodule(&repo, dep.directory(), "vendor/b");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add dep twice").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "vendor/a"]).unwrap();
    drop_dependencies_of_b(&dir);
    repo.add_all_and_commit("fix: drop vendor/a").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule, and
    // leaves the other uninitialized.
    repo.git(&["config", "--remove-section", "submodule.vendor/a"])
        .unwrap();
    repo.git(&["submodule", "deinit", "-f", "vendor/b"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/vendor")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// A `.gitmodules` section left behind once its gitlink was removed is not a
/// submodule of the working copy: the release's submodule of that name is
/// fetched as one removed since.
#[test]
fn git_only_update_ignores_sections_without_a_gitlink() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    let dir = released_with_member_b(temp_dir.path(), "../vendor/dep");
    let repo = Repo::init(&dir);
    add_submodule(&repo, dep.directory(), "vendor/dep");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "--cached", "-q", "vendor/dep"]).unwrap();
    fs_err::remove_dir_all(dir.join("vendor/dep")).unwrap();
    drop_dependencies_of_b(&dir);
    repo.add_all_and_commit("fix: drop dep, not its section")
        .unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.vendor/dep"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/vendor")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// A relative submodule URL resolved against a remote that is itself a
/// relative path names a repository relative to the working copy, not to the
/// worktree the release is checked out in.
#[test]
fn git_only_update_anchors_urls_relative_to_a_relative_remote() {
    let temp_dir = Utf8TempDir::new().unwrap();
    crate_repo(&temp_dir.path().join("remotes/dep"), "dep", "");
    let dir = released_with_member_b(temp_dir.path(), "../vendor/dep");
    let repo = Repo::init(&dir);
    repo.git(&["remote", "add", "origin", "../remotes/project"])
        .unwrap();
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "add",
        "../dep",
        "vendor/dep",
    ])
    .unwrap();
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add dep").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "vendor/dep"]).unwrap();
    drop_dependencies_of_b(&dir);
    repo.add_all_and_commit("fix: drop dep").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.vendor/dep"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/vendor")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// Git reports the URL an `insteadOf` rule rewrote a submodule URL to, which
/// can carry a token of its own: the warning about an unreachable submodule
/// withholds it.
#[test]
fn git_only_update_withholds_urls_rewritten_by_git() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let extra_dir = temp_dir.path().join("extra");
    crate_repo(&extra_dir, "extra", "");
    let dir = released_with_member_b(temp_dir.path(), "../vendor/dep");
    let repo = Repo::init(&dir);
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    add_submodule(&repo, &extra_dir, "extra");
    repo.git(&[
        "submodule",
        "set-url",
        "extra",
        "https://alias.invalid/extra",
    ])
    .unwrap();
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add submodules").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "extra"]).unwrap();
    repo.add_all_and_commit("chore: remove extra").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.extra"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/extra")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    let output = release_plz_cmd(&temp_dir.path().join("target"))
        .current_dir(repo.directory())
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .env(
            "GIT_CONFIG_KEY_1",
            "url.https://host.invalid/PATHSECRET/.insteadOf",
        )
        .env("GIT_CONFIG_VALUE_1", "https://alias.invalid/")
        .arg("update")
        .assert()
        .success()
        .get_output()
        .clone();
    let logs = String::from_utf8(output.stderr).unwrap();
    assert!(logs.contains("cannot check out submodule extra"), "{logs}");
    assert!(!logs.contains("PATHSECRET"), "{logs}");
}

/// A submodule removed after the release that nothing needs is optional all
/// the way down: one whose own submodule's URL cannot be resolved only warns.
#[test]
fn git_only_update_tolerates_unresolvable_nested_urls_of_removed_submodules() {
    let temp_dir = Utf8TempDir::new().unwrap();
    let extra_dir = temp_dir.path().join("extra");
    let extra = crate_repo(&extra_dir, "extra", "");
    // Climbs above the root of the URL `extra` is fetched from.
    let unresolvable = format!("{}inner", "../".repeat(64));
    fs_err::write(
        extra_dir.join(".gitmodules"),
        format!("[submodule \"inner\"]\n\tpath = inner\n\turl = {unresolvable}\n"),
    )
    .unwrap();
    let head = extra.git(&["rev-parse", "HEAD"]).unwrap();
    // An uninitialized submodule's directory, which keeps its gitlink staged.
    fs_err::create_dir(extra_dir.join("inner")).unwrap();
    extra
        .git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},inner", head.trim()),
        ])
        .unwrap();
    extra.add_all_and_commit("add inner").unwrap();
    let dir = released_with_member_b(temp_dir.path(), "../vendor/dep");
    let repo = Repo::init(&dir);
    let dep = crate_repo(&temp_dir.path().join("dep"), "dep", "");
    add_submodule(&repo, dep.directory(), "vendor/dep");
    add_submodule(&repo, &extra_dir, "extra");
    generate_lockfile(&dir);
    repo.add_all_and_commit("chore: add submodules").unwrap();
    repo.tag_lightweight("app-v0.1.0").unwrap();
    repo.tag_lightweight("b-v0.1.0").unwrap();

    repo.git(&["rm", "extra"]).unwrap();
    repo.add_all_and_commit("chore: remove extra").unwrap();
    // As in a fresh clone, which has no trace of the removed submodule.
    repo.git(&["config", "--remove-section", "submodule.extra"])
        .unwrap();
    fs_err::remove_dir_all(dir.join(".git/modules/extra")).unwrap();
    fs_err::write(dir.join("app/src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();

    assert_git_only_patch_release_of(&temp_dir, &repo, "app/Cargo.toml");
}

/// A `git_only` workspace in `<parent>/project` with members `app` and `b`,
/// `b` depending on the crate at `dependency`, and `vendor` excluded.
fn released_with_member_b(parent: &Utf8Path, dependency: &str) -> Utf8PathBuf {
    let dir = parent.join("project");
    for (member, dependencies) in [
        ("app", String::new()),
        ("b", format!("dep = {{ path = \"{dependency}\" }}")),
    ] {
        fs_err::create_dir_all(dir.join(member).join("src")).unwrap();
        fs_err::write(
            dir.join(member).join("Cargo.toml"),
            format!(
                "[package]\nname = \"{member}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 publish = false\n\n[dependencies]\n{dependencies}\n"
            ),
        )
        .unwrap();
        fs_err::write(dir.join(member).join("src/lib.rs"), "// Initial release\n").unwrap();
    }
    fs_err::write(
        dir.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"b\"]\nexclude = [\"vendor\"]\nresolver = \"3\"\n",
    )
    .unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    dir
}

/// Member `b` of [`released_with_member_b`] no longer depends on anything.
fn drop_dependencies_of_b(dir: &Utf8Path) {
    fs_err::write(
        dir.join("b/Cargo.toml"),
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n",
    )
    .unwrap();
    generate_lockfile(dir);
}

/// Git configuration for the submodules of these tests, whose URLs are local
/// paths that git refuses to clone by default.
const ALLOW_FILE: [&str; 2] = ["-c", "protocol.file.allow=always"];

/// Crate `name` in its own repository at `dir`, with `manifest` appended to
/// its `Cargo.toml`.
fn crate_repo(dir: &Utf8Path, name: &str, manifest: &str) -> Repo {
    fs_err::create_dir_all(dir.join("src")).unwrap();
    fs_err::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"1.0.0\"\nedition = \"2024\"\n{manifest}"
        ),
    )
    .unwrap();
    fs_err::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    Repo::init(dir)
}

/// A `git_only` project in `<parent>/project` whose package `app` has the
/// given `dependencies`.
fn git_only_app(parent: &Utf8Path, dependencies: &str) -> Repo {
    let dir = parent.join("project");
    fs_err::create_dir_all(dir.join("src")).unwrap();
    fs_err::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\npublish = false\n\n\
             [dependencies]\n{dependencies}\n"
        ),
    )
    .unwrap();
    fs_err::write(dir.join("src/lib.rs"), "// Initial release\n").unwrap();
    fs_err::write(
        dir.join("release-plz.toml"),
        "[workspace]\ngit_only = true\npublish = false\nsemver_check = false\n",
    )
    .unwrap();
    Repo::init(&dir)
}

fn add_submodule(repo: &Repo, url: &Utf8Path, path: &str) {
    repo.git(&[
        ALLOW_FILE[0],
        ALLOW_FILE[1],
        "submodule",
        "add",
        url.as_str(),
        path,
    ])
    .unwrap();
}

/// Tag the project as released at 0.1.0, then commit a fix to `app`.
fn release_then_fix(repo: &Repo) {
    generate_lockfile(repo.directory());
    repo.add_all_and_commit("chore: add dependencies").unwrap();
    repo.tag_lightweight("v0.1.0").unwrap();
    change_app(repo);
}

fn change_app(repo: &Repo) {
    fs_err::write(repo.directory().join("src/lib.rs"), "// Fixed\n").unwrap();
    repo.add_all_and_commit("fix: update app").unwrap();
}

/// `release-plz update` bumps `app` to 0.1.1.
fn assert_git_only_patch_release(temp_dir: &Utf8TempDir, repo: &Repo) {
    assert_git_only_patch_release_of(temp_dir, repo, "Cargo.toml");
}

/// `release-plz update` bumps the package of `manifest` to 0.1.1.
fn assert_git_only_patch_release_of(temp_dir: &Utf8TempDir, repo: &Repo, manifest: &str) {
    release_plz_cmd(&temp_dir.path().join("target"))
        .current_dir(repo.directory())
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .arg("update")
        .assert()
        .success();

    let manifest = fs_err::read_to_string(repo.directory().join(manifest)).unwrap();
    assert!(manifest.contains("version = \"0.1.1\""), "{manifest}");
}

/// Creates a workspace with [`init_workspace`] whose packages are already tagged
/// as released at their current version, and whose `origin` remote is on GitHub.
fn released_workspace(
    packages: &[(&str, &str)],
    workspace_package_toml: &str,
    release_plz_toml: &str,
) -> (Utf8TempDir, Repo) {
    let (temp_dir, repo) = init_workspace(packages, workspace_package_toml, release_plz_toml);
    repo.git(&["remote", "add", "origin", "https://github.com/test/project"])
        .unwrap();
    for package in locked_metadata(repo.directory()).workspace_packages() {
        repo.tag_lightweight(&format!("{}-v{}", package.name, package.version))
            .unwrap();
    }
    (temp_dir, repo)
}

/// Creates a `project` workspace with its git repository and a matching `registry`
/// workspace, which models the published versions.
///
/// `packages` maps each package name to the manifest lines appended after
/// `[package] name/edition`, and `workspace_package_toml` is appended to the root
/// `Cargo.toml`.
fn init_workspace(
    packages: &[(&str, &str)],
    workspace_package_toml: &str,
    release_plz_toml: &str,
) -> (Utf8TempDir, Repo) {
    let temp_dir = Utf8TempDir::new().unwrap();
    let project_dir = temp_dir.path().join("project");
    let registry_dir = temp_dir.path().join("registry");
    let members: Vec<_> = packages.iter().map(|(name, _)| name).collect();
    for dir in [&project_dir, &registry_dir] {
        fs_err::create_dir(dir).unwrap();
        fs_err::write(
            dir.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = {members:?}\nresolver = \"3\"\n{workspace_package_toml}"
            ),
        )
        .unwrap();
        for (name, manifest) in packages {
            let package_dir = dir.join(name);
            fs_err::create_dir_all(package_dir.join("src")).unwrap();
            fs_err::write(
                package_dir.join("Cargo.toml"),
                format!("[package]\nname = {name:?}\nedition = \"2024\"\n{manifest}"),
            )
            .unwrap();
            fs_err::write(package_dir.join("src/lib.rs"), "// Initial release\n").unwrap();
        }
        generate_lockfile(dir);
    }
    fs_err::write(project_dir.join("release-plz.toml"), release_plz_toml).unwrap();
    let repo = Repo::init(&project_dir);
    (temp_dir, repo)
}

fn generate_lockfile(dir: &Utf8Path) {
    assert_cmd::Command::new("cargo")
        .current_dir(dir)
        .args(["generate-lockfile", "--offline"])
        .assert()
        .success();
}

/// Commit `commit_message` with a change to the Rust source of package `name`.
/// The source contains the message, so commits with distinct messages always
/// change it, even for the same package.
fn change_package(repo: &Repo, name: &str, commit_message: &str) {
    fs_err::write(
        repo.directory().join(name).join("src/lib.rs"),
        format!("// {commit_message}\n"),
    )
    .unwrap();
    repo.add_all_and_commit(commit_message).unwrap();
}

fn run_workspace_update(temp_dir: &Utf8TempDir, repo: &Repo, repo_url: Option<&str>) -> String {
    let mut cmd = workspace_update_command(temp_dir, repo);
    if let Some(url) = repo_url {
        cmd.args(["--repo-url", url]);
    }
    let output = cmd.assert().success().get_output().stdout.clone();
    String::from_utf8(output).unwrap()
}

/// Run [`workspace_update_command`] and return its debug logs.
fn run_workspace_update_with_logs(temp_dir: &Utf8TempDir, repo: &Repo) -> String {
    let output = workspace_update_command(temp_dir, repo)
        .env("RELEASE_PLZ_LOG", "release_plz_core=debug")
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

/// `release-plz update` for a workspace created by [`init_workspace`].
fn workspace_update_command(temp_dir: &Utf8TempDir, repo: &Repo) -> assert_cmd::Command {
    let mut cmd = release_plz_cmd(&temp_dir.path().join("target"));
    cmd.current_dir(repo.directory())
        .args(["update", "--registry-manifest-path"])
        .arg(temp_dir.path().join("registry/Cargo.toml"));
    cmd
}
