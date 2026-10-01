//! A live handle onto a project's runtime.
//!
//! The application-side delegation tools must not freeze a socket path and a
//! root triple into themselves: a daemon can die and be replaced, and when it is
//! the old root is swept away. They hold a `RuntimeBinding` instead and resolve
//! the current handle per call, so a replacement is picked up automatically.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_store::ipc::{IpcError, IpcRequest, IpcResponse, send_request};

use crate::attach::{self, AttachedProjectRuntime};

/// Everything one delegation call needs to reach the runtime: where to connect
/// and which root to act as.
#[derive(Clone, Debug)]
pub struct RuntimeHandle {
    pub socket_path: PathBuf,
    pub workspace_root: PathBuf,
    pub session_id: String,
    pub task_id: String,
    pub capability: String,
}

/// What is needed to bring the runtime back up when it is gone.
struct RepairPlan {
    cfg: RuntimeConfig,
    runtime_dir: PathBuf,
}

/// The mutable half: which handle is current, and -- for a managed binding -- the
/// attached runtime that keeps the embedded daemon alive.
struct BindingState {
    handle: RuntimeHandle,
    /// `None` for fixed bindings; `Some` for managed ones.
    runtime: Option<Arc<AttachedProjectRuntime>>,
    /// How many times a replacement runtime has been installed.
    ///
    /// A fresh binding is generation 0; every `repair()` that adopts a
    /// replacement runtime bumps this. A replacement daemon sweeps the previous
    /// daemon's roots away, so anything that cached a root across the boundary
    /// must be able to notice that the runtime it belongs to is gone.
    generation: u64,
}

/// A shared, self-healing reference to a project runtime.
///
/// `managed` bindings repair themselves; `fixed` bindings (the TUI, which owns
/// its daemon for the whole session) only ever hand out the handle they were
/// built with, preserving today's behaviour exactly.
pub struct RuntimeBinding {
    state: Mutex<BindingState>,
    repair: Option<RepairPlan>,
    /// Serialises repair so one thread starts the daemon while the others wait
    /// and then adopt its result instead of racing into a second start.
    flight: Mutex<()>,
}

