//! 整机共享配额域（G3 §6.2 / §6.3）。
//!
//! # 为什么这个文件不能依赖宿主 crate
//!
//! 插件对主程序保持零依赖（Spec 4 §2）。所以这里自带一份实现，但它读写的是
//! **同一份磁盘契约**：
//!
//! - 配额域 = `$HOME/.yi-agent/superpowers-kanban/`；
//! - 配置 = 域内 `quota.json`（`{revision, max_concurrent, max_starts_per_hour}`）；
//! - 租约 = 域内 `leases/slot-N.lock`（flock 槽位，已有实现）；
//! - 活动 = 域内 `activity/<owner>.activity`（本模块新增，见下）。
//!
//! 契约是 JSON 形状与目录布局，不是代码。宿主侧 `yi-agent-plugins` 的
//! `quota.rs` 有对应实现；**改一边必须同时改另一边**，两边各有测试钉住形状。
//!
//! # 为什么活动要用「活锁 + 计数文件」而不是各实例自报
//!
//! `sessions.running` 是宿主**本进程**的数字。两个实例各自拿它去比总上限，
//! 两边都会看到"还有余量"，合计就超了（G3 §6.3）。要判断"别的实例此刻有多
//! 少会话在跑"，必须有一个跨进程、且能分辨「活着」与「留下了垃圾」的记录。
//!
//! `flock` 正好具备这个性质：进程退出（含 `SIGKILL`）内核释放，`SIGSTOP` 不
//! 释放。所以每个实例把自己的计数写进 `activity/<owner>.activity` 并**一直
//! 持有该文件的 flock**；别人扫这个目录时，能拿到锁的说明那人已经没了，跳过；
//! 拿不到的才计入合计。
//!
//! 计数写在文件里而不是靠文件是否存在：一个实例可能开着 0 个会话（它只是活着），
//! 这时它仍应参与「我活着」的判断，但贡献 0。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配额域的目录名。配置与租约都必须在它下面。
const DOMAIN: &str = "superpowers-kanban";

/// 一份共享配额配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedQuotaConfig {
    pub revision: u64,
    pub max_concurrent: u32,
    pub max_starts_per_hour: u32,
}

/// 一次共享配额域定位的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaDomain {
    /// `$HOME/.yi-agent/superpowers-kanban`。
    pub root: PathBuf,
}

impl QuotaDomain {
    /// 按 `home` 定位配额域。
    ///
    /// `None` 表示**无法定位共享域**。调用方必须据此**阻止**新的自动任务，
    /// 而不是退回"本实例自己的池"——后者会让两个实例各拿一份完整预算，用户
    /// 账单翻倍，而且没有任何报错（G3 §6.2）。
    pub fn resolve(home: &Path) -> Option<QuotaDomain> {
        Some(QuotaDomain {
            root: home.join(".yi-agent").join(DOMAIN),
        })
    }

    pub fn leases_dir(&self) -> PathBuf {
        self.root.join("leases")
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("quota.json")
    }

    pub fn activity_dir(&self) -> PathBuf {
        self.root.join("activity")
    }

    /// 维护所有权目录。与租约、活动同域：维护动作也是"整机只许一个"。
    pub fn owners_dir(&self) -> PathBuf {
        self.root.join("owners")
    }

