//! 解包上限。
//!
//! 上限由宿主传入，但**插件不能抬高它**：上限若可被被检查方设置，就不再是上限。
//! 因此这里给出一份硬上限，任何超出它的配置直接判为配置错误。

/// 宿主可降低、插件不可提升的解包上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractLimits {
    pub max_archive_bytes: u64,
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_files: usize,
}

/// 硬上限。常见桌面包里最大的是前端资源与 sidecar，下面这些数字留了
/// 一个数量级的余量，同时远小于"把用户磁盘写满"。
const HARD_CEILING: ExtractLimits = ExtractLimits {
    max_archive_bytes: 256 * 1024 * 1024,
    max_total_bytes: 1024 * 1024 * 1024,
    max_file_bytes: 128 * 1024 * 1024,
    max_files: 20_000,
};

impl Default for ExtractLimits {
    fn default() -> Self {
        HARD_CEILING
    }
}

impl ExtractLimits {
    /// 是否越过了硬上限。越过说明调用方试图放宽边界。
    pub fn exceeds_hard_ceiling(&self) -> bool {
        self.max_archive_bytes > HARD_CEILING.max_archive_bytes
            || self.max_total_bytes > HARD_CEILING.max_total_bytes
            || self.max_file_bytes > HARD_CEILING.max_file_bytes
            || self.max_files > HARD_CEILING.max_files
    }

    /// 硬上限本身，用于错误信息与测试。
    pub fn hard_ceiling() -> Self {
        HARD_CEILING
    }
}
