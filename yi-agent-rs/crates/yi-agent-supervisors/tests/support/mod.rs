//! Shared fixtures for the supervisor integration tests.

pub mod process_fixture;

/// A short instance home derived from `seed`, so the derived socket path fits
/// `sun_path` even when the seed itself (a `TempDir`) is long.
///
/// Deterministic on purpose: fixtures call their `scope()` helper repeatedly, and
/// two different homes would mean two different socket paths for one guardian.
// Shared by every test binary that includes this module, and several of them do not
// need a shortened home; without this each of those reports it as dead code.
#[allow(dead_code)]
pub fn short_instance_home(seed: &std::path::Path) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    std::path::PathBuf::from(format!("/tmp/ya-{:016x}", hasher.finish()))
}
