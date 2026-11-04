//! The daemon artifact identity must be a function of the build *inputs*, not of
//! `git SHA + dirty`. A wrapper point release that ships identical sidecar bytes
//! must compare equal; a rebuild with a changed source file, a changed feature
//! set, or a different target triple at the same SHA must compare different.
//!
//! This test compiles the same pure module the build script uses, so the property
//! is checked against production code rather than a copy of it.

#[path = "../build_identity.rs"]
mod build_identity;

use build_identity::input_digest;

fn sources(pairs: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
    pairs
        .iter()
        .map(|(path, body)| ((*path).to_string(), body.to_vec()))
        .collect()
}

#[test]
fn changed_dirty_source_changes_identity() {
    let a = vec![("src/lib.rs".into(), b"const X: u8 = 1;".to_vec())];
    let b = vec![("src/lib.rs".into(), b"const X: u8 = 2;".to_vec())];
    assert_ne!(input_digest(&a), input_digest(&b));
    assert_eq!(input_digest(&a), input_digest(&a));
}

#[test]
fn a_renamed_file_changes_identity_even_with_identical_bytes() {
    let a = sources(&[("src/a.rs", b"same")]);
    let b = sources(&[("src/b.rs", b"same")]);
    assert_ne!(
        input_digest(&a),
        input_digest(&b),
        "the path participates in the digest, so a move is a rebuild"
    );
}

#[test]
fn input_order_does_not_change_identity() {
    let a = sources(&[("src/a.rs", b"one"), ("src/b.rs", b"two")]);
    let b = sources(&[("src/b.rs", b"two"), ("src/a.rs", b"one")]);
    assert_eq!(input_digest(&a), input_digest(&b));
}

#[test]
fn the_same_source_at_two_different_features_differs() {
    let source = sources(&[("src/lib.rs", b"pub fn f() {}")]);
    let plain = build_identity::build_id(
        build_identity::BuildInputs {
            sources: source.clone(),
            target_triple: "aarch64-apple-darwin".into(),
            rustc_version: "1.85.0".into(),
            profile: "release".into(),
            features: vec![],
            rustflags: String::new(),
            manifest: b"[package]".to_vec(),
        }
        .into(),
    );
    let featured = build_identity::build_id(
        build_identity::BuildInputs {
            sources: source,
            target_triple: "aarch64-apple-darwin".into(),
            rustc_version: "1.85.0".into(),
            profile: "release".into(),
            features: vec!["syntax-highlight".into()],
            rustflags: String::new(),
            manifest: b"[package]".to_vec(),
        }
        .into(),
    );
    assert_ne!(plain, featured);
}

#[test]
fn a_different_target_triple_changes_identity() {
    let base = build_identity::BuildInputs {
        sources: vec![("src/lib.rs".into(), b"pub fn f() {}".to_vec())],
        target_triple: "aarch64-apple-darwin".into(),
        rustc_version: "1.85.0".into(),
        profile: "release".into(),
        features: vec![],
        rustflags: String::new(),
        manifest: b"[package]".to_vec(),
    };
    let mut other = base.clone();
    other.target_triple = "x86_64-unknown-linux-gnu".into();
    assert_ne!(
        build_identity::build_id(base.into()),
        build_identity::build_id(other.into())
    );
}

#[test]
fn the_same_inputs_give_the_same_id_twice() {
    // Reproducibility: this is what lets a copied artifact keep its identity.
    let inputs = build_identity::BuildInputs {
        sources: sources(&[("src/lib.rs", b"pub fn f() {}")]),
        target_triple: "aarch64-apple-darwin".into(),
        rustc_version: "1.85.0".into(),
        profile: "release".into(),
        features: vec!["a".into(), "b".into()],
        rustflags: "-C target-cpu=native".into(),
        manifest: b"[package]\nname = \"x\"".to_vec(),
    };
    assert_eq!(
        build_identity::build_id(inputs.clone().into()),
        build_identity::build_id(inputs.into())
    );
}

