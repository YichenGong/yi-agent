//! 项目授权：代次、能力集合与重新确认。
//!
//! # 三层身份
//!
//! 设计 §4 把运行绑定拆成四件事：宿主身份、发布摘要、激活代次、项目 +
//! **项目授权代次**。这里管后两者：
//!
//! - **项目授权代次**在该项目每次启用、停用或变更授权时递增。它只让
//!   **该项目**的旧绑定失效——撤销 A 项目不能顺手让 B 项目的绑定失效，
//!   否则用户在 A 里改个设置就把 B 正在跑的任务打断了。
//! - **激活代次**是实例级的，由 T11/T12 管理。这里不碰它。
//!
//! # 能力只增不自动批
//!
//! 新版本需要更多能力时，已授权的项目**不会**因为升级获得新能力
//! （设计 §6：「能力增加的更新须对受影响项目重新确认；不得沿用旧授权自动
//! 扩大能力」）。未批的新能力让该项目进入「待重新确认」，而不是静默拒绝
//! 或静默放行。
//!
//! 反过来，一直存在的旧权限不能无故重置：把用户已批准的 `Write` 打回
//! 未批准，会让每次升级都像第一次启用。

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::project::{ProjectResolver, is_independent};
use crate::types::{Effect, ProjectId, ReleaseId};

/// 一个项目对一个插件的授权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub plugin: String,
    pub project: ProjectId,
    /// 授权时的发布身份。换了版本要重新判断能力差异。
    pub release: ReleaseId,
    /// 项目授权代次。每次变更递增，旧绑定随之失效。
    pub generation: u64,
    /// 已批准的能力。
    pub granted: Vec<Effect>,
    /// 已批准、但新版本需要而尚未确认的能力。非空即「待重新确认」。
    #[serde(default)]
    pub pending: Vec<Effect>,
    /// 授权对应的宿主身份。不同宿主不共享授权。
    pub host_id: String,
}

impl Grant {
    /// 是否已有待确认的能力增加。
    pub fn needs_reconfirmation(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 该能力的来源：基本授权还是需要独立确认。
    pub fn is_independent(&self, effect: Effect) -> bool {
        is_independent(effect)
    }
}

/// 授权表。按 `(plugin, project)` 索引。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GrantTable {
    grants: Vec<Grant>,
}

impl GrantTable {
    pub fn new() -> Self {
        GrantTable::default()
    }

    pub fn all(&self) -> &[Grant] {
        &self.grants
    }

    pub fn get(&self, plugin: &str, project: &ProjectId) -> Option<&Grant> {
        self.grants
            .iter()
            .find(|g| g.plugin == plugin && &g.project == project)
    }

    /// 启用一个项目。已存在则视为幂等，不递增代次。
    ///
    /// 递增代次只能由**变更**触发：一次重复的"启用"若也递增，客户端刚拿到
    /// 的绑定会在没有任何变更的情况下失效。
    pub fn grant(
        &mut self,
        plugin: &str,
        project: ProjectId,
        release: &ReleaseId,
        host_id: &str,
        capabilities: &[Effect],
    ) -> Result<Grant, PluginError> {
        if let Some(existing) = self
            .grants
            .iter_mut()
            .find(|g| g.plugin == plugin && g.project == project)
        {
            // 同一宿主、同一版本、能力已覆盖 → 幂等。
            if existing.host_id == host_id
                && &existing.release == release
                && capabilities.iter().all(|c| existing.granted.contains(c))
            {
                return Ok(existing.clone());
            }
            // 否则是一次真实变更：合并能力、递增代次、清空待确认。
            existing.host_id = host_id.to_string();
            existing.release = release.clone();
            existing.generation += 1;
            for capability in capabilities {
                if !existing.granted.contains(capability) {
                    existing.granted.push(*capability);
                }
            }
            existing.pending.clear();
            return Ok(existing.clone());
        }

        let grant = Grant {
            plugin: plugin.to_string(),
            project,
            release: release.clone(),
            generation: 1,
            granted: capabilities.to_vec(),
            pending: Vec::new(),
            host_id: host_id.to_string(),
        };
        self.grants.push(grant.clone());
        Ok(grant)
    }

