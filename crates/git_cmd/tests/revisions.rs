use camino::Utf8Path;
use git_cmd::Repo;
use tempfile::tempdir;

/// Missing positive revisions must not look like a successful empty parent walk.
#[test]
fn a_parent_walk_rejects_a_missing_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    assert!(
        repo.parents_at_paths("missing-branch", &[], &[Utf8Path::new("README.md")], None)
            .is_err()
    );
}

/// The convenience method must preserve the same invalid-tip error.
#[test]
fn a_commit_walk_rejects_a_missing_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    assert!(
        repo.commits_at_paths("missing-branch", &[], &[Utf8Path::new("README.md")], None)
            .is_err()
    );
}

/// Full-history traversal must not hide a missing positive revision either.
#[test]
fn an_ancestor_walk_rejects_a_missing_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    assert!(
        repo.ancestors_at_paths("missing-branch", &[], &[Utf8Path::new("README.md")])
            .is_err()
    );
}

/// Ignoring missing exclusions must not allow the tip to become a Git option.
#[test]
fn a_walk_rejects_an_option_as_its_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    assert!(
        repo.parents_at_paths("--all", &[], &[Utf8Path::new("README.md")], None)
            .is_err()
    );
}

/// Valid branch tips still work with boundaries absent from the local clone.
#[test]
fn a_valid_tip_still_ignores_missing_exclusions() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    let tip = repo.current_commit_hash().unwrap();
    let paths = [Utf8Path::new("README.md")];
    let exclusions = ["missing-boundary"];
    assert_eq!(
        repo.commits_at_paths("HEAD", &exclusions, &paths, None)
            .unwrap(),
        std::slice::from_ref(&tip)
    );
    assert_eq!(
        repo.ancestors_at_paths("HEAD", &exclusions, &paths)
            .unwrap(),
        [tip]
    );
}

/// Resolving a tip must preserve annotated-tag support by peeling it to a commit.
#[test]
fn an_annotated_tag_remains_a_valid_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    let tip = repo.current_commit_hash().unwrap();
    repo.git(&["tag", "--no-sign", "-a", "release", "-m", "release"])
        .unwrap();
    assert_eq!(
        repo.commits_at_paths("release", &[], &[Utf8Path::new("README.md")], None)
            .unwrap(),
        [tip]
    );
}

/// An existing object that is not a commit must not be accepted as a walk's tip.
#[test]
fn a_blob_is_not_a_valid_tip() {
    let directory = tempdir().unwrap();
    let repo = Repo::init(&directory);
    let blob = repo.git(&["rev-parse", "HEAD:README.md"]).unwrap();
    assert!(
        repo.ancestors_at_paths(&blob, &[], &[Utf8Path::new("README.md")])
            .is_err()
    );
}