    /// 读配置。`Ok(None)` 表示没有配置（没人授权过整机配额）。
    ///
    /// 坏文件报 `Err` 而不是当空：当成空表会让调用方以为"还没有人设过"，
    /// 于是写一份默认值，把另一个实例刚设的限额覆盖掉。
    pub fn read_config(&self) -> Result<Option<SharedQuotaConfig>, String> {
        match std::fs::read(self.config_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                format!(
                    "shared quota config {} is corrupt ({error}); refusing to guess a limit",
                    self.config_path().display()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "could not read {}: {error}",
                self.config_path().display()
            )),
        }
    }

    /// 写配置。`expected_revision` 必须等于当前修订，且 `next.revision` 必须
    /// 正好加一。
    ///
    /// 两道检查都是为了同一件事：两个实例同时改设置时，后写的那个不能静默
    /// 覆盖先写的。调用方拿到 `Err` 应当重读、与用户重新确认，而不是重试。
    pub fn write_config(
        &self,
        expected_revision: u64,
        next: &SharedQuotaConfig,
    ) -> Result<(), String> {
        let current = self
            .read_config()?
            .map(|config| config.revision)
            .unwrap_or(0);
        if current != expected_revision {
            return Err(format!(
                "shared quota config is at revision {current}, not {expected_revision}; \
                 another instance changed it and overwriting would discard their limit"
            ));
        }
        let want = expected_revision + 1;
        if next.revision != want {
            return Err(format!(
                "a write must advance the revision to {want}, not {}",
                next.revision
            ));
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|error| format!("could not create {}: {error}", self.root.display()))?;
        let bytes = serde_json::to_vec_pretty(next)
            .map_err(|error| format!("could not serialize the quota config: {error}"))?;
        let target = self.config_path();
        let tmp = target.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)
            .map_err(|error| format!("could not write {}: {error}", tmp.display()))?;
        std::fs::rename(&tmp, &target)
            .map_err(|error| format!("could not publish {}: {error}", target.display()))
    }
}

/// 本实例在整机活动表里的那一行，**持锁期间有效**。
///
/// `Drop` 关闭描述符即释放 `flock`，别的实例下一轮就看不到这一行。
pub struct ActivityClaim {
    _file: std::fs::File,
    path: PathBuf,
}

impl ActivityClaim {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 本实例的活动登记。持有它 = 声明"我还活着，我有 `sessions` 个会话在跑"。
pub struct LocalActivity {
    domain: QuotaDomain,
    owner: String,
    claim: Option<ActivityClaim>,
    sessions: u32,
}

impl LocalActivity {
    pub fn new(domain: QuotaDomain, owner: impl Into<String>) -> LocalActivity {
        LocalActivity {
            domain,
            owner: owner.into(),
            claim: None,
            sessions: 0,
        }
    }

    /// 更新本实例的会话数并确保登记文件存在且被本进程持锁。
    ///
    /// 每次调用都重写计数（同一把锁、同一个文件），所以文件内容总是最新值；
    /// 锁只在第一次建立，之后一直持有——中途释放再拿会让别人短暂看到"这个
    /// 实例没了"，从而多发名额。
    pub fn publish(&mut self, sessions: u32) -> Result<(), String> {
        use std::os::unix::io::AsRawFd;

        self.sessions = sessions;
        if self.claim.is_none() {
            let dir = self.domain.activity_dir();
            std::fs::create_dir_all(&dir)
                .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
            let path = dir.join(format!("{}.activity", sanitize(&self.owner)));
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(|error| format!("could not open {}: {error}", path.display()))?;
            let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if outcome != 0 {
                // 同名登记被别人占着：说明同名 owner 还活着。这不是常见路径
                // （owner 里带 pid），报出来比静默覆盖安全。
                let error = std::io::Error::last_os_error();
                return Err(format!(
                    "activity file {} is held by another live instance ({error}); \
                     two instances must not share one owner name",
                    path.display()
                ));
            }
            self.claim = Some(ActivityClaim { _file: file, path });
        }
        let claim = self.claim.as_ref().expect("just established");
        // 计数写在锁内：读到旧值的唯一可能是"上一个持有者已死"，那种情况下
        // 我们的锁已经拿到了，因此这里的写入必然是最新的。
        std::fs::write(&claim.path, self.sessions.to_string())
            .map_err(|error| format!("could not write {}: {error}", claim.path.display()))
    }

    /// 本实例最后一次发布的会话数。
    pub fn sessions(&self) -> u32 {
        self.sessions
    }

    /// 显式放弃登记（正常关停时调用）。`Drop` 也会做同样的事，这里只是想
    /// 让"关闭实例"在日志里可见。
    pub fn release(&mut self) {
        self.claim = None;
    }
}

/// 整机观测到的一次活动快照。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineActivity {
    /// 每个活实例报告的会话数，按 owner 去重（同一 owner 取最大值，不累加）。
    pub per_owner: BTreeMap<String, u32>,
    /// 本次扫描时**无法确认存活**的登记数（死实例留下的文件）。只为诊断；
    /// 回收靠的是"拿得到锁"，不是这个数字。
    pub dead_claims: usize,
}

