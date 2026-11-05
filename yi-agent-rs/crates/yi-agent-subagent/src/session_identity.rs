//! The host's private resume credential, one file per conversation.
//!
//! This is the only long-lived secret the new delegation lifecycle adds, and it
//! must never reach the runtime database, a log line, the thread transcript or a
//! fork payload. It therefore lives in its own directory, is written once with
//! `0600` and created with `create_new`, and is never rewritten: "load" either
//! finds the file the host wrote before or refuses to guess.
//!
//! A corrupt or world-readable file is an error rather than a miss. Recreating it
//! on suspicion would silently rotate a credential the daemon still associates
//! with this conversation, which is how a host would lose access to its own
//! session and never be told why.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use yi_agent_lifecycle::foreground::SecretCapability;

/// Where the host stores one resume secret per conversation.
///
/// The directory is supplied by the caller (the session store), never derived
/// from the cwd: a cwd-derived identity would move when the user `cd`s, and two
/// terminals in the same directory would then share a credential.
#[derive(Clone, Debug)]
pub struct SessionIdentityStore {
    directory: PathBuf,
}

impl SessionIdentityStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// The directory this store owns. Exposed so a caller can assert on the file
    /// it expects, and so nothing needs to reconstruct it by hand.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The secret for `thread_id`, created on first use.
    ///
    /// Concurrent first calls agree on one file: `create_new` makes exactly one
    /// writer a winner, and the loser reads what the winner wrote rather than
    /// overwriting it.
    pub fn load_or_create(&self, thread_id: &str) -> std::io::Result<SecretCapability> {
        let path = self.path_for(thread_id)?;
        if path.exists() {
            return self.read(&path);
        }
        self.ensure_directory()?;
        let secret = new_secret();
        match write_new(&path, &secret) {
            Ok(()) => Ok(SecretCapability::new(secret)),
            // Another process won the race. Its value is the credential; adopting
            // our own would give the two hosts two different identities for one
            // conversation.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => self.read(&path),
            Err(error) => Err(error),
        }
    }

    /// Forgets the secret. Failure to remove is reported, not swallowed: a caller
    /// retiring a conversation has to know whether the credential is really gone.
    pub fn remove(&self, thread_id: &str) -> std::io::Result<()> {
        let path = self.path_for(thread_id)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The file for one thread, refusing anything that could escape the directory.
    fn path_for(&self, thread_id: &str) -> std::io::Result<PathBuf> {
        if !is_safe_thread_id(thread_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a thread id may only contain letters, digits, '-', '_' and '.'",
            ));
        }
        Ok(self.directory.join(format!("{thread_id}.json")))
    }

    fn ensure_directory(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.directory)?;
        // Applied every time, not just at creation: a directory that lost its
        // permissions is the exact case where a secret is readable by others.
        std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))
    }

    fn read(&self, path: &Path) -> std::io::Result<SecretCapability> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the resume secret file is a symlink; refusing to follow it",
            ));
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the resume secret file is not 0600; refusing to trust it",
            ));
        }
        let body = std::fs::read_to_string(path)?;
        let value = serde_json::from_str::<StoredSecret>(&body)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .secret;
        if value.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the resume secret file is empty",
            ));
        }
        Ok(SecretCapability::new(value))
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredSecret {
    secret: String,
}

/// A fresh secret with the same entropy source the daemon uses.
fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let mut secret = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(secret, "{byte:02x}");
    }
    // Domain separation is unnecessary here (the digest is computed later, with
    // its own domain), but a stable prefix makes a leaked value identifiable.
    format!("resume-{secret}")
}

/// Writes through a temporary file and renames, so a reader never sees a partial
/// secret and a crash cannot leave an empty file that "load" would reject.
fn write_new(path: &Path, secret: &str) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    let body = serde_json::to_vec(&StoredSecret {
        secret: secret.to_string(),
    })
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&body)?;
    file.sync_all()?;
    drop(file);
    // `create_new` on the destination, not `rename`: the loser of a race must
    // read the winner's file, and rename would silently replace it.
    match std::fs::hard_link(&temporary, path) {
        Ok(()) => {}
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            // Filesystems without hard links: fall back to a checked rename.
            if path.exists() {
                return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
            }
            std::fs::rename(&temporary, path)?;
            return Ok(());
        }
    }
    let _ = std::fs::remove_file(&temporary);
    if let Some(parent) = path.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Whether a thread id is a plain file name component.
///
/// Deliberately narrow: a thread id reaches a file path, and `.`/`..` or a slash
/// would let one conversation address another's secret.
fn is_safe_thread_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

/// The digest the daemon stores for a resume secret.
///
/// Re-exported through this module so a host never has to reach for the lifecycle
/// crate to compute the value it will send.
pub fn resume_digest(secret: &SecretCapability) -> String {
    yi_agent_lifecycle::session_delegation::session_resume_digest(secret)
}

/// A stable, non-secret fingerprint of a secret, safe to put in a log line.
///
/// `Debug` on `SecretCapability` already redacts the value; this exists for the
/// few places that want to correlate two hosts without printing the secret.
pub fn fingerprint(secret: &SecretCapability) -> String {
    let digest = Sha256::digest(secret.expose().as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_delegation_lifecycle_resume_secret_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionIdentityStore::new(dir.path().join("identities"));
        let a = store.load_or_create("thread-one").unwrap();
        let b = store.load_or_create("thread-one").unwrap();
        assert_eq!(a, b);
        let path = dir.path().join("identities/thread-one.json");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            std::fs::metadata(dir.path().join("identities"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
                == 0o700
        );
        assert!(store.load_or_create("../escape").is_err());
        assert!(store.load_or_create("..").is_err());
        assert!(store.load_or_create("").is_err());
        assert!(!format!("{a:?}").contains(a.expose()));
    }

    #[test]
    fn session_delegation_lifecycle_a_world_readable_secret_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = SessionIdentityStore::new(dir.path().join("identities"));
        let secret = store.load_or_create("thread-one").unwrap();
        let path = dir.path().join("identities/thread-one.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = store.load_or_create("thread-one").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        // A refused file must not be replaced by a fresh secret: that would rotate
        // the credential the daemon still holds for this conversation.
        assert!(path.exists());
        assert!(!secret.expose().is_empty());
    }

    #[test]
    fn session_delegation_lifecycle_a_corrupt_secret_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionIdentityStore::new(dir.path().join("identities"));
        store.load_or_create("thread-one").unwrap();
        let path = dir.path().join("identities/thread-one.json");
        std::fs::write(&path, b"not json").unwrap();
        let error = store.load_or_create("thread-one").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn session_delegation_lifecycle_remove_forgets_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionIdentityStore::new(dir.path().join("identities"));
        store.load_or_create("thread-one").unwrap();
        store.remove("thread-one").unwrap();
        assert!(!dir.path().join("identities/thread-one.json").exists());
        // Removing twice is not an error: retiring a conversation is idempotent.
        store.remove("thread-one").unwrap();
    }
}
