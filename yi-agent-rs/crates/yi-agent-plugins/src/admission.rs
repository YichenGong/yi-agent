//! 活动准入与排空屏障。
//!
//! # 两条硬约束
//!
//! 1. **permit 必须是真实存活的 OS 锁。** 进程崩溃 → 内核释放 → 对端可观察
//!    到"已无操作"；进程被 `SIGSTOP` → **不释放**。因此判定"已无操作"只能
//!    看锁的实际状态，不能看 TTL、PID 存活或文件 mtime。按 TTL 回收会
//!    把一个被暂停、恢复后仍要继续写的进程当成已经结束。
//! 2. **`parent_turn` 字符串不是授权。** 仅凭一个长得像别人 turn ID 的字符串
//!    就允许外部写收尾，等于任何调用方都能认领别人的回合。要收尾必须同时
//!    满足：turn 在宿主侧登记过、该 turn 仍有存活的屏障前 permit、请求的
//!    binding 与该 turn 的 binding 在 `project` 与 `grant` 上一致。
//!
//! # 排空期间的准入规则
//!
//! `draining` 拒绝新的 `Launch` 与**外部** `Write`，但允许旧 turn 的状态写与
//! permit 释放。否则一旦开始排空，正在收尾的回合就永远收不完——它既不能
//! 写，也不能释放 permit，屏障永远等不到零。
//!
//! # 存活检测怎么做
//!
//! 用**一个独立的文件描述符**去试 `flock(LOCK_EX | LOCK_NB)`：`flock` 按打开
//! 文件描述加锁，同进程里第二个描述符同样会冲突。拿到锁说明没人持有（然后
//! 立刻放掉），拿不到才说明真有人持有。这条**不能**用本进程的去重表判断，
//! 因为表里存的是 `Arc`，`Arc` 还在不代表锁还在。

use std::collections::BTreeMap;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::error::PluginError;
use crate::locks::{self, ExclusiveLock};
use crate::types::{Binding, Effect};

/// 实例的准入阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 正常接受新工作。
    Open,
    /// 正在排空：拒绝新的 Launch 与外部 Write，允许旧 turn 收尾。
    Draining,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Open => "open",
            Phase::Draining => "draining",
        }
    }
}

/// 一个回合的存活凭据。
///
/// 持有期间 permit 文件被 `flock` 锁住。`Drop` 关闭文件即释放；进程崩溃
/// 时由内核释放。字段里没有"过期时间"——那是设计明确禁止的判据。
#[derive(Debug)]
pub struct TurnPermit {
    /// 只为持有锁而存在。
    _lock: Arc<ExclusiveLock>,
    turn_id: String,
    binding: Binding,
    before_barrier: bool,
    /// 释放时要把自己从登记表里摘掉。只 release 锁而不摘表，会让
    /// 「锁已释放」与「表里还有这个 turn」同时成立，屏障计数就永远不归零。
    registry: Arc<Mutex<BTreeMap<String, RegisteredTurn>>>,
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.turn_id);
        // `_lock` 随后 drop，内核释放 flock。
    }
}

impl TurnPermit {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// 是否在屏障之前创建。排空期只认这类 permit。
    pub fn is_before_barrier(&self) -> bool {
        self.before_barrier
    }
}

#[derive(Debug, Clone)]
struct RegisteredTurn {
    lock: Arc<ExclusiveLock>,
    binding: Binding,
    before_barrier: bool,
}

/// 准入控制。跨进程共享同一实例根下的 permit 目录。
#[derive(Debug)]
pub struct Admission {
    home: PathBuf,
    phase: Mutex<Phase>,
    turns: Arc<Mutex<BTreeMap<String, RegisteredTurn>>>,
}

