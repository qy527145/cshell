//! macOS 原生实现。
//!
//! 两个关键点：
//! - **`copyfile(3)` + `COPYFILE_CLONE`** —— 内核在可能时自动走 `clonefile(2)`
//!   的 APFS 写时复制（瞬时完成、不占额外空间），不可能时无缝回退为普通复制。
//!   这是一条零成本的快路径。同时 `COPYFILE_ALL` 会一并带走权限、时间戳、
//!   xattr、ACL 与 resource fork，不用我们逐项处理。
//! - **`renamex_np(RENAME_EXCL)`** —— 由内核保证「目标已存在就失败」，
//!   消除先 `exists()` 再 `rename` 的 TOCTOU 竞争。

use std::io;
use std::path::Path;

#[path = "unix_common.rs"]
mod common;

pub use common::{
    available_space, create_dir, create_dir_symlink, create_hard_link, is_cross_device, link_kind,
    read_dir, read_link_target, remove_dir, remove_file, volume_id,
};

use common::cpath;

// libc 尚未导出这两个符号，手动声明。
// 见 macOS 的 <stdlib.h> 与 <copyfile.h>。
const RENAME_EXCL: libc::c_uint = 0x0000_0004;

/// `copyfile(3)` 的 flags，见 <copyfile.h>
const COPYFILE_ACL: u32 = 1 << 0;
const COPYFILE_STAT: u32 = 1 << 1;
const COPYFILE_XATTR: u32 = 1 << 2;
const COPYFILE_DATA: u32 = 1 << 3;
/// = ACL | STAT | XATTR | DATA，一次带走全部元数据
const COPYFILE_ALL: u32 = COPYFILE_ACL | COPYFILE_STAT | COPYFILE_XATTR | COPYFILE_DATA;
/// 尽量用 clonefile 做写时复制；做不到则自动回退普通复制
const COPYFILE_CLONE: u32 = 1 << 24;
/// 不跟随源端的符号链接（我们对链接有专门处理，绝不能在这里跟过去）
const COPYFILE_NOFOLLOW_SRC: u32 = 1 << 18;
/// 目标已存在则失败，而不是覆盖
const COPYFILE_EXCL: u32 = 1 << 17;

extern "C" {
    fn renamex_np(
        from: *const libc::c_char,
        to: *const libc::c_char,
        flags: libc::c_uint,
    ) -> libc::c_int;

    fn copyfile(
        from: *const libc::c_char,
        to: *const libc::c_char,
        state: *mut libc::c_void,
        flags: u32,
    ) -> libc::c_int;
}

/// 原子重命名，目标已存在时失败。
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    let a = cpath(src)?;
    let b = cpath(dst)?;

    let rc = unsafe { renamex_np(a.as_ptr(), b.as_ptr(), RENAME_EXCL) };
    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    // 老系统或某些文件系统不支持 RENAME_EXCL：退回普通 rename，
    // 但要自己先确认目标不存在（此时无法消除 TOCTOU，只能尽力）。
    if err.raw_os_error() == Some(libc::ENOTSUP) || err.raw_os_error() == Some(libc::EINVAL) {
        if dst.symlink_metadata().is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "目标已存在"));
        }
        if unsafe { libc::rename(a.as_ptr(), b.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }

    Err(err)
}

/// 复制单个文件。
///
/// `COPYFILE_CLONE` 让内核优先尝试 APFS 的写时复制克隆 —— 同卷内瞬时完成。
/// 跨卷时它会自动退化为普通复制，所以无条件带上这个标志没有任何损失。
pub fn copy_file(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    let a = cpath(src)?;
    let b = cpath(dst)?;

    let flags = COPYFILE_ALL | COPYFILE_CLONE | COPYFILE_NOFOLLOW_SRC | COPYFILE_EXCL;
    let rc = unsafe { copyfile(a.as_ptr(), b.as_ptr(), std::ptr::null_mut(), flags) };
    if rc == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    // 目标已存在是真错误，不该被兜底路径掩盖
    if err.raw_os_error() == Some(libc::EEXIST) {
        return Err(err);
    }
    // 其余情况（目标文件系统不支持 xattr/ACL 等）退回大缓冲读写
    common::copy_file_fallback(src, dst, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_macos_{}_{}", tag, std::process::id()));
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
        std::fs::write(&src, b"payload").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();

        copy_file(&src, &dst, 7).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "copyfile 的 COPYFILE_STAT 应带走权限位");

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
    fn copy_file_does_not_follow_source_symlink() {
        let d = tmpdir("nofollow");
        let real = d.join("real");
        let link = d.join("link");
        let dst = d.join("copied");
        std::fs::write(&real, b"secret").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        copy_file(&link, &dst, 0).unwrap();

        // 应该复制出一个链接，而不是把目标内容拷过来
        let md = std::fs::symlink_metadata(&dst).unwrap();
        assert!(
            md.file_type().is_symlink(),
            "COPYFILE_NOFOLLOW_SRC 应保持链接本身"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rename_no_replace_is_atomic_and_exclusive() {
        let d = tmpdir("rename");
        let a = d.join("a");
        let b = d.join("b");
        std::fs::write(&a, b"x").unwrap();

        rename_no_replace(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());

        // 再建一个 a，rename 到已存在的 b 必须失败
        std::fs::write(&a, b"y").unwrap();
        assert!(rename_no_replace(&a, &b).is_err());
        assert_eq!(std::fs::read(&b).unwrap(), b"x", "b 不应被覆盖");

        std::fs::remove_dir_all(&d).ok();
    }
}