    /// 停用并撤销。只递增**该项目**的代次，其他项目不受影响。
    pub fn revoke(&mut self, plugin: &str, project: &ProjectId) -> Result<u64, PluginError> {
        let grant = self
            .grants
            .iter_mut()
            .find(|g| g.plugin == plugin && &g.project == project)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{plugin}` has no grant for project `{}`",
                    project.as_str()
                ))
            })?;
        // 撤销后仍保留记录但清空能力：代次必须前进，旧绑定才会失效。
        // 直接删记录会让"旧代次的绑定"无法被识别为「已撤销」而只能
        // 被判成「未知项目」，两种错误的处置方式不同。
        grant.generation += 1;
        grant.granted.clear();
        grant.pending.clear();
        Ok(grant.generation)
    }

    /// 版本升级时比较能力差异。
    ///
    /// 旧能力保留（不无故重置），新增能力进入 `pending`（不自动批准）。
    pub fn compare_capabilities(
        &mut self,
        plugin: &str,
        project: &ProjectId,
        new_release: &ReleaseId,
        new_capabilities: &[Effect],
    ) -> Result<Grant, PluginError> {
        let grant = self
            .grants
            .iter_mut()
            .find(|g| g.plugin == plugin && &g.project == project)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{plugin}` has no grant for project `{}`",
                    project.as_str()
                ))
            })?;

        grant.release = new_release.clone();
        grant.pending = new_capabilities
            .iter()
            .filter(|c| !grant.granted.contains(c))
            .copied()
            .collect();
        Ok(grant.clone())
    }

    /// 检查一次调用是否被允许。
    ///
    /// `effect` 必须来自**签名清单**，不接受调用方自报：插件把 `Write`
    /// 说成 `Read` 就能绕过授权（设计 §6 的要求）。
    pub fn check(
        &self,
        plugin: &str,
        project: &ProjectId,
        effect: Effect,
        host_id: &str,
        generation: u64,
    ) -> Result<(), PluginError> {
        let grant = self.get(plugin, project).ok_or_else(|| {
            PluginError::PermissionDenied(format!(
                "plugin `{plugin}` is not authorized for project `{}`",
                project.as_str()
            ))
        })?;

        // 宿主身份不匹配：授权记在执行宿主上，别的宿主不能拿来用。
        if grant.host_id != host_id {
            return Err(PluginError::PermissionDenied(format!(
                "grant belongs to host `{}`, not `{host_id}`",
                grant.host_id
            )));
        }

        // 代次不符 = 旧绑定。必须在能力检查之前判：一个已撤销的授权即使
        // 能力还在，也不能继续用。
        if grant.generation != generation {
            return Err(PluginError::StaleBinding(format!(
                "binding carries grant generation {generation}, but project `{}` is at {}",
                project.as_str(),
                grant.generation
            )));
        }

        if !grant.granted.contains(&effect) {
            return Err(PluginError::PermissionDenied(format!(
                "plugin `{plugin}` is not authorized for {:?} in project `{}`{}",
                effect,
                project.as_str(),
                if grant.pending.contains(&effect) {
                    " (pending reconfirmation after a capability increase)"
                } else {
                    ""
                }
            )));
        }

        // 待确认的能力即使出现在 granted 里也不放行——升级不该顺手扩权。
        if grant.pending.contains(&effect) {
            return Err(PluginError::PermissionDenied(format!(
                "plugin `{plugin}` needs reconfirmation for {:?} in project `{}`",
                effect,
                project.as_str()
            )));
        }

        Ok(())
    }

    /// 便捷版：按路径检查，内部走 `ProjectResolver` 规范化。
    pub fn check_path(
        &self,
        resolver: &ProjectResolver,
        plugin: &str,
        path: &std::path::Path,
        effect: Effect,
        host_id: &str,
        generation: u64,
    ) -> Result<ProjectId, PluginError> {
        let project = resolver.resolve(path)?;
        self.check(plugin, &project, effect, host_id, generation)?;
        Ok(project)
    }
}
