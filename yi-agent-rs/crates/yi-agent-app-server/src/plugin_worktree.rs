//! 插件 worktree 的**通用路径约定**：`<project>/.worktrees/<namespace>/<slug>`。
//!
//! 一个插件替用户划出的隔离 worktree 不是「项目」：它没有组长，也不该在侧栏里
//! 顶层成组。宿主需要一个判据把它们与**用户自己的** `.worktrees/<branch>` 分开
//! ——后者在 `.worktrees` 之后只有一段，前者有两段。
//!
//! # 归属裁定（spec §5.2，本任务落地）
//!
//! spec §5.2 给了三选一，本模块取 **(a)：保留为通用的「插件 worktree 路径前缀」
//! 约定，只做命名中性化**。理由：
//!
//! - **(b) 改读会话 meta（`driver_project`）不可行**：判定必须能作用于**任意**
//!   一条 meta，包括真机上已经落错的幽灵组长（`role: "lead"` 且
//!   `driver_project: None`）。它们**只有路径这条线索**，读 meta 就漏掉它们，
//!   而正是这些幽灵让侧栏出现「带组长行的顶层项目组」。
//! - **(c) 前缀由插件清单声明**在判据上是更干净的形态，但本判据是**纯路径函数**，
//!   被 `is_lead_meta` / `listAll` 按 meta **逐条**调用；改成清单驱动就要在这些
//!   热路径里插入每个 meta 一次目录读取 + JSON 解析，并引入新的失败模式
//!   （清单目录不可读时判定会翻转）。收益（消掉一个字面量）不抵这份风险。
//!
//! 因此命名空间留作本模块的**单一常量**，把「宿主源码里出现插件名」压缩到一处，
//! 并在此处显式记账为**必要兼容**（spec §6.2 验收 grep 的豁免项，见 Task 6 报告）。
//! 未来若插件清单要声明前缀（§5.2 方案 c），改这里一个常量即可，调用点不动。

use std::path::{Path, PathBuf};

/// 插件 worktree 的保留命名空间（`.worktrees/<namespace>/<slug>`）。
///
/// 通用机制 + 一个**当前保留的名字**：宿主要能在纯路径判定里认出「这是插件划的
/// 领地」，就必须知道保留名；名字来自当前唯一的此类插件。
///
/// **必要兼容（验收 §6.2 的单行豁免）**：本行含插件名，故会命中
/// `grep -rni "kanban\|board\|卡片"`。不能靠删掉它来「归零」——删掉就等于删掉
/// 幽灵组长的过滤与 worktree 回收的判据（真机上已有 9 例幽灵组长）。
///
/// 同时它也定义了分支名前缀（`<namespace>/<slug>`）。分支名是**与插件共享的
/// 契约**：宿主按它推导「是否已并入」，拼错会让「已并入」被判成「分支缺失」
/// （安全侧失败，但会漏删 worktree）。
pub const NAMESPACE: &str = "kanban";

/// 合并轮把 base 检出到的**兄弟**命名空间：`.worktrees/<NAMESPACE>-merge/<slug>`。
///
/// 与 [`NAMESPACE`] 同属保留领地，同样不是项目。它是 `"<NAMESPACE>-"` 前缀族的
/// 一员，判定里按前缀收（见 [`is_plugin_worktree`]）。
const MERGE_NAMESPACE_SUFFIX: &str = "-merge";

/// `<project>/.worktrees`：所有 worktree 的父目录。
fn worktrees_root(project: &Path) -> PathBuf {
    project.join(".worktrees")
}

/// 插件 worktree 的落点目录：`<project>/.worktrees/<NAMESPACE>`。
pub fn worktree_dir(project: &Path) -> PathBuf {
    worktrees_root(project).join(NAMESPACE)
}

/// 一个 slug 对应的分支名：`<NAMESPACE>/<slug>`。
///
/// `slug` 由调用方用 [`crate::worktree_reclaim::slugify`] 归一（与插件逐字一致）。
pub fn branch_name(slug: &str) -> String {
    format!("{NAMESPACE}/{slug}")
}