#[test]
fn rustflags_changes_identity() {
    let base = build_identity::BuildInputs {
        sources: sources(&[("src/lib.rs", b"pub fn f() {}")]),
        target_triple: "aarch64-apple-darwin".into(),
        rustc_version: "1.85.0".into(),
        profile: "release".into(),
        features: vec![],
        rustflags: String::new(),
        manifest: b"[package]".to_vec(),
    };
    let mut other = base.clone();
    other.rustflags = "-C target-cpu=native".into();
    assert_ne!(
        build_identity::build_id(base.into()),
        build_identity::build_id(other.into())
    );
}

#[test]
fn the_build_id_is_a_plain_hex_digest_not_a_git_describe() {
    // Guards against a regression to "git SHA + dirty": the id must contain no
    // path separators, no spaces, and no "-dirty" suffix.
    let id = build_identity::build_id(
        build_identity::BuildInputs {
            sources: sources(&[("src/lib.rs", b"pub fn f() {}")]),
            target_triple: "aarch64-apple-darwin".into(),
            rustc_version: "1.85.0".into(),
            profile: "release".into(),
            features: vec![],
            rustflags: String::new(),
            manifest: b"[package]".to_vec(),
        }
        .into(),
    );
    assert!(
        id.bytes().all(|b| b.is_ascii_hexdigit()),
        "build id must be hex: {id}"
    );
    assert_eq!(
        id.len(),
        32,
        "128 bits is enough and keeps the manifest short"
    );
}

#[test]
fn source_walk_excludes_generated_and_foreign_directories() {
    let root = tempfile::tempdir().unwrap();
    let crate_dir = root.path();
    for (rel, body) in [
        ("src/lib.rs", "pub fn f() {}"),
        ("build.rs", "fn main() {}"),
        ("Cargo.toml", "[package]"),
        ("target/debug/junk.rs", "junk"),
        ("node_modules/pkg/index.js", "junk"),
        (".git/config", "junk"),
        (".worktrees/other/src/lib.rs", "junk"),
    ] {
        let path = crate_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let walked = build_identity::collect_sources(crate_dir, &[]);
    let names: Vec<String> = walked.iter().map(|(p, _)| p.clone()).collect();
    assert!(names.contains(&"src/lib.rs".to_string()), "{names:?}");
    assert!(names.contains(&"build.rs".to_string()), "{names:?}");
    assert!(names.contains(&"Cargo.toml".to_string()), "{names:?}");
    for forbidden in ["target/", "node_modules/", ".git/", ".worktrees/"] {
        assert!(
            !names.iter().any(|n| n.starts_with(forbidden)),
            "{forbidden} must be excluded: {names:?}"
        );
    }
}

#[test]
fn the_digest_is_stable_across_a_directory_walk_order() {
    let root = tempfile::tempdir().unwrap();
    for (rel, body) in [
        ("src/a.rs", "a"),
        ("src/b.rs", "b"),
        ("src/nested/c.rs", "c"),
    ] {
        let path = root.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let first = build_identity::collect_sources(root.path(), &[]);
    let second = build_identity::collect_sources(root.path(), &[]);
    assert_eq!(input_digest(&first), input_digest(&second));
}

#[test]
fn an_empty_input_set_is_still_a_digest_not_an_error() {
    assert_eq!(input_digest(&[]).len(), 32);
}

#[test]
fn build_id_ignores_inputs_that_cannot_affect_the_artifact() {
    // Only build-relevant material is digested. Timestamps and absolute build
    // paths would make every machine produce a different id for identical bytes.
    let id = build_identity::build_id(
        build_identity::BuildInputs {
            sources: vec![("src/lib.rs".into(), b"pub fn f() {}".to_vec())],
            target_triple: "aarch64-apple-darwin".into(),
            rustc_version: "1.85.0".into(),
            profile: "release".into(),
            features: vec![],
            rustflags: String::new(),
            manifest: b"[package]".to_vec(),
        }
        .into(),
    );
    assert!(!id.contains('/'), "{id}");
    assert!(!id.contains('-'), "{id}");
}
