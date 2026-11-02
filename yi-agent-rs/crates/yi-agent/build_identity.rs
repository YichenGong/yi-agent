// Daemon artifact identity as a function of build inputs.
//
// Shared by `build.rs` (which embeds the resulting id) and the integration test
// that pins the property. It lives at the crate root, outside `src/`, so that
// `#[path = "../build_identity.rs"]` can include the exact production code in a
// test and by `include!` in the build script — no second copy to drift.
//
// Why not `git describe --dirty`: the spec requires the identity to describe the
// **daemon executable artifact**. Two different wrapper versions can ship the same
// sidecar bytes (must compare equal), and one git SHA can produce different
// binaries when a feature flag, a `RUSTFLAGS` value, the rustc version, or the
// target triple changes (must compare different). `-dirty` is a file-mtime
// heuristic: it misses staged-only changes and cannot see build configuration at
// all.

use std::path::Path;

/// Directories that are never build inputs.
///
/// Scanning `target/` alone would make the id change on every incremental build;
/// scanning `.git/` or a sibling worktree would make it depend on other checkouts.
/// The spec is explicit that the walk must stay inside known workspace source and
/// config directories and must not reach into a user's home directory.
const EXCLUDED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".worktrees",
    ".superpowers",
    "dist",
    ".turbo",
    ".next",
];

/// Everything that can change the produced bytes.
#[derive(Clone, Debug)]
pub struct BuildInputs {
    /// `(path relative to the crate root, file contents)`, in any order.
    pub sources: Vec<(String, Vec<u8>)>,
    pub target_triple: String,
    pub rustc_version: String,
    pub profile: String,
    /// Sorted, deduplicated by `build_id`.
    pub features: Vec<String>,
    /// The exact `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` value, empty when unset.
    pub rustflags: String,
    /// The crate manifest bytes and the workspace lockfile bytes.
    pub manifest: Vec<u8>,
}

/// Owned form used so the test and the build script can build one value.
#[derive(Clone, Debug)]
pub struct OwnedBuildInputs(BuildInputs);

impl From<BuildInputs> for OwnedBuildInputs {
    fn from(inputs: BuildInputs) -> Self {
        Self(inputs)
    }
}

impl OwnedBuildInputs {
    pub fn get(&self) -> &BuildInputs {
        &self.0
    }
}

/// Collect `(relative path, bytes)` for every file under `crate_dir` that can
/// affect the artifact, skipping [`EXCLUDED_DIRS`] and any extra `skip` prefixes.
///
/// Paths are normalized to forward slashes and sorted, so the digest does not
/// depend on directory iteration order (which differs between filesystems).
pub fn collect_sources(crate_dir: &Path, skip: &[&str]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    walk(crate_dir, crate_dir, skip, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk(root: &Path, dir: &Path, skip: &[&str], out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            if EXCLUDED_DIRS.contains(&name.as_str()) {
                continue;
            }
            if skip.iter().any(|prefix| relative == *prefix) {
                continue;
            }
            walk(root, &path, skip, out);
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push((relative, bytes));
        }
    }
}

/// Digest a set of inputs, order-independently.
///
/// Each entry contributes `len(path) || path || len(body) || body` so that
/// concatenation is unambiguous: without the lengths, `("a", "bc")` and
/// `("ab", "c")` would hash the same.
pub fn input_digest(inputs: &[(String, Vec<u8>)]) -> String {
    use sha2::{Digest, Sha256};

    let mut sorted: Vec<&(String, Vec<u8>)> = inputs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (path, body) in sorted {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((body.len() as u64).to_be_bytes());
        hasher.update(body);
    }
    hex32(&hasher.finalize())
}

/// The artifact id: 128 bits of the digest over every build input.
pub fn build_id(inputs: OwnedBuildInputs) -> String {
    use sha2::{Digest, Sha256};

    let inputs = inputs.get();
    let source_digest = input_digest(&inputs.sources);

    let mut features = inputs.features.clone();
    features.sort();
    features.dedup();

    let mut hasher = Sha256::new();
    hasher.update(b"yi-agent-daemon-build-identity-v1\0");
    hasher.update(source_digest.as_bytes());
    hasher.update(b"\0target\0");
    hasher.update(inputs.target_triple.as_bytes());
    hasher.update(b"\0rustc\0");
    hasher.update(inputs.rustc_version.as_bytes());
    hasher.update(b"\0profile\0");
    hasher.update(inputs.profile.as_bytes());
    hasher.update(b"\0features\0");
    for feature in &features {
        hasher.update(feature.as_bytes());
        hasher.update(b",");
    }
    hasher.update(b"\0rustflags\0");
    hasher.update(inputs.rustflags.as_bytes());
    hasher.update(b"\0manifest\0");
    hasher.update((inputs.manifest.len() as u64).to_be_bytes());
    hasher.update(&inputs.manifest);
    hex32(&hasher.finalize())
}

fn hex32(digest: &[u8]) -> String {
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
