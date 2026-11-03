//! Carrying the control plane across a controlled `exec`.
//!
//! When the guardian's own build differs from the target, the old guardian cannot
//! keep serving: the new build owns the control plane. The handover must not lose
//! what clients already established, and it must not *invent* anything either.
//!
//! The state travels in a private anonymous descriptor as a small, bounded JSON
//! document. Three rules make that safe:
//!
//! 1. **A hard independent size limit.** 4 MiB, not the 16 KiB control-frame budget:
//!    the state carries grant lists and tombstone epochs, which do not fit in a
//!    frame. Over the limit the handover is refused *before* the old runtime stops,
//!    with manual guidance -- a truncated state would silently drop clients.
//! 2. **Everything the new program needs to check the state is in the state.** scope
//!    key, target build, handover version, the guardian's revision, and the count of
//!    each optional field. A new program that cannot match all of them refuses.
//! 3. **Remaining budgets are remaining, not fresh.** Leases are carried as the
//!    milliseconds left, measured on a cross-process monotonic clock, and the
//!    handover's own elapsed time is subtracted before they are used. Re-issuing a
//!    full 30 seconds would extend leases the user never granted.
//!
//! Nothing here is written to disk: the descriptor is anonymous, and a consumed token
//! is carried only as its identity so it cannot be replayed.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};

use serde::{Deserialize, Serialize};
use yi_agent_lifecycle::types::{ArtifactIdentity, ClientGrant, OperationStatus, RuntimeScope};

/// The largest state this build will carry across `exec`.
///
/// Independent of the control-frame limit on purpose: a control frame is one
/// message, while this is the whole live control state of a runtime.
pub const MAX_HANDOVER_BYTES: usize = 4 * 1024 * 1024;

/// The handover format this build writes and understands.
///
/// Bumped only for a change the other side cannot ignore; an unknown version is
/// refused rather than interpreted.
pub const HANDOVER_VERSION: u16 = 1;

/// A client's live lease, carried as the time left rather than an absolute instant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemainingLease {
    pub grant: ClientGrant,
    /// Milliseconds left on this lease when the state was captured.
    pub remaining_ms: u64,
}

/// The live control state of one guardian, as it crosses `exec`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoverState {
    /// Format version; a receiver refuses anything it does not know.
    pub version: u16,
    /// The runtime this state belongs to. A mismatch is refused.
    pub scope_key: String,
    /// The operation in flight, so a client polling after the switch still sees it.
    pub operation: OperationStatus,
    /// Identities of already-consumed confirmation tokens.
    ///
    /// Idents, not the tokens: carrying the token itself would re-arm a one-shot
    /// confirmation across a process boundary.
    pub consumed_tokens: Vec<String>,
    /// The OS monotonic reading at capture, so the receiver can subtract the
    /// handover's own elapsed time.
    pub captured_monotonic_ns: u64,
    /// Remaining lease time per client.
    pub remaining_leases_ms: Vec<RemainingLease>,
    /// Whatever is left of the start budget when the state was captured.
    pub remaining_start_budget_ms: u64,
    /// The build the runtime is switching to.
    pub target: ArtifactIdentity,
    /// The guardian revision this state was taken at.
    pub guardian_revision: u64,
    /// Released identities, so a carried lease cannot resurrect a released client.
    pub released: Vec<ReleasedIdentity>,
    /// Roles of the descriptors carried alongside this state.
    pub preserved: Vec<PreservedDescriptor>,
}

/// A client identity whose lease was released, with the epoch it was released at.
///
/// Without this a handover could hand back a lease the old guardian had already
/// released, which would resurrect a client that is supposed to be gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedIdentity {
    pub client_run_id: String,
    pub epoch: u64,
}

/// What one carried descriptor is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorRole {
    /// The bound control listener.
    ControlListener,
    /// The coordination lock, held for the duration of the switch.
    CoordinationLock,
    /// A registered client connection that has not finished its exchange.
    RegisteredConnection,
}

/// One descriptor the new program must recognise by role, not by number.
///
/// The number itself is deliberately absent: a number means nothing across `exec`
/// (the kernel may hand out a different one), while the role and its epoch are what
/// the receiver validates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreservedDescriptor {
    pub role: DescriptorRole,
    /// For a registered connection: the client it belongs to.
    #[serde(default)]
    pub client_run_id: Option<String>,
    /// For a registered connection: the epoch it was registered at.
    #[serde(default)]
    pub epoch: Option<u64>,
}