impl MachineActivity {
    pub fn total(&self) -> u32 {
        self.per_owner.values().copied().sum()
    }

    pub fn is_empty(&self) -> bool {
        self.per_owner.is_empty()
    }
}

/// 扫一遍整机活动表，只计**活着的**实例。
///
/// 判据只有一条：能不能在它的登记文件上拿到 `flock`。拿得到 = 那人没了
/// （内核已释放），跳过；拿不到 = 活着，计入它写下的计数。
///
/// **不按 mtime / TTL 判断**：一个实例可能在等用户审批、被 `SIGSTOP` 暂停，
/// 这些都让它"很久没动"，但它确实还开着会话。按时间回收会把它的额度再发
/// 一次，于是整机超发。
pub fn observe_machine(domain: &QuotaDomain) -> Result<MachineActivity, String> {
    use std::os::unix::io::AsRawFd;

    let dir = domain.activity_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MachineActivity::default());
        }
        Err(error) => return Err(format!("could not read {}: {error}", dir.display())),
    };

    let mut activity = MachineActivity::default();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("activity") {
            continue;
        }
        let Some(owner) = path
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(probe) = std::fs::OpenOptions::new().write(true).open(&path) else {
            // 打不开不能证明"没人持有"，保守地按仍然活着处理：多算一个人
            // 只是少发名额，少算一个人会让整机超发。
            activity.dead_claims += 0;
            let count = read_count(&path);
            activity
                .per_owner
                .entry(owner)
                .and_modify(|existing| *existing = (*existing).max(count))
                .or_insert(count);
            continue;
        };
        let outcome = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if outcome == 0 {
            // 拿到锁：持有者已死。立刻放开，不占用这个文件。
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
            activity.dead_claims += 1;
            continue;
        }
        let error = std::io::Error::last_os_error();
        let contended = matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        );
        if !contended {
            // 真故障：不能当成"没人"，按活着计入。
            let count = read_count(&path);
            activity
                .per_owner
                .entry(owner)
                .and_modify(|existing| *existing = (*existing).max(count))
                .or_insert(count);
            continue;
        }
        let count = read_count(&path);
        activity
            .per_owner
            .entry(owner)
            .and_modify(|existing| *existing = (*existing).max(count))
            .or_insert(count);
    }
    Ok(activity)
}

