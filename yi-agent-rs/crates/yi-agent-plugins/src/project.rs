//! 项目规范化：路径 → `ProjectId`。
//!
//! # 为什么授权不能靠字符串前缀
//!
//! 设计 §6：项目授权基于**规范路径**和宿主身份，不向子目录、同名路径或
//! 其他宿主隐式传播；worktree 必须能解析回已授权项目，手动新建且未登记
//! 的目录不能靠前缀继承授权。所以这里做两件事：
//!
//! 1. `canonicalize`（跟随 symlink）得到真实路径。`/a/link` 与 `/a/repo`
//!    是同一个项目，只按字面比较会让同一个项目拿到两份互不相关的授权。
//! 2. 显式登记一份「项目根 → ProjectId」的记录。子目录**不会**自动获得
//!    父目录的授权；worktree 必须自己解析回已登记的根。
//!
//! # 同名目录不是同一个项目
//!
//! `/a/x` 与 `/b/x` 的 basename 都是 `x`。用 basename 当项目标识会在
//! 两个真实项目之间共享授权——这正是"不向同名路径传播"要防的。

use std::path::{Path, PathBuf};

use crate::error::PluginError;
use crate::types::ProjectId;

/// 已登记的项目根。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoot {
    /// 规范化后的绝对路径。
    pub canonical: PathBuf,
    pub id: ProjectId,
}

/// 项目解析器：持有已登记的根，把路径映射回项目。
#[derive(Debug, Clone, Default)]
pub struct ProjectResolver {
    roots: Vec<ProjectRoot>,
}

impl ProjectResolver {
    pub fn new() -> Self {
        ProjectResolver { roots: Vec::new() }
    }

    /// 登记一个项目根。路径必须存在，否则 `canonicalize` 无法给出真实路径。
    pub fn register(&mut self, path: &Path) -> Result<ProjectRoot, PluginError> {
        let canonical = canonicalize(path)?;
        // 同一个真实路径登记两次是幂等的：返回已有 id，不造第二个。
        if let Some(existing) = self.roots.iter().find(|r| r.canonical == canonical) {
            return Ok(existing.clone());
        }
        let id = ProjectId::parse(&canonical.to_string_lossy())?;
        let root = ProjectRoot { canonical, id };
        self.roots.push(root.clone());
        Ok(root)
    }

    /// 把任意路径解析回已登记的项目。
    ///
    /// 只接受**恰好等于**某个已登记根，或位于其下的路径。不按前缀字符串
    /// 比较，而是按路径分量比较——`/a/repo-evil` 不是 `/a/repo` 的子目录，
    /// 字符串 `starts_with` 会把它误判成子目录。
    ///
    /// 子目录解析到项目根是**允许**的（设计要求 worktree 能解析回已授权
    /// 项目），但授权本身仍记在根上：撤销根就撤销所有子目录。
    pub fn resolve(&self, path: &Path) -> Result<ProjectId, PluginError> {
        let canonical = canonicalize(path)?;
        self.roots
            .iter()
            .filter(|root| canonical.starts_with(&root.canonical))
            .max_by_key(|root| root.canonical.as_os_str().len())
            .map(|root| root.id.clone())
            .ok_or_else(|| {
                PluginError::PermissionDenied(format!(
                    "{} is not inside any registered project; a directory that was created by \
                     hand must be registered explicitly rather than inheriting a grant by path \
                     prefix",
                    canonical.display()
                ))
            })
    }

    /// 已登记的根，供持久化与诊断。
    pub fn roots(&self) -> &[ProjectRoot] {
        &self.roots
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, PluginError> {
    std::fs::canonicalize(path).map_err(|error| {
        PluginError::NotFound(format!("could not resolve {}: {error}", path.display()))
    })
}

/// 能力分类。设计 §6 把权限分成两组：
///
/// - **基本看板授权**：读项目数据、改插件队列。一次确认即可。
/// - **独立确认**：发起自动模型任务、修改整机配额、后台常驻。三者**分开**
///   确认，不把配额修改与常驻捆绑。
///
/// 这个划分是协议的一部分：把 `QuotaWrite` 归进基本授权，就等于用户同意
/// "让它在我的项目里跑"时顺手同意了"改我的整机配额"。
pub fn is_independent(capability: crate::types::Effect) -> bool {
    use crate::types::Effect;
    matches!(
        capability,
        Effect::Launch | Effect::QuotaWrite | Effect::Resident
    )
}