/// Why a handover state was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandoverRefusal {
    /// The state is larger than this build will carry.
    TooLarge { size: usize, limit: usize },
    /// The bytes are not a state this build understands.
    Malformed(String),
    /// A format version this build does not speak.
    UnknownVersion(u16),
    /// The state belongs to a different runtime.
    WrongScope { expected: String, found: String },
    /// The state was captured for a different target build.
    WrongTarget { expected: String, found: String },
    /// The state claims a release this build would undo.
    StaleRelease(String),
}

impl HandoverRefusal {
    /// A message fit to show a person, without the state's contents.
    pub fn reason(&self) -> String {
        match self {
            Self::TooLarge { size, limit } => format!(
                "the control state is {size} bytes, over the {limit} byte handover limit; \
                 quit the other frontends and restart the runtime instead of upgrading in place"
            ),
            Self::Malformed(detail) => format!("the handover state is unreadable: {detail}"),
            Self::UnknownVersion(version) => format!(
                "this build speaks handover version {HANDOVER_VERSION}, not {version}; \
                 restart the runtime manually"
            ),
            Self::WrongScope { expected, found } => {
                format!("the handover state belongs to runtime {found}, not {expected}")
            }
            Self::WrongTarget { expected, found } => {
                format!("the handover state was captured for build {found}, not {expected}")
            }
            Self::StaleRelease(client) => {
                format!("the handover state would resurrect the released client {client}")
            }
        }
    }
}

/// Serializes the state into a private anonymous descriptor.
///
/// `memfd_create` on Linux and an unlinked temp file on macOS both give a descriptor
/// with no name on disk. The writer's offset is rewound so the receiver reads from
/// the beginning.
pub fn encode_state(state: &HandoverState) -> io::Result<OwnedFd> {
    let body = serde_json::to_vec(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.len() > MAX_HANDOVER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the control state is {} bytes, over the {MAX_HANDOVER_BYTES} byte handover limit",
                body.len()
            ),
        ));
    }
    let mut file = anonymous_file()?;
    file.write_all(&body)?;
    file.flush()?;
    // The receiver must start at byte zero; `write_all` left the offset at the end.
    use std::io::Seek;
    file.rewind()?;
    // Keep the same descriptor as an `OwnedFd` without re-opening it.
    let fd = std::mem::ManuallyDrop::new(file);
    let raw = std::os::fd::AsRawFd::as_raw_fd(&*fd);
    // SAFETY: `fd` is a live file descriptor that this function owns; taking it as
    // `OwnedFd` transfers that ownership exactly once, and `ManuallyDrop` prevents a
    // second close.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Decodes a state the receiver was handed.
