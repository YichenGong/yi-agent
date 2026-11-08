//! Locate and verify the bundled runtime sidecar manifest.
//!
//! The desktop shell must never run a daemon it cannot identify. `finalize-runtime-bundle.sh`
//! writes `Resources/runtime-sidecar.json` after the sidecar is signed, containing
//! the daemon's build id and the sha256 of the final bytes. This module resolves
//! that manifest relative to the *running bundle*, so a copied app still finds its
//! own sidecar, and refuses to guess when it cannot.
//!
//! The expected build id is embedded at compile time by `build.rs` from the same
//! `internal-runtime identity` output the manifest is generated from. Comparing the
//! two is what catches "the app was rebuilt but the sidecar was not" — the failure
//! mode that would otherwise silently pair a new wrapper with an old daemon.

// The manifest, its verifier, and the dev fallback are reachable only from the
// lease worker added in Task 13; until then nothing in the shell calls them and
// clippy fails the crate on dead code. Same convention as `remote::config::save`.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The manifest's shape. Mirrors `yi_agent_lifecycle::types::BundledArtifactManifest`
/// field for field; kept as a local struct because the backend crate cannot depend
/// on the workspace's lifecycle crate without also taking its whole dependency
/// graph, and this struct carries no logic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundledArtifactManifest {
    pub build_id: String,
    pub display_version: String,
    pub executable: String,
    pub sha256: String,
    pub schema_min: u32,
    pub schema_max: u32,
    pub handover_version: u16,
    pub business_protocol_min: u32,
    pub business_protocol_max: u32,
    pub capabilities: Vec<String>,
}

/// What the shell learned about its own daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeArtifact {
    /// A manifest was found and every check passed.
    Verified {
        manifest: BundledArtifactManifest,
        executable: PathBuf,
    },
    /// Something about the identity could not be established. Carries a reason for
    /// the UI to show; the shell must disable automatic switching in this state
    /// rather than fall back to a wrapper version number.
    Unknown { reason: String },
}

impl RuntimeArtifact {
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

/// The build id this shell was compiled against.
pub const EXPECTED_BUILD_ID: &str = env!("YI_AGENT_EXPECTED_RUNTIME_BUILD_ID");

/// Where the manifest lives inside a bundle root.
pub const MANIFEST_RESOURCE: &str = "Contents/Resources/runtime-sidecar.json";

/// Resolve and verify the sidecar for `bundle_root` (the directory containing
/// `Contents/`, i.e. `Foo.app`).
pub fn verify_bundle(bundle_root: &Path) -> RuntimeArtifact {
    let manifest_path = bundle_root.join(MANIFEST_RESOURCE);
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RuntimeArtifact::Unknown {
                reason: format!(
                    "no runtime manifest at {}: {error}",
                    manifest_path.display()
                ),
            };
        }
    };
    let manifest: BundledArtifactManifest = match serde_json::from_slice(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            return RuntimeArtifact::Unknown {
                reason: format!("unreadable runtime manifest: {error}"),
            };
        }
    };
    verify_in(bundle_root, manifest)
}

/// The bundle root that owns a macOS `Resources` directory.
///
/// Tauri's `resource_dir()` resolves to `exe_dir/../Resources`, i.e.
/// `<Foo.app>/Contents/Resources` — one level deeper than the `Contents`
/// directory the previous rule assumed. Stopping at `Contents` made every
/// lookup ask for `Contents/Contents/Resources/runtime-sidecar.json`, so a
/// perfectly good bundle was reported as unverifiable and the shell sent no
/// `expected_runtime` at all.
///
/// The parent's name is checked rather than counted: a dev build's cargo output
/// has a `Resources`-like directory with no `Contents` above it, and inventing
/// `<somewhere>` from it would name a bundle root that cannot exist.
pub fn bundle_root_from_resources(resources: &Path) -> Option<PathBuf> {
    let contents = resources.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    contents.parent().map(Path::to_path_buf)
}

