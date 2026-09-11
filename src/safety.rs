//! 安全检查。
//!
//! 迁移不是删除，危险度本身低一些 —— 但「原地留链接」这个动作意味着源路径
//! 会被替换掉，搬错了系统目录同样会让机器起不来。这里拦住几类明确不该碰的
//! 情况，其余交给用户判断（可用 `--force` 越过）。
//!
//! 另有一类检查**不允许 `--force` 越过**：自吞（target 在 source 内部）之类
//! 会直接导致数据损坏的逻辑错误，那不是「危险但用户知道自己在干嘛」，
//! 而是纯粹的错误。这类检查在 [`crate::migrate`] 里做。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 检查结果。
#[derive(Debug)]
pub enum Verdict {
    Safe,
    /// 危险但用户可以用 `--force` 越过
    Risky(String),
    /// 绝对禁止，`--force` 也不行
    Forbidden(String),
}

/// 对源路径做安全检查。
pub fn check_source(path: &Path) -> Verdict {
    if let Some(reason) = is_volume_root(path) {
        return Verdict::Forbidden(reason);
    }
    if let Some(reason) = is_system_directory(path) {
        return Verdict::Forbidden(reason);
    }
    if let Some(reason) = is_home_directory(path) {
        return Verdict::Risky(reason);
    }
    if let Some(reason) = contains_current_dir(path) {
        return Verdict::Risky(reason);
    }
    Verdict::Safe
}

/// 把检查结果转成错误，或在 `force` 下放行。
pub fn enforce(path: &Path, force: bool) -> Result<()> {
    match check_source(path) {
        Verdict::Safe => Ok(()),
        Verdict::Forbidden(reason) => Err(Error::forbidden(path, reason)),
        Verdict::Risky(reason) => {
            if force {
                eprintln!("cshl: ⚠️  {}（已被 --force 放行）", reason);
                Ok(())
            } else {
                Err(Error::risky(path, reason))
            }
        }
    }
}

/// 卷根：搬走整个卷毫无意义，而且必然失败或造成灾难。
fn is_volume_root(path: &Path) -> Option<String> {
    let canonical = canonical_or_self(path);

    #[cfg(unix)]
    {
        if canonical == Path::new("/") {
            return Some("这是根目录".to_string());
        }
        // /Volumes/xxx 或 /mnt/xxx 这类挂载点本身
        if let Some(parent) = canonical.parent() {
            if parent == Path::new("/Volumes") || parent == Path::new("/mnt") {
                return Some(format!("'{}' 是一个卷的挂载点", canonical.display()));
            }
        }
    }

    #[cfg(windows)]
    {
        // C:\ 或 \\?\C:\
        let s = canonical.to_string_lossy();
        let trimmed = s.trim_start_matches(r"\\?\");
        if trimmed.len() <= 3 && trimmed.ends_with(":\\") {
            return Some("这是一个驱动器的根目录".to_string());
        }
    }

    None
}

/// 系统关键目录。
fn is_system_directory(path: &Path) -> Option<String> {
    let canonical = canonical_or_self(path);

    #[cfg(unix)]
    const PROTECTED: &[&str] = &[
        "/bin",
        "/boot",
        "/dev",
        "/etc",
        "/lib",
        "/lib64",
        "/proc",
        "/root",
        "/sbin",
        "/sys",
        "/usr",
        "/var",
        "/System",
        "/Library",
        "/Applications",
        "/private",
        "/cores",
        "/opt",
    ];

    #[cfg(windows)]
    const PROTECTED: &[&str] = &[
        r"C:\Windows",
        r"C:\Program Files",
        r"C:\Program Files (x86)",
        r"C:\ProgramData",
        r"C:\Users",
    ];

    let s = canonical.to_string_lossy();
    let s = s.trim_start_matches(r"\\?\");

    for p in PROTECTED {
        #[cfg(windows)]
        let hit = s.eq_ignore_ascii_case(p);
        #[cfg(not(windows))]
        let hit = s == *p;

        if hit {
            return Some(format!("'{}' 是系统关键目录", p));
        }
    }

    None
}

/// 用户主目录本身（搬它的子目录没问题，搬它本身风险很高）。
fn is_home_directory(path: &Path) -> Option<String> {
    let home = dirs::home_dir()?;
    if canonical_or_self(path) == canonical_or_self(&home) {
        return Some("这是你的用户主目录".to_string());
    }
    None
}

/// 源目录包含（或就是）当前工作目录 —— 搬走之后 shell 会处于一个
/// 已经不存在的目录里。
fn contains_current_dir(path: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = canonical_or_self(&cwd);
    let src = canonical_or_self(path);

    if cwd.starts_with(&src) {
        return Some(format!("'{}' 包含你当前所在的工作目录", src.display()));
    }
    None
}

/// 尽力规范化路径；路径不存在时退回原值。
///
/// 走 [`crate::link::canonicalize`] 而非 `std::fs::canonicalize`：后者在
/// Windows 上返回 `\\?\C:\...`，而失败时的退路是原样返回的普通形式 ——
/// 两者混在一起比较，`starts_with` 永远不成立，安全检查会静默失效。
pub fn canonical_or_self(path: &Path) -> PathBuf {
    crate::link::canonicalize(path).unwrap_or_else(|_| crate::platform::strip_verbatim(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_subdir_is_safe() {
        let d = std::env::temp_dir().join("cshell_safety_ok");
        let _ = std::fs::create_dir_all(&d);
        assert!(matches!(check_source(&d), Verdict::Safe));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn root_is_forbidden() {
        #[cfg(unix)]
        assert!(matches!(
            check_source(Path::new("/")),
            Verdict::Forbidden(_)
        ));
        #[cfg(windows)]
        assert!(matches!(
            check_source(Path::new(r"C:\")),
            Verdict::Forbidden(_)
        ));
    }

    #[test]
    fn system_dirs_are_forbidden() {
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(
                check_source(Path::new("/System")),
                Verdict::Forbidden(_)
            ));
            assert!(matches!(
                check_source(Path::new("/usr")),
                Verdict::Forbidden(_)
            ));
        }
        #[cfg(target_os = "linux")]
        {
            assert!(matches!(
                check_source(Path::new("/etc")),
                Verdict::Forbidden(_)
            ));
        }
        #[cfg(windows)]
        {
            assert!(matches!(
                check_source(Path::new(r"C:\Windows")),
                Verdict::Forbidden(_)
            ));
        }
    }

    #[test]
    fn forbidden_cannot_be_forced() {
        #[cfg(unix)]
        let p = Path::new("/");
        #[cfg(windows)]
        let p = Path::new(r"C:\");

        // 即便 force = true 也必须拒绝
        assert!(enforce(p, true).is_err());
    }

    #[test]
    fn risky_can_be_forced() {
        let home = dirs::home_dir().expect("需要主目录");
        assert!(matches!(check_source(&home), Verdict::Risky(_)));
        assert!(enforce(&home, false).is_err());
        assert!(enforce(&home, true).is_ok());
    }

    #[test]
    fn mount_point_itself_is_forbidden() {
        #[cfg(target_os = "macos")]
        {
            // /Volumes 下的挂载点本身不该被搬走
            let p = Path::new("/Volumes/SomeDisk");
            assert!(matches!(check_source(p), Verdict::Forbidden(_)));
        }
        #[cfg(target_os = "linux")]
        {
            let p = Path::new("/mnt/somedisk");
            assert!(matches!(check_source(p), Verdict::Forbidden(_)));
        }
    }
}