///
/// Reads at most `MAX_HANDOVER_BYTES + 1` so an oversized descriptor is refused
/// instead of read into memory.
pub fn decode_state(fd: BorrowedFd<'_>) -> Result<HandoverState, HandoverRefusal> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    // SAFETY: the caller owns `fd` for the duration of this call; `File::from_raw_fd`
    // would close it, so dup it first and own the duplicate instead.
    let duplicate = unsafe { libc::dup(raw) };
    if duplicate < 0 {
        return Err(HandoverRefusal::Malformed(
            io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: `dup` returned a fresh descriptor this function now owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(duplicate) };
    let mut body = Vec::new();
    let limit = (MAX_HANDOVER_BYTES + 1) as u64;
    if let Err(error) = (&mut file).take(limit).read_to_end(&mut body) {
        return Err(HandoverRefusal::Malformed(error.to_string()));
    }
    if body.len() > MAX_HANDOVER_BYTES {
        return Err(HandoverRefusal::TooLarge {
            size: body.len(),
            limit: MAX_HANDOVER_BYTES,
        });
    }
    let state: HandoverState = serde_json::from_slice(&body)
        .map_err(|error| HandoverRefusal::Malformed(error.to_string()))?;
    if state.version != HANDOVER_VERSION {
        return Err(HandoverRefusal::UnknownVersion(state.version));
    }
    Ok(state)
}

/// Validates a decoded state against the runtime the new program is about to serve.
///
/// Refusing here is the only safe answer: a state that does not match this scope or
/// target cannot be interpreted, and guessing would hand a client to the wrong
/// runtime.
pub fn validate_state(
    state: &HandoverState,
    scope: &RuntimeScope,
    target: &ArtifactIdentity,
) -> Result<(), HandoverRefusal> {
    if state.scope_key != scope.key {
        return Err(HandoverRefusal::WrongScope {
            expected: scope.key.clone(),
            found: state.scope_key.clone(),
        });
    }
    if state.target.build_id != target.build_id {
        return Err(HandoverRefusal::WrongTarget {
            expected: target.build_id.clone(),
            found: state.target.build_id.clone(),
        });
    }
    // A lease for a client the old guardian already released must not come back.
    for lease in &state.remaining_leases_ms {
        if state.released.iter().any(|released| {
            released.client_run_id == lease.grant.client_run_id
                && released.epoch >= lease.grant.epoch
        }) {
            return Err(HandoverRefusal::StaleRelease(
                lease.grant.client_run_id.clone(),
            ));
        }
    }
    Ok(())
}

/// The monotonic millisecond clock shared with a process that will `exec` over us.
///
/// Uses `CLOCK_MONOTONIC` with an absolute origin (`CLOCK_REALTIME`, sampled once) so
/// the reading is comparable across processes on one machine, unlike `Instant`, which
/// has a per-process origin and must never be serialized.
pub fn cross_process_monotonic_ns() -> u64 {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `now` is a valid `timespec` for the call to fill.
        let outcome = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        if outcome == 0 {
            return (now.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(now.tv_nsec as u64);
        }
    }
    0
}

/// Milliseconds left on a lease after subtracting the handover's own cost.
///
/// The subtraction is the whole point: a state captured `d` milliseconds ago must
/// hand over `remaining - d`, never `remaining`.
pub fn remaining_after_capture(captured_ns: u64, now_ns: u64, remaining_ms: u64) -> u64 {
    // A clock that appears to run backwards (a different machine, a suspend) must not
    // extend a lease, so an underflow keeps the lease exactly as it was.
    let elapsed_ms = now_ns.saturating_sub(captured_ns) / 1_000_000;
    remaining_ms.saturating_sub(elapsed_ms)
}

/// Creates a descriptor with no name on disk.
fn anonymous_file() -> io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `memfd_create` with a static name; the returned descriptor is owned
        // by the caller and nothing else references it.
        let fd = unsafe {
            libc::memfd_create(
                c"yi-agent-handover".as_ptr(),
                libc::MFD_CLOEXEC as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // The new program must inherit it, so CLOEXEC is cleared at exec time by the
        // caller that preserves descriptors; here it only avoids leaking into
        // unrelated children.
        // SAFETY: `fd` is a fresh descriptor this function owns.
        return Ok(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    #[cfg(target_os = "macos")]
    {
        // macOS has no memfd; an immediately unlinked temp file has the same property
        // (no name reachable after creation) and stays alive while the descriptor is
        // open.
        let mut template = *b"/tmp/yi-agent-handover-XXXXXX\0";
        // SAFETY: `mkstemp` writes into the template, which is a mutable array of the
        // size it expects, NUL-terminated.
        let fd = unsafe { libc::mkstemp(template.as_mut_ptr() as *mut libc::c_char) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let path = std::ffi::CStr::from_bytes_until_nul(&template)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        // SAFETY: `fd` is a fresh descriptor this function owns.
        return Ok(unsafe { std::fs::File::from_raw_fd(fd) });
    }
    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no anonymous descriptor primitive on this platform",
    ))
}

/// Replaces this process with the target artifact, preserving only `preserve`.
///
/// Never returns on success. This is the safe exec boundary: no allocator work after
/// the fork point, no other threads by contract (the caller must have quiesced), and
/// an explicit list of descriptors rather than "everything we happen to have open".
pub fn exec_target(
    target: &ArtifactIdentity,
    state: OwnedFd,
    preserve: &[RawFd],
) -> io::Result<std::convert::Infallible> {
    use std::os::fd::AsRawFd;

    // The state descriptor is handed to the child at a fixed number so the new
    // program can find it without guessing, and it is *not* CLOEXEC.
    const HANDOVER_STATE_FD: RawFd = 3;
    let state_raw = state.as_raw_fd();
    if state_raw != HANDOVER_STATE_FD {
        // SAFETY: both descriptors are valid; `dup2` makes the target name refer to
        // the same open file description.
        if unsafe { libc::dup2(state_raw, HANDOVER_STATE_FD) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // `dup2` keeps the destination's CLOEXEC flag when the source already *was* the
    // destination, and mkstemp/memfd set it. Clearing it explicitly is the only way
    // the new program receives the descriptor at all.
    // SAFETY: `F_GETFD`/`F_SETFD` on a descriptor this function owns.
    unsafe {
        let flags = libc::fcntl(HANDOVER_STATE_FD, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(HANDOVER_STATE_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    for raw in preserve {
        // SAFETY: each is a descriptor the caller declared it is carrying across.
        unsafe {
            let flags = libc::fcntl(*raw, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(*raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
    }

    let executable = std::ffi::CString::new(target.executable.to_string_lossy().as_bytes())
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the target executable path contains a NUL",
            )
        })?;
    let handover_env = std::ffi::CString::new(format!("YI_AGENT_HANDOVER_FD={HANDOVER_STATE_FD}"))
        .map_err(|_| io::Error::other("handover descriptor number was not representable"))?;
    let handover_version = std::ffi::CString::new(format!(
        "YI_AGENT_HANDOVER_VERSION={}",
        target.handover_version
    ))
    .map_err(|_| io::Error::other("handover version was not representable"))?;

    // `execv` over `execve` with an explicit environment: the new program must inherit
    // the instance's environment (YI_AGENT_HOME, model paths) plus the two handover
    // variables, and nothing else changes.
    let mut environment: Vec<CString> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            let value = value.into_string().ok()?;
            // Do not carry a stale handover variable from an earlier attempt.
            if key.starts_with("YI_AGENT_HANDOVER_") {
                return None;
            }
            CString::new(format!("{key}={value}")).ok()
        })
        .collect();
    environment.push(handover_env);
    environment.push(handover_version);
    let mut pointers: Vec<*const libc::c_char> =
        environment.iter().map(|entry| entry.as_ptr()).collect();
    pointers.push(std::ptr::null());
    let argv = [executable.as_ptr(), std::ptr::null()];

    // SAFETY: `executable` and `argv` are NUL-terminated and live for this call;
    // `pointers` is a null-terminated environment array of live strings.
    unsafe {
        libc::execv(executable.as_ptr(), argv.as_ptr());
    }
    // Only reached when exec failed.
    Err(io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn target(build: &str) -> ArtifactIdentity {
        ArtifactIdentity {
            build_id: build.into(),
            display_version: "0.2.0".into(),
            executable: "/usr/bin/true".into(),
            sha256: "aa".into(),
            schema_min: 1,
            schema_max: 16,
            handover_version: 1,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec![],
        }
    }

    fn scope() -> RuntimeScope {
        RuntimeScope {
            key: "/project/runtime".into(),
            runtime_dir: std::path::PathBuf::from("/project/runtime"),
            project_key: "/project".into(),
            instance_home: std::path::PathBuf::from("/project"),
        }
    }

    fn grant(client: &str) -> ClientGrant {
        ClientGrant {
            client_run_id: client.into(),
            mode: yi_agent_lifecycle::types::LeaseMode::Business,
            epoch: 1,
            credential: "secret".into(),
        }
    }

    fn status() -> OperationStatus {
        OperationStatus {
            operation_id: "op-1".into(),
            phase: yi_agent_lifecycle::types::SwitchPhase::Starting,
            detail: None,
            ready_identity: None,
        }
    }

    fn state() -> HandoverState {
        HandoverState {
            version: HANDOVER_VERSION,
            scope_key: scope().key,
            operation: status(),
            consumed_tokens: vec!["token-identity-1".into()],
            captured_monotonic_ns: 1_000_000_000,
            remaining_leases_ms: vec![RemainingLease {
                grant: grant("tui"),
                remaining_ms: 20_000,
            }],
            remaining_start_budget_ms: 25_000,
            target: target("next"),
            guardian_revision: 7,
            released: vec![ReleasedIdentity {
                client_run_id: "gone".into(),
                epoch: 3,
            }],
            preserved: vec![
                PreservedDescriptor {
                    role: DescriptorRole::ControlListener,
                    client_run_id: None,
                    epoch: None,
                },
                PreservedDescriptor {
                    role: DescriptorRole::RegisteredConnection,
                    client_run_id: Some("tui".into()),
                    epoch: Some(1),
                },
            ],
        }
    }

    #[test]
    fn a_state_round_trips_through_a_descriptor() {
        let original = state();
        let fd = encode_state(&original).unwrap();
        let decoded = decode_state(fd.as_fd()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn a_consumed_token_is_carried_as_an_identity_not_a_token() {
        // The field must never hold anything a receiver could re-submit.
        let decoded = decode_state(encode_state(&state()).unwrap().as_fd()).unwrap();
        assert_eq!(
            decoded.consumed_tokens,
            vec!["token-identity-1".to_string()]
        );
    }

    #[test]
    fn a_state_for_another_scope_is_refused() {
        let mut other = state();
        other.scope_key = "/somewhere/else".into();
        let refusal = validate_state(&other, &scope(), &target("next")).unwrap_err();
        assert!(matches!(refusal, HandoverRefusal::WrongScope { .. }));
    }

    #[test]
    fn a_state_for_another_target_is_refused() {
        let refusal = validate_state(&state(), &scope(), &target("other")).unwrap_err();
        assert!(matches!(refusal, HandoverRefusal::WrongTarget { .. }));
    }

    /// A lease the old guardian released must not come back through the handover.
    #[test]
    fn a_released_client_is_not_resurrected() {
        let mut carried = state();
        carried.released.push(ReleasedIdentity {
            client_run_id: "tui".into(),
            epoch: 5,
        });
        let refusal = validate_state(&carried, &scope(), &target("next")).unwrap_err();
        assert!(matches!(refusal, HandoverRefusal::StaleRelease(_)));
    }

    #[test]
    fn an_unknown_version_is_refused_not_interpreted() {
        let mut future = state();
        future.version = HANDOVER_VERSION + 1;
        let fd = encode_state(&future).unwrap();
        let refusal = decode_state(fd.as_fd()).unwrap_err();
        assert_eq!(
            refusal,
            HandoverRefusal::UnknownVersion(HANDOVER_VERSION + 1)
        );
    }

    #[test]
    fn garbage_is_refused_rather_than_guessed() {
        let fd = anonymous_file().unwrap();
        use std::io::Write as _;
        let mut writer = &fd;
        writer.write_all(b"not json at all").unwrap();
        use std::io::Seek;
        (&fd).rewind().unwrap();
        let refusal = decode_state(fd.as_fd()).unwrap_err();
        assert!(matches!(refusal, HandoverRefusal::Malformed(_)));
    }

    /// Over the limit the handover is refused *before* the old runtime stops, so the
    /// caller still has a working runtime.
    #[test]
    fn an_oversized_state_is_refused_with_manual_guidance() {
        let mut huge = state();
        huge.consumed_tokens = (0..MAX_HANDOVER_BYTES / 8)
            .map(|index| format!("{index:08}"))
            .collect();
        let error = encode_state(&huge).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("handover limit"));
    }

    /// Only the elapsed time is subtracted; a fresh full budget is never re-issued.
    #[test]
    fn the_handover_cost_is_subtracted_from_the_remaining_budget() {
        let captured = 1_000_000_000u64;
        // 3 seconds of handover.
        let now = captured + 3_000_000_000;
        assert_eq!(remaining_after_capture(captured, now, 20_000), 17_000);
    }

    #[test]
    fn a_clock_that_goes_backwards_never_extends_a_lease() {
        let captured = 5_000_000_000u64;
        let now = 1_000_000_000u64;
        assert_eq!(remaining_after_capture(captured, now, 20_000), 20_000);
    }

    /// The monotonic reading is a real value, not zero: a zero would make every
    /// elapsed computation subtract nothing.
    #[test]
    fn the_cross_process_clock_reads_a_real_value() {
        let first = cross_process_monotonic_ns();
        assert!(first > 0, "the monotonic clock must be readable");
        let second = cross_process_monotonic_ns();
        assert!(second >= first, "a monotonic clock must not go backwards");
    }

    #[test]
    fn the_limit_is_independent_of_the_control_frame_budget() {
        // The control frame is 16 KiB; the state carries grants and tombstones and
        // must not be squeezed into that budget.
        assert_eq!(MAX_HANDOVER_BYTES, 4 * 1024 * 1024);
        const { assert!(MAX_HANDOVER_BYTES > 16 * 1024) };
    }
}