/// The verification itself, split from file reading so tests can drive it with a
/// real binary in a temp directory.
pub fn verify_in(bundle_root: &Path, manifest: BundledArtifactManifest) -> RuntimeArtifact {
    // Relative paths only: an absolute path from the build machine would point
    // outside the app the user actually installed.
    if Path::new(&manifest.executable).is_absolute() {
        return RuntimeArtifact::Unknown {
            reason: "manifest executable path is absolute; it must be bundle-relative".into(),
        };
    }
    let executable = bundle_root.join(&manifest.executable);
    let bytes = match std::fs::read(&executable) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RuntimeArtifact::Unknown {
                reason: format!("sidecar missing at {}: {error}", executable.display()),
            };
        }
    };
    let actual = hex(&Sha256::digest(&bytes));
    if actual != manifest.sha256 {
        return RuntimeArtifact::Unknown {
            reason: format!(
                "sidecar digest mismatch: manifest says {}, the file is {actual}",
                manifest.sha256
            ),
        };
    }
    // The wrapper and the sidecar are built by one pipeline; a mismatch means one
    // of them was replaced, and pairing them would be a silent downgrade.
    if !EXPECTED_BUILD_ID.is_empty() && manifest.build_id != EXPECTED_BUILD_ID {
        return RuntimeArtifact::Unknown {
            reason: format!(
                "sidecar build {} does not match this app's expected build {}",
                manifest.build_id, EXPECTED_BUILD_ID
            ),
        };
    }
    RuntimeArtifact::Verified {
        manifest,
        executable,
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A dev-mode artifact: no bundle, no manifest, so identity is explicitly unknown.
///
/// `tauri dev` never produces a bundle, and pretending a dev sidecar has a verified
/// identity would let a developer enable a switch that then fails in production.
pub fn dev_artifact() -> RuntimeArtifact {
    RuntimeArtifact::Unknown {
        reason: "development build: no bundled runtime manifest, automatic switching disabled"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_sidecar(dir: &Path, bytes: &[u8]) -> String {
        let path = dir.join("Contents/MacOS/yi-agent");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        hex(&Sha256::digest(bytes))
    }

    fn manifest_for(digest: &str, build_id: &str) -> BundledArtifactManifest {
        BundledArtifactManifest {
            build_id: build_id.into(),
            display_version: "0.1.13".into(),
            executable: "Contents/MacOS/yi-agent".into(),
            sha256: digest.into(),
            schema_min: 0,
            schema_max: 15,
            handover_version: 1,
            business_protocol_min: 2,
            business_protocol_max: 2,
            capabilities: vec!["control.v1".into()],
        }
    }

    #[test]
    fn a_matching_manifest_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let digest = write_sidecar(dir.path(), b"daemon bytes");
        let artifact = verify_in(dir.path(), manifest_for(&digest, EXPECTED_BUILD_ID));
        assert!(artifact.is_verified(), "{artifact:?}");
    }

    #[test]
    fn a_relocated_bundle_resolves_its_relative_sidecar() {
        // Copying the whole app elsewhere must keep working: the manifest holds a
        // relative path, so nothing points back at the build directory.
        let original = tempfile::tempdir().unwrap();
        let digest = write_sidecar(original.path(), b"daemon bytes");
        let moved = tempfile::tempdir().unwrap();
        let target = moved.path().join("Contents/MacOS/yi-agent");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(original.path().join("Contents/MacOS/yi-agent"), &target).unwrap();
        let artifact = verify_in(moved.path(), manifest_for(&digest, EXPECTED_BUILD_ID));
        assert!(artifact.is_verified(), "{artifact:?}");
    }

    #[test]
    fn a_replaced_sidecar_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let digest = write_sidecar(dir.path(), b"the original daemon");
        // Swap the binary after the manifest was written, as a tampered or
        // half-updated bundle would.
        std::fs::write(
            dir.path().join("Contents/MacOS/yi-agent"),
            b"a different daemon",
        )
        .unwrap();
        let artifact = verify_in(dir.path(), manifest_for(&digest, EXPECTED_BUILD_ID));
        match artifact {
            RuntimeArtifact::Unknown { reason } => assert!(reason.contains("digest"), "{reason}"),
            other => panic!("expected a digest rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_mismatched_build_id_is_rejected_even_with_matching_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let digest = write_sidecar(dir.path(), b"daemon bytes");
        let artifact = verify_in(dir.path(), manifest_for(&digest, "someotherbuildid"));
        assert!(!artifact.is_verified(), "{artifact:?}");
    }

    #[test]
    fn an_absolute_executable_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let digest = write_sidecar(dir.path(), b"daemon bytes");
        let mut manifest = manifest_for(&digest, EXPECTED_BUILD_ID);
        manifest.executable = "/Users/someone/build/target/debug/yi-agent".into();
        let artifact = verify_in(dir.path(), manifest);
        match artifact {
            RuntimeArtifact::Unknown { reason } => assert!(reason.contains("absolute"), "{reason}"),
            other => panic!("expected an absolute-path rejection, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_manifest_is_unknown_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!verify_bundle(dir.path()).is_verified());
    }

    #[test]
    fn a_corrupt_manifest_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MANIFEST_RESOURCE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(!verify_bundle(dir.path()).is_verified());
    }

    #[test]
    fn the_manifest_round_trips_through_json() {
        let manifest = manifest_for("abc", "build");
        let text = serde_json::to_string(&manifest).unwrap();
        assert_eq!(
            serde_json::from_str::<BundledArtifactManifest>(&text).unwrap(),
            manifest
        );
    }

    #[test]
    fn a_dev_build_reports_unknown_instead_of_guessing() {
        assert!(!dev_artifact().is_verified());
    }

    /// macOS `resource_dir()` returns `<Foo.app>/Contents/Resources`, but a
    /// manifest is resolved from the *bundle root*. One `parent()` stops at
    /// `Contents`, so every lookup then asked for
    /// `Contents/Contents/Resources/runtime-sidecar.json` and reported the
    /// runtime as unverifiable.
    #[test]
    fn a_macos_resources_dir_resolves_to_its_bundle_root() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Foo.app");
        let resources = bundle.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();

        assert_eq!(bundle_root_from_resources(&resources), Some(bundle));
    }

    /// A `Resources` directory with no `Contents` above it (a dev build's cargo
    /// output) must yield no bundle root rather than a path that cannot exist.
    /// `verify_bundle` then answers `Unknown`, which is the honest dev answer.
    #[test]
    fn a_resources_dir_without_contents_has_no_bundle_root() {
        let dir = tempfile::tempdir().unwrap();
        let resources = dir.path().join("Resources");
        std::fs::create_dir_all(&resources).unwrap();

        let root = bundle_root_from_resources(&resources);
        assert!(root.is_none(), "{root:?}");
    }

    /// The whole chain, from the directory Tauri hands us: a real tree with a
    /// real sidecar and manifest must come back verified.
    #[test]
    fn a_bundle_is_verified_from_the_resources_dir_tauri_reports() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Foo.app");
        let digest = write_sidecar(&bundle, b"daemon bytes");
        let resources = bundle.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        let manifest = manifest_for(&digest, EXPECTED_BUILD_ID);
        std::fs::write(
            resources.join("runtime-sidecar.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let root = bundle_root_from_resources(&resources).unwrap();
        assert!(verify_bundle(&root).is_verified());
    }

    #[test]
    fn the_expected_build_id_is_never_a_wrapper_version_string() {
        // It comes from `internal-runtime identity`, which is a hex digest.
        assert!(
            EXPECTED_BUILD_ID.is_empty()
                || EXPECTED_BUILD_ID.bytes().all(|b| b.is_ascii_hexdigit()),
            "{EXPECTED_BUILD_ID}"
        );
    }
}