/// `cwd` 是否落在插件 worktree 领地内。
///
/// 判据：路径中出现 `.worktrees` 之后**紧跟**的那一段是保留命名空间
/// （`NAMESPACE` 本身或其 `-` 前缀族，如 `<NAMESPACE>-merge`）。用户自己的
/// `.worktrees/<branch>` 只有一段、且名字不是保留名，故不受影响；
/// `/p/kanban/x` 这类「目录名恰好叫 kanban 但不在 `.worktrees` 下」也不算。
pub fn is_plugin_worktree(cwd: &Path) -> bool {
    let mut saw_worktrees = false;
    for part in cwd.components() {
        let std::path::Component::Normal(part) = part else {
            continue;
        };
        let part = part.to_string_lossy();
        if saw_worktrees && is_reserved_namespace(&part) {
            return true;
        }
        saw_worktrees = part == ".worktrees";
    }
    false
}

/// `.worktrees` 之后那一段是否是一个保留命名空间。
fn is_reserved_namespace(part: &str) -> bool {
    part == NAMESPACE
        || part == format!("{NAMESPACE}{MERGE_NAMESPACE_SUFFIX}")
        || part.starts_with(&format!("{NAMESPACE}-"))
}

/// 按**名字**在 `PATH` 上解析一个插件可执行文件。
///
/// 通用：调用方给插件名（`plugin/install` 的请求参数），宿主不知道那个插件是
/// 干什么的。之所以要绝对路径：supervisor 清单里的 `command` 需要一条能解析的
/// 命令，而裸插件名未必能。
///
/// 解析规则（与已删的那个看板 registry crate 的同名逻辑逐字一致，故插件装在哪儿
/// 都不受影响）：Homebrew 前缀 (`/opt/homebrew/bin/<name>`) 优先，其次 `PATH` 上
/// 第一个存在的同名可执行文件。
///
/// 解析不到**不是错误**：调用方回 `plugin_not_installed`，宿主自己照常运行。
pub fn resolve_plugin_command(name: &str) -> Option<PathBuf> {
    let homebrew = PathBuf::from("/opt/homebrew/bin").join(name);
    if homebrew.is_file() {
        return Some(homebrew);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|entry| entry.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_and_merge_worktrees_are_in_the_reserved_land() {
        assert!(is_plugin_worktree(Path::new("/p/.worktrees/kanban/card-1")));
        assert!(is_plugin_worktree(Path::new("/p/.worktrees/kanban")));
        assert!(is_plugin_worktree(Path::new("/p/.worktrees/kanban-merge")));
        assert!(is_plugin_worktree(Path::new(
            "/p/.worktrees/kanban-merge/card-1"
        )));
    }

    #[test]
    fn a_users_own_worktree_is_not_reserved_land() {
        assert!(!is_plugin_worktree(Path::new("/p")));
        assert!(!is_plugin_worktree(Path::new("/p/.worktrees/other/x")));
        assert!(!is_plugin_worktree(Path::new("/p/.worktrees/feature-x")));
        // 只有 `.worktrees` 之后**紧跟**的那一段才算：目录名碰巧叫 kanban 不算。
        assert!(!is_plugin_worktree(Path::new("/p/kanban/x")));
        // 前缀必须整段匹配，`kanbanx` 不是保留名的 `-` 前缀族成员。
        assert!(!is_plugin_worktree(Path::new("/p/.worktrees/kanbanx/x")));
    }

    /// 解析不到就回 `None`，不 panic：某个插件没装不该让宿主起不来。
    #[test]
    fn an_unknown_plugin_name_resolves_to_nothing() {
        assert_eq!(
            resolve_plugin_command("definitely-not-a-real-plugin-xyz"),
            None
        );
    }

    /// 名字存在时解析出的是**绝对路径**并以该名字结尾（`PATH` 上必有一个，如
    /// `sh`）。这条钉的是「解析规则真的看 `PATH`」，不是 Homebrew 的硬编码。
    #[test]
    fn a_name_on_the_path_resolves_to_an_absolute_path() {
        let resolved = resolve_plugin_command("sh").expect("`sh` is on every PATH");
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert!(resolved.ends_with("sh"), "{resolved:?}");
    }

    #[test]
    fn the_worktree_dir_and_branch_name_share_one_namespace() {
        assert_eq!(
            worktree_dir(Path::new("/p")),
            PathBuf::from("/p/.worktrees/kanban")
        );
        assert_eq!(branch_name("card-1"), "kanban/card-1");
    }
}