impl RuntimeBinding {
    pub fn managed(
        cfg: &RuntimeConfig,
        runtime_dir: PathBuf,
        initial: Arc<AttachedProjectRuntime>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BindingState {
                handle: Self::handle_of(&initial),
                runtime: Some(initial),
                generation: 0,
            }),
            repair: Some(RepairPlan {
                cfg: cfg.clone(),
                runtime_dir,
            }),
            flight: Mutex::new(()),
        })
    }

    pub fn fixed(handle: RuntimeHandle) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BindingState {
                handle,
                runtime: None,
                generation: 0,
            }),
            repair: None,
            flight: Mutex::new(()),
        })
    }

    fn handle_of(rt: &AttachedProjectRuntime) -> RuntimeHandle {
        RuntimeHandle {
            socket_path: rt.socket_path.clone(),
            workspace_root: rt.workspace_root.clone(),
            session_id: rt.attached_root.session_id.clone(),
            task_id: rt.attached_root.task_id.clone(),
            capability: rt.attached_root.capability.clone(),
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BindingState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current handle, without any repair attempt.
    pub fn current(&self) -> Result<RuntimeHandle, String> {
        Ok(self.lock_state().handle.clone())
    }

    /// The current handle and the generation that produced it, read from one
    /// state snapshot.
    ///
    /// Reading both under a single lock matters: a caller that caches something
    /// keyed on the generation must be sure the handle it pairs it with came from
    /// that very generation.
    pub(crate) fn current_with_generation(&self) -> (RuntimeHandle, u64) {
        let state = self.lock_state();
        (state.handle.clone(), state.generation)
    }

    /// How many replacement runtimes this binding has installed.
    ///
    /// Starts at 0 and is bumped by every `repair()` that adopts a fresh runtime.
    /// A caller that caches something minted against one runtime (a conversation
    /// root, say) compares this before and after: a change means the runtime it
    /// cached against was replaced and whatever it cached is gone.
    pub fn generation(&self) -> u64 {
        self.lock_state().generation
    }

    /// The project directory this binding was created for. Used to decide
    /// whether any live thread still needs it.
    pub fn project_root(&self) -> PathBuf {
        let state = self.lock_state();
        state
            .runtime
            .as_ref()
            .map(|rt| rt.project_root.clone())
            .unwrap_or_else(|| state.handle.workspace_root.clone())
    }

    /// Brings the runtime back up (single-flight) and adopts the result.
    pub fn repair(&self) -> Result<RuntimeHandle, String> {
        self.repair_with_generation().map(|(handle, _generation)| handle)
    }

    /// Like `repair`, but also reports the generation of the runtime adopted.
    fn repair_with_generation(&self) -> Result<(RuntimeHandle, u64), String> {
        let Some(plan) = &self.repair else {
            return Err("this runtime binding cannot be repaired".to_string());
        };
        let _flight = self
            .flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another thread may have finished repairing while we waited for the
        // flight lock; if so, adopt its result instead of starting a second daemon.
        {
            let state = self.lock_state();
            if attach::probe_runtime(&state.handle.socket_path) == attach::RuntimeProbe::Healthy {
                return Ok((state.handle.clone(), state.generation));
            }
        }
        let fresh = attach::ensure_owned_runtime(&plan.cfg, plan.runtime_dir.clone())
            .map_err(|failure| format!("{}: {}", failure.stage, failure.cause))?;
        let handle = Self::handle_of(&fresh);
        let mut state = self.lock_state();
        state.handle = handle.clone();
        state.runtime = Some(Arc::new(fresh));
        // A replacement runtime sweeps the old one's roots away; record the
        // boundary so callers that cached a root can notice it is stale.
        state.generation = state.generation.wrapping_add(1);
        Ok((handle, state.generation))
    }

    /// Current if usable, else repair.
    pub fn current_or_repair(&self) -> Result<RuntimeHandle, String> {
        match self.current() {
            Ok(handle) => Ok(handle),
            Err(_) => self.repair(),
        }
    }

    /// Sends one request, repairing and retrying once if the runtime turns out
    /// to be unreachable. `build` is called with whichever handle is current.
    pub fn send<F>(&self, build: F) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        self.send_with_generation(build)
            .map(|(response, _generation)| response)
    }

    /// Like `send`, but also reports the generation of the runtime that served
    /// the request.
    ///
    /// The generation comes from the same snapshot as the handle, so an answer
    /// can never be attributed to a runtime other than the one that produced it
    /// -- a repair racing with the request cannot make a stale result look fresh.
    pub(crate) fn send_with_generation<F>(&self, build: F) -> Result<(IpcResponse, u64), String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        let (handle, generation) = self.current_with_generation();
        match send_request(&handle.socket_path, build(&handle)) {
            Ok(response) => Ok((response, generation)),
            Err(error) if is_liveness_error(&error) => {
                let (handle, generation) = self.repair_with_generation()?;
                send_request(&handle.socket_path, build(&handle))
                    .map(|response| (response, generation))
                    .map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Activates the current root, repairing first only when unreachable.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        match self.send(|h| IpcRequest::ActivateApplicationRoot {
            session_id: h.session_id.clone(),
            root_task_id: h.task_id.clone(),
            capability: h.capability.clone(),
            objective: objective.to_owned(),
        }) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!("daemon rejected the runtime activation: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// Detaches the current root. Best effort: a failure is a trace line, never
    /// an error that could mask why the process is winding down. There is no
    /// repair here -- a detach against a dead daemon has nothing to undo.
    pub fn detach(&self) {
        let Ok(handle) = self.current() else {
            return;
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the application root");
        }
    }
}

/// Whether an IPC failure means "the runtime is unreachable", which is the only
/// class worth repairing. Business rejections (a frame too large, a validation
/// error) arrive as `Ok(Error { .. })` or as transport-adjacent errors and must
/// not trigger a takeover.
pub fn is_liveness_error(error: &IpcError) -> bool {
    match error {
        IpcError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::TimedOut
        ),
        IpcError::TruncatedFrame { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};
    use yi_agent_store::ipc::IpcError;

    fn handle(socket: &str) -> RuntimeHandle {
        RuntimeHandle {
            socket_path: socket.into(),
            workspace_root: "/tmp/ws".into(),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        }
    }

    #[test]
    fn a_fixed_binding_hands_out_its_frozen_handle() {
        let binding = RuntimeBinding::fixed(handle("/tmp/frozen.sock"));
        assert_eq!(
            binding.current().unwrap().socket_path,
            PathBuf::from("/tmp/frozen.sock")
        );
    }

    #[test]
    fn a_fixed_binding_cannot_repair() {
        let binding = RuntimeBinding::fixed(handle("/tmp/frozen.sock"));
        assert!(
            binding.repair().is_err(),
            "TUI bindings have no repair plan"
        );
    }

    #[test]
    fn a_dead_socket_is_a_liveness_error() {
        let dead = IpcError::Io(IoError::new(ErrorKind::ConnectionRefused, "refused"));
        assert!(is_liveness_error(&dead));
    }

    #[test]
    fn a_frame_too_large_is_not_a_liveness_error() {
        assert!(!is_liveness_error(&IpcError::FrameTooLarge));
    }
}