impl Admission {
    pub fn new(home: impl Into<PathBuf>) -> Admission {
        Admission {
            home: home.into(),
            phase: Mutex::new(Phase::Open),
            turns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn phase(&self) -> Phase {
        *self.phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn permit_path(&self, turn_id: &str) -> PathBuf {
        self.home
            .join("plugins")
            .join("permits")
            .join(format!("{turn_id}.permit"))
    }

    /// 开始排空。返回时屏障已建立：此后创建的 permit 都标记为 post-barrier。
    pub fn begin_drain(&self) -> Result<(), PluginError> {
        let mut phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        *phase = Phase::Draining;
        Ok(())
    }

    /// 取消排空，恢复旧准入。**不能关闭整个插件**：排空是一次生命周期动作，
    /// 取消它只是回到原状态。
    pub fn cancel_drain(&self) -> Result<(), PluginError> {
        let mut phase = self.phase.lock().unwrap_or_else(|e| e.into_inner());
        *phase = Phase::Open;
        Ok(())
    }

    /// 取得一个回合的准入。
    ///
    /// 排空期间拒绝新的 turn —— 它们属于"新工作"，屏障不等人。
    pub fn admit_turn(&self, turn_id: &str, binding: Binding) -> Result<TurnPermit, PluginError> {
        // 先判阶段、再取锁：反过来的话，排空刚开始时会出现一个"已经拿到
        // permit 但阶段已变"的窗口，屏障就会为它多等一个回合。
        if self.phase() == Phase::Draining {
            return Err(PluginError::Draining(format!(
                "the plugin is draining; turn `{turn_id}` may not start"
            )));
        }
        let path = self.permit_path(turn_id);
        let lock = locks::try_acquire(&path)?;
        let registered = RegisteredTurn {
            lock: lock.clone(),
            binding: binding.clone(),
            before_barrier: true,
        };
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(turn_id.to_string(), registered);

        // `Admission` 可以被克隆进不同任务；permit 持有登记表的 `Arc`，
        // 这样即使 `Admission` 已经 drop，permit 的 `Drop` 仍能清表。
        Ok(TurnPermit {
            _lock: lock,
            turn_id: turn_id.to_string(),
            binding,
            before_barrier: true,
            registry: Arc::clone(&self.turns),
        })
    }

    /// 释放一个回合的 permit（正常结束路径）。
    ///
    /// 正常路径应当直接 `drop` 那个 `TurnPermit`；这个函数留给"宿主自己
    /// 结束了一个回合但没有 panic-safe 的 permit 变量"的场景。
    pub fn end_turn(&self, turn_id: &str) {
        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
        turns.remove(turn_id);
    }

    /// 取得一个**外部**写操作的准入。
    ///
    /// 排空期间外部写被拒（`Draining`）。已登记的旧 turn 应当走
    /// `admit_turn_write`，那条路径要求提供可核验的 turn 凭据。
    pub fn admit_external_write(
        &self,
        request_id: &str,
        binding: &Binding,
    ) -> Result<(), PluginError> {
        match self.phase() {
            Phase::Open => {
                let _ = (request_id, binding);
                Ok(())
            }
            Phase::Draining => Err(PluginError::Draining(
                "the plugin is draining; external writes are refused until it finishes".to_string(),
            )),
        }
    }

    /// 旧 turn 在排空期的状态写。
    ///
    /// 允许的条件（G1 §6.2 三条全要满足）：
    /// 1. 该 turn 在本宿主登记过；
    /// 2. 该 turn 仍有存活的**屏障前** permit；
    /// 3. 请求的 binding 与该 turn 的 binding 在 `project` 与 `grant` 上一致。
    pub fn admit_turn_write(
        &self,
        turn_id: &str,
        binding: &Binding,
        effect: Effect,
    ) -> Result<(), PluginError> {
        if effect != Effect::Write {
            return Err(PluginError::PermissionDenied(format!(
                "`admit_turn_write` only covers Writes, got {effect:?}"
            )));
        }

        let registered = self
            .turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(turn_id)
            .cloned()
            .ok_or_else(|| {
                PluginError::PermissionDenied(format!(
                    "turn `{turn_id}` is not registered in this host; a caller-supplied \
                     turn id is not authorization"
                ))
            })?;

        // 锁是否仍被持有——这是唯一可信的"该 turn 仍在进行"的证据。
        if !is_lock_held(&registered.lock) {
            return Err(PluginError::PermissionDenied(format!(
                "turn `{turn_id}` has already released its permit; a finished turn may not \
                 start another round"
            )));
        }

        if !registered.before_barrier {
            return Err(PluginError::Draining(format!(
                "turn `{turn_id}` started after the barrier; it is new work, not a wrap-up"
            )));
        }

        // binding 必须与登记的 turn 在同一项目、同一授权代次上。
        if registered.binding.project != binding.project
            || registered.binding.grant != binding.grant
        {
            return Err(PluginError::PermissionDenied(format!(
                "turn `{turn_id}` belongs to project `{}` grant {}, not project `{}` grant {}",
                registered.binding.project.as_str(),
                registered.binding.grant,
                binding.project.as_str(),
                binding.grant
            )));
        }

        Ok(())
    }

    /// 仍存活的屏障前 permit 数量。屏障"等它归零"用的就是它。
    ///
    /// **必须以磁盘上的锁为准，不能读进程内的登记表。** 三个理由：
    ///
    /// 1. 屏障的读者（生命周期事务）通常持有**另一个** `Admission` 句柄，
    ///    它看不到登记表里由别的句柄放进去的条目——那样屏障会立刻"通过"，
    ///    而旧回合其实还在写。
    /// 2. 跨进程的回合完全不在本进程的登记表里，读表会把它们算成零。
    /// 3. `flock` 才是真实存活证据：进程崩溃由内核释放，`SIGSTOP` 则不释放。
    ///
    /// 目录里的每个 permit 都是屏障前的：排空期间 `admit_turn` 直接拒绝，
    /// 根本不会新建文件。所以"文件存在且锁被持有"就是"屏障前的活跃回合"。
    /// 打不开的文件按仍被持有处理——让屏障多等一轮，比在旧进程还在写时
    /// 继续迁移安全。
    pub fn live_before_barrier_permits(&self) -> usize {
        let dir = self.home.join("plugins").join("permits");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("permit")
            })
            .filter(|entry| is_path_held(&entry.path()))
            .count()
    }

    /// 上升的 `Launch` 准入。排空期一律拒绝：启动新进程是"新工作"。
    pub fn admit_launch(&self, binding: &Binding) -> Result<(), PluginError> {
        match self.phase() {
            Phase::Open => {
                let _ = binding;
                Ok(())
            }
            Phase::Draining => Err(PluginError::Draining(
                "the plugin is draining; no new launches are accepted".to_string(),
            )),
        }
    }
}

/// 按路径探测：这个 permit 文件上的锁现在还有人持有吗？
fn is_path_held(path: &Path) -> bool {
    let Ok(probe) = std::fs::OpenOptions::new().write(true).open(path) else {
        // 打不开就不能证明"没人持有"。
        return true;
    };
    let outcome = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        // 探针拿到了锁 → 真的没人持有。立即放掉。
        unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    let error = std::io::Error::last_os_error();
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    )
}

/// 用一个**独立描述符**探测锁是否仍被持有。
///
/// `flock` 按打开文件描述加锁：同进程里再 open 一次会得到新描述符，与已持有
/// 的锁照样冲突。所以这里拿到 `LOCK_EX` 就说明确实没人持有；拿不到说明有。
///
/// 拿到之后立即释放——探针本身不能影响真实状态。
fn is_lock_held(lock: &Arc<ExclusiveLock>) -> bool {
    let path: &Path = lock.path();
    let Ok(probe) = std::fs::OpenOptions::new().write(true).open(path) else {
        // 打不开就不能证明"没人持有"，按仍被持有处理：让屏障多等一轮，
        // 比误判为已结束、在旧进程还在写的时候继续迁移要安全。
        return true;
    };
    let outcome = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        // 探针拿到了锁 → 真的没人持有。立即放掉。
        unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    let error = std::io::Error::last_os_error();
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
    )
}