/// 读登记文件里的计数。读不到或解析不了按 0 计：**不**按 1 计，因为把
/// 一个坏的登记当成"有一人在跑"会让名额永久少一个；而 0 最坏只是少算它
/// 自己，它自己的会话数始终由 `own` 兜底。
fn read_count(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// 本拍还能起几个会话。
///
/// `observed` 是整机观测，`own` 是本进程上报的会话数。用
/// `observed.total().max(own)` 而**不是相加**：
///
/// - 本进程的会话本来就已登记在自己的 activity 行里，相加就是双扣；
/// - 拿不到自己那一行时（活动目录不可写，或老版本只写 sessions），`own`
///   才是唯一来源；
/// - 取 `max` 两种情形都对：有登记时二者相等，缺登记时退化为 `own`。
///
/// 配置缺失返回 0：没人授权过整机配额，就不该自动起任务。
pub fn remaining(
    domain: &QuotaDomain,
    observed: &MachineActivity,
    own: u32,
) -> Result<u32, String> {
    let Some(config) = domain.read_config()? else {
        return Ok(0);
    };
    let in_use = observed.total().max(own);
    Ok(config.max_concurrent.saturating_sub(in_use))
}

/// 文件名投影：可读前缀 + 摘要。
///
/// 只保留可读前缀会让 `a-b` 与 `a_b`、`p/a` 与 `p_a` 撞到同一个文件——两个
/// 不同实例共用一个活动登记，其中一个的计数会覆盖另一个。尾部摘要保证唯一。
fn sanitize(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let mut readable: String = key
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    if readable.is_empty() {
        readable.push_str("instance");
    }
    format!("{readable}-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等整机活动表收敛到 `expected` 总数。
    ///
    /// 为什么不是一拍断言：`flock` 的释放与「探测」之间没有同步保证。进程退出
    /// 后内核异步释放锁；`fork` 与 `exec` 之间的子进程还与本进程共享同一个打开
    /// 文件描述（`cargo test` 并行跑时，别的用例正在 `spawn git`，正好落在这个
    /// 窗口里）。两者都让「刚刚关闭的实例」短暂地仍被判为活着。
    ///
    /// 这是 flock 的固有性质，不是泄漏：生产的调用方（每拍一次的观测）同样只
    /// 依赖最终一致。所以测试断言**有界收敛**，否则会随机红——那是把测试的
    /// 疏忽当成实现的缺陷。
    fn wait_until_total(domain: &QuotaDomain, expected: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let total = observe_machine(domain).unwrap().total();
            if total == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "machine activity never converged to {expected} sessions; last saw {total}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn domain() -> (tempfile::TempDir, QuotaDomain) {
        let dir = tempfile::tempdir().unwrap();
        let domain = QuotaDomain::resolve(dir.path()).expect("a home always resolves");
        (dir, domain)
    }

    #[test]
    fn a_missing_config_grants_no_new_automatic_work() {
        // 没有配置 = 没人授权过整机配额。默认拒绝，而不是"没有上限"。
        let (_dir, domain) = domain();
        assert!(domain.read_config().unwrap().is_none());
        assert_eq!(
            remaining(&domain, &MachineActivity::default(), 0).unwrap(),
            0
        );
    }

    #[test]
    fn a_corrupt_config_is_reported_instead_of_treated_as_absent() {
        let (_dir, domain) = domain();
        std::fs::create_dir_all(&domain.root).unwrap();
        std::fs::write(domain.config_path(), b"{ not json").unwrap();
        assert!(
            domain.read_config().is_err(),
            "treating a corrupt file as absent invites overwriting another instance's limit"
        );
    }

    #[test]
    fn a_stale_revision_cannot_overwrite_a_newer_limit() {
        let (_dir, domain) = domain();
        domain
            .write_config(
                0,
                &SharedQuotaConfig {
                    revision: 1,
                    max_concurrent: 4,
                    max_starts_per_hour: 40,
                },
            )
            .unwrap();
        // 另一个实例推进到修订 2。
        domain
            .write_config(
                1,
                &SharedQuotaConfig {
                    revision: 2,
                    max_concurrent: 8,
                    max_starts_per_hour: 80,
                },
            )
            .unwrap();
        // 第一个实例还拿着修订 1 想写。
        assert!(
            domain
                .write_config(
                    1,
                    &SharedQuotaConfig {
                        revision: 2,
                        max_concurrent: 1,
                        max_starts_per_hour: 1,
                    },
                )
                .is_err()
        );
        assert_eq!(domain.read_config().unwrap().unwrap().max_concurrent, 8);
    }

    #[test]
    fn a_write_must_advance_the_revision_by_exactly_one() {
        let (_dir, domain) = domain();
        domain
            .write_config(
                0,
                &SharedQuotaConfig {
                    revision: 1,
                    max_concurrent: 4,
                    max_starts_per_hour: 40,
                },
            )
            .unwrap();
        assert!(
            domain
                .write_config(
                    1,
                    &SharedQuotaConfig {
                        revision: 7,
                        max_concurrent: 4,
                        max_starts_per_hour: 40,
                    },
                )
                .is_err(),
            "jumping revisions makes conflict detection meaningless"
        );
    }

    #[test]
    fn a_live_instance_contributes_its_session_count() {
        let (_dir, domain) = domain();
        let mut activity = LocalActivity::new(domain.clone(), "instance-a");
        activity.publish(3).unwrap();

        let observed = observe_machine(&domain).unwrap();
        assert_eq!(observed.total(), 3);
        assert_eq!(observed.dead_claims, 0);
    }

    #[test]
    fn a_zero_session_instance_is_alive_but_contributes_nothing() {
        let (_dir, domain) = domain();
        let mut activity = LocalActivity::new(domain.clone(), "instance-a");
        activity.publish(0).unwrap();

        let observed = observe_machine(&domain).unwrap();
        assert_eq!(observed.total(), 0);
        assert_eq!(observed.dead_claims, 0, "it is alive, not a dead claim");
    }

    #[test]
    fn a_dropped_instance_stops_counting() {
        // 实例关闭不能留下虚假活动。`Drop` 释放 flock 后，别人扫目录应当
        // 立刻把它算成死登记，而不是继续按它写的数字扣额度。
        let (_dir, domain) = domain();
        {
            let mut activity = LocalActivity::new(domain.clone(), "instance-a");
            activity.publish(5).unwrap();
            wait_until_total(&domain, 5);
        }
        wait_until_total(&domain, 0);
        assert_eq!(
            observe_machine(&domain).unwrap().dead_claims,
            1,
            "the closed instance must be reported as a dead claim, not silently ignored"
        );
    }

    #[test]
    fn two_instances_share_one_budget_instead_of_each_getting_a_full_one() {
        let (_dir, domain) = domain();
        domain
            .write_config(
                0,
                &SharedQuotaConfig {
                    revision: 1,
                    max_concurrent: 4,
                    max_starts_per_hour: 40,
                },
            )
            .unwrap();

        let mut a = LocalActivity::new(domain.clone(), "instance-a");
        let mut b = LocalActivity::new(domain.clone(), "instance-b");
        a.publish(3).unwrap();
        b.publish(1).unwrap();

        let observed = observe_machine(&domain).unwrap();
        assert_eq!(observed.total(), 4);
        // 两个实例看到同一份"已满"，因此都没有余量。
        assert_eq!(remaining(&domain, &observed, 3).unwrap(), 0);
        assert_eq!(remaining(&domain, &observed, 1).unwrap(), 0);
    }

    #[test]
    fn own_sessions_are_not_added_on_top_of_the_observation() {
        let (_dir, domain) = domain();
        domain
            .write_config(
                0,
                &SharedQuotaConfig {
                    revision: 1,
                    max_concurrent: 4,
                    max_starts_per_hour: 40,
                },
            )
            .unwrap();

        let mut a = LocalActivity::new(domain.clone(), "instance-a");
        a.publish(3).unwrap();

        let observed = observe_machine(&domain).unwrap();
        // `own` 已经在观测里：相加会算成 6，把"还有一个名额"变成"一个都没有"。
        assert_eq!(remaining(&domain, &observed, 3).unwrap(), 1);
        // 观测不到自己时（老宿主），`own` 是唯一来源。
        assert_eq!(
            remaining(&domain, &MachineActivity::default(), 3).unwrap(),
            1
        );
    }

    #[test]
    fn a_limit_below_usage_saturates_at_zero() {
        let (_dir, domain) = domain();
        domain
            .write_config(
                0,
                &SharedQuotaConfig {
                    revision: 1,
                    max_concurrent: 2,
                    max_starts_per_hour: 40,
                },
            )
            .unwrap();
        let mut a = LocalActivity::new(domain.clone(), "instance-a");
        a.publish(5).unwrap();
        assert_eq!(
            remaining(&domain, &observe_machine(&domain).unwrap(), 0).unwrap(),
            0,
            "saturating subtraction, never a wrapped huge remainder"
        );
    }

    #[test]
    fn two_owner_names_that_differ_only_in_separators_do_not_share_a_file() {
        let (_dir, domain) = domain();
        let mut a = LocalActivity::new(domain.clone(), "a-b");
        let mut b = LocalActivity::new(domain.clone(), "a_b");
        a.publish(2).unwrap();
        b.publish(3).unwrap();
        let observed = observe_machine(&domain).unwrap();
        assert_eq!(
            observed.total(),
            5,
            "distinct instances must not overwrite each other's count"
        );
    }

    #[test]
    fn a_second_instance_with_the_same_owner_is_refused() {
        // 同名 owner 会被同一把锁挡住。静默共用一份登记会让两个实例的计数
        // 互相覆盖，最终少算。
        let (_dir, domain) = domain();
        let mut a = LocalActivity::new(domain.clone(), "shared-name");
        a.publish(1).unwrap();
        let mut b = LocalActivity::new(domain.clone(), "shared-name");
        assert!(b.publish(2).is_err());
    }
}
