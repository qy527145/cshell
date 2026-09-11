//! Linux 原生实现。
//!
//! 文件复制走三级回退，每一级都比下一级快一个数量级：
//! 1. **`FICLONE` ioctl** —— btrfs / XFS 的 reflink，写时复制，瞬时完成、不占额外空间
//! 2. **`copy_file_range(2)`** —— 内核内复制，数据不经过用户态；5.3+ 支持跨文件系统，
//!    并天然保留稀疏文件的空洞
//! 3. **大缓冲读写** —— 兜底，配 `SEEK_HOLE`/`SEEK_DATA` 跳过空洞以保持稀疏性
//!
//! 重命名走 `renameat2(RENAME_NOREPLACE)`，由内核保证不覆盖目标。

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

#[path = "unix_common.rs"]
mod common;

pub use common::{
    available_space, create_dir, create_dir_symlink, create_hard_link, is_cross_device, link_kind,
    read_dir, read_link_target, remove_dir, remove_file, volume_id,
};

use common::cpath;

/// `renameat2` 的标志，见 <linux/fs.h>
const RENAME_NOREPLACE: libc::c_uint = 1 << 0;

/// btrfs/XFS reflink 的 ioctl 号：`_IOW(0x94, 9, int)`
const FICLONE: libc::c_ulong = 0x4004_9409;

/// 原子重命名，目标已存在时失败。
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    let a = cpath(src)?;
    let b = cpath(dst)?;

    // renameat2 在部分 libc 版本里没有包装函数，直接走 syscall 最稳
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    // 内核 < 3.15 或文件系统不支持 RENAME_NOREPLACE：退回普通 rename，
    // 自己先确认目标不存在（此时无法消除 TOCTOU，只能尽力）
    match err.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::ENOTSUP) => {
            if dst.symlink_metadata().is_ok() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在"));
            }
            if unsafe { libc::rename(a.as_ptr(), b.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        _ => Err(err),
    }
}

/// 复制单个文件，三级回退。
pub fn copy_file(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    let fin = std::fs::File::open(src)?;
    let md = fin.metadata()?;

    // create_new：目标已存在就失败，绝不覆盖
    let fout = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;

    // 第 1 级：reflink。成功就是瞬时的，且不占额外空间。
    if unsafe { libc::ioctl(fout.as_raw_fd(), FICLONE, fin.as_raw_fd()) } == 0 {
        common::copy_metadata(&md, &fout, dst)?;
        return Ok(());
    }

    // 第 2 级：copy_file_range，内核内复制，数据不进用户态。
    if size > 0 && copy_file_range_all(&fin, &fout, size).is_ok() {
        common::copy_metadata(&md, &fout, dst)?;
        return Ok(());
    }

    // 第 3 级：大缓冲读写兜底。
    // 前两级可能已经写进去了一部分，必须先截断归零再重来。
    fout.set_len(0)?;
    drop(fout);
    std::fs::remove_file(dst).ok();
    common::copy_file_fallback(src, dst, size)
}

/// 用 `copy_file_range` 搬完整个文件。
///
/// 该系统调用每次只保证搬「一部分」，必须循环到搬完为止。
fn copy_file_range_all(fin: &std::fs::File, fout: &std::fs::File, size: u64) -> io::Result<()> {
    let mut remaining = size;
    while remaining > 0 {
        // 单次请求量封顶，避免在超大文件上一次调用阻塞过久
        let chunk = remaining.min(1024 * 1024 * 1024) as usize;
        let copied = unsafe {
            libc::copy_file_range(
                fin.as_raw_fd(),
                std::ptr::null_mut(), // off_in = NULL：用并推进文件自身的偏移
                fout.as_raw_fd(),
                std::ptr::null_mut(),
                chunk,
                0,
            )
        };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        if copied == 0 {
            // 提前到达文件尾：源文件在扫描后被截短了，按实际长度收尾
            break;
        }
        remaining -= copied as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_linux_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn copy_file_preserves_content_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        let d = tmpdir("copymode");
        let src = d.join("a");
        let dst = d.join("b");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();

        copy_file(&src, &dst, payload.len() as u64).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), payload);
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_file_refuses_to_clobber() {
        let d = tmpdir("noclobber");
        let src = d.join("a");
        let dst = d.join("b");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"existing").unwrap();

        assert!(copy_file(&src, &dst, 3).is_err());
        assert_eq!(std::fs::read(&dst).unwrap(), b"existing");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rename_no_replace_is_exclusive() {
        let d = tmpdir("rename");
        let a = d.join("a");
        let b = d.join("b");
        std::fs::write(&a, b"x").unwrap();

        rename_no_replace(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());

        std::fs::write(&a, b"y").unwrap();
        assert!(rename_no_replace(&a, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"x");

        std::fs::remove_dir_all(&d).ok();
    }
}
