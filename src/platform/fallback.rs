//! 其他 Unix（FreeBSD、illumos 等）的兜底实现。
//!
//! 只用 POSIX 保证存在的系统调用，不追求零拷贝 —— 这些平台不是主要目标，
//! 但要保证能编译、能正确工作。

use std::io;
use std::path::Path;

#[path = "unix_common.rs"]
mod common;

pub use common::{
    available_space, create_dir, create_dir_symlink, create_hard_link, is_cross_device, link_kind,
    read_dir, read_link_target, remove_dir, remove_file, volume_id,
};

use common::cpath;

/// 普通 `rename(2)`。
///
/// POSIX 的 `rename` 会**静默覆盖**已存在的目标，而这里的契约是绝不覆盖，
/// 所以只能先检查再重命名。这中间存在 TOCTOU 窗口 —— 没有 `RENAME_NOREPLACE`
/// / `RENAME_EXCL` 的平台上无法消除，只能尽力。
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.symlink_metadata().is_ok() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在"));
    }

    let a = cpath(src)?;
    let b = cpath(dst)?;
    if unsafe { libc::rename(a.as_ptr(), b.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 大缓冲读写复制。
pub fn copy_file(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    common::copy_file_fallback(src, dst, size)
}
