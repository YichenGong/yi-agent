//! 开关操作的错误。故意很小：只有「不认识的键」和「写不进去」两种。

use std::fmt;

#[derive(Debug)]
pub enum FlagError {
    /// 名字不在注册表里。写入时必须拒绝——否则一个拼写错误会表现成
    /// 「开了但没生效」，极难排查。
    UnknownFlag(String),
    /// 落盘失败。
    Io(std::io::Error),
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlagError::UnknownFlag(name) => {
                write!(f, "unknown flag `{name}`: it is not in the registry")
            }
            FlagError::Io(e) => write!(f, "could not write flags.json: {e}"),
        }
    }
}

impl std::error::Error for FlagError {}
