//! 错误类型与退出码。
//!
//! 迁移工具的错误必须能区分「什么都没做」和「做了一半」——前者可以直接重试，
//! 后者需要人工介入或 `cshl list` 查台账。[`Error::Aborted`] 与
//! [`Error::PartialFailure`] 的区分就是为此。

use std::fmt;
use std::io;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// I/O 错误，尽可能带上出错的路径
    Io {
        path: Option<PathBuf>,
        source: io::Error,
    },
    /// 路径校验不通过（不存在、不是目录、自吞、target 已存在……）
    InvalidPath { path: PathBuf, reason: String },
    /// 安全检查拦截（系统目录、卷根等）。
    /// `can_force` 区分「危险但用户可以 --force 越过」与「绝对禁止」——
    /// 提示语必须据此不同，否则会引导用户去试一个注定失败的参数。
    Unsafe {
        path: PathBuf,
        reason: String,
        can_force: bool,
    },
    /// 空间不足：跨卷复制前的预检
    InsufficientSpace {
        needed: u64,
        available: u64,
        target: PathBuf,
    },
    /// 复制阶段失败并已回滚 —— **source 原封不动**，可直接重试
    Aborted {
        reason: String,
        failures: Vec<FailedItem>,
    },
    /// 删除阶段失败：数据已在 target，但 source 没清干净
    PartialFailure {
        total: usize,
        failed: usize,
        failures: Vec<FailedItem>,
    },
    /// 台账损坏或找不到对应记录
    Ledger(String),
}

/// 单个文件/目录的失败记录
#[derive(Debug, Clone)]
pub struct FailedItem {
    pub path: PathBuf,
    pub error: String,
    pub is_dir: bool,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => match path {
                Some(p) => write!(f, "I/O 错误 '{}': {}", p.display(), source),
                None => write!(f, "I/O 错误: {}", source),
            },
            Error::InvalidPath { path, reason } => {
                write!(f, "路径无效 '{}': {}", path.display(), reason)
            }
            Error::Unsafe { path, reason, .. } => {
                write!(f, "安全检查拒绝 '{}': {}", path.display(), reason)
            }
            Error::InsufficientSpace {
                needed,
                available,
                target,
            } => write!(
                f,
                "目标卷空间不足 '{}': 需要 {}，可用 {}",
                target.display(),
                human_bytes(*needed),
                human_bytes(*available)
            ),
            Error::Aborted { reason, failures } => {
                write!(f, "迁移已中止，源目录未被改动: {}", reason)?;
                if !failures.is_empty() {
                    write!(f, "（{} 项失败）", failures.len())?;
                }
                Ok(())
            }
            Error::PartialFailure { total, failed, .. } => write!(
                f,
                "源目录清理未完成: {}/{} 项删除失败（数据已在目标位置）",
                failed, total
            ),
            Error::Ledger(msg) => write!(f, "台账错误: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Error::Io { path: None, source }
    }
}

impl Error {
    /// 附带路径上下文的 I/O 错误
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io {
            path: Some(path.into()),
            source,
        }
    }

    pub fn invalid(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::InvalidPath {
            path: path.into(),
            reason: reason.into(),
        }
    }

    /// 危险但可以用 `--force` 越过。
    pub fn risky(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Unsafe {
            path: path.into(),
            reason: reason.into(),
            can_force: true,
        }
    }

    /// 绝对禁止，`--force` 也不行。
    pub fn forbidden(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Error::Unsafe {
            path: path.into(),
            reason: reason.into(),
            can_force: false,
        }
    }

    /// 退出码：1 = 用户/校验问题，2 = I/O 故障，3 = 做了一半需人工介入
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::InvalidPath { .. }
            | Error::Unsafe { .. }
            | Error::InsufficientSpace { .. }
            | Error::Ledger(_) => 1,
            Error::Io { .. } | Error::Aborted { .. } => 2,
            Error::PartialFailure { .. } => 3,
        }
    }
}

/// 人类可读的字节数，用于报错与进度展示。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{} B", n);
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{:.2} {}", v, UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn exit_codes_distinguish_severity() {
        // 校验类问题 → 1，可以改参数重来
        assert_eq!(Error::invalid("/x", "不存在").exit_code(), 1);
        // 已中止但源完好 → 2，可以直接重试
        assert_eq!(
            Error::Aborted {
                reason: "复制失败".into(),
                failures: vec![]
            }
            .exit_code(),
            2
        );
        // 做了一半 → 3，需人工介入
        assert_eq!(
            Error::PartialFailure {
                total: 10,
                failed: 2,
                failures: vec![]
            }
            .exit_code(),
            3
        );
    }
}
