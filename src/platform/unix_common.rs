//! Unix 各平台共用的实现（macOS 与 Linux 都 `mod` 进来）。
//!
//! 这里放的是两边系统调用完全一致的部分：目录枚举、删除、建目录、硬链接、
//! 卷信息。差异部分（`copy_file` 的零拷贝路径、`rename` 的原子不覆盖标志）
//! 由各自的 `macos.rs` / `linux.rs` 覆盖。

use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::platform::{DirEntryInfo, EntryKind};

/// 把路径转成可以直接喂给系统调用的 C 字符串。
pub fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径中含有 NUL 字节"))
}

pub fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

/// 枚举目录项。
///
/// 走 `std::fs::read_dir`：它的 `DirEntry::file_type()` 在 Unix 上直接用
/// `readdir` 返回的 `d_type`，只有文件系统返回 `DT_UNKNOWN` 时才回退
/// `lstat` —— 正是我们想要的优化，而且省去了手写 `readdir` 时
/// errno 检查在 macOS/Linux 上函数名不同的可移植性麻烦。
///
/// 只有普通文件才继续 `lstat`（需要大小与硬链接数），目录和符号链接
/// 靠 `d_type` 一次定夺。
pub fn read_dir(dir: &Path) -> io::Result<Vec<DirEntryInfo>> {
    let mut out = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();

        let ft = match entry.file_type() {
            Ok(ft) => ft,
            // 枚举过程中被删：跳过，不让整次扫描失败
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };

        let info = if ft.is_symlink() {
            DirEntryInfo {
                name,
                kind: EntryKind::Symlink,
                size: 0,
                hardlink_id: None,
            }
        } else if ft.is_dir() {
            DirEntryInfo {
                name,
                kind: EntryKind::Dir,
                size: 0,
                hardlink_id: None,
            }
        } else {
            // 普通文件（以及 FIFO/socket/设备）：需要大小与硬链接数
            stat_entry(dir, name)?
        };

        out.push(info);
    }

    Ok(out)
}

/// 对单项 `lstat`，填出完整的 [`DirEntryInfo`]。
fn stat_entry(dir: &Path, name: OsString) -> io::Result<DirEntryInfo> {
    let full = dir.join(&name);
    let md = match std::fs::symlink_metadata(&full) {
        Ok(md) => md,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // 扫描期间被别的进程删掉了：当作大小为 0 的普通文件，
            // 后续复制阶段自会再报一次错。
            return Ok(DirEntryInfo {
                name,
                kind: EntryKind::File,
                size: 0,
                hardlink_id: None,
            });
        }
        Err(e) => return Err(e),
    };

    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    };

    // 只有确实存在多个硬链接时才记录，省掉绝大多数文件的去重表开销
    let hardlink_id = if kind == EntryKind::File && md.nlink() > 1 {
        Some((md.dev() as u64, md.ino()))
    } else {
        None
    };

    Ok(DirEntryInfo {
        name,
        kind,
        size: if kind == EntryKind::File { md.len() } else { 0 },
        hardlink_id,
    })
}

pub fn create_dir(path: &Path) -> io::Result<()> {
    let c = cpath(path)?;
    // 0o777 会被 umask 裁剪，与 mkdir(1) 行为一致
    if unsafe { libc::mkdir(c.as_ptr(), 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn create_hard_link(existing: &Path, link: &Path) -> io::Result<()> {
    let a = cpath(existing)?;
    let b = cpath(link)?;
    if unsafe { libc::link(a.as_ptr(), b.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 删除文件或链接本身。`unlink` 天然不跟随符号链接。
pub fn remove_file(path: &Path) -> io::Result<()> {
    let c = cpath(path)?;
    if unsafe { libc::unlink(c.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn remove_dir(path: &Path) -> io::Result<()> {
    let c = cpath(path)?;
    if unsafe { libc::rmdir(c.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 可用空间：用 `f_bavail`（非特权用户可用）而非 `f_bfree`（含保留块），
/// 否则会高估，导致复制到一半才发现写不下。
pub fn available_space(path: &Path) -> io::Result<u64> {
    let c = cpath(path)?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

pub fn volume_id(path: &Path) -> io::Result<u64> {
    let md = std::fs::metadata(path)?;
    Ok(md.dev() as u64)
}

/// 大缓冲读写兜底复制。所有零拷贝路径都失败时用它。
///
/// 会保留权限位与修改时间；稀疏空洞的保留由各平台的快路径负责（这里的
/// 兜底不做空洞探测，因为能走到这条路的通常是不支持这些特性的文件系统）。
pub fn copy_file_fallback(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    use std::io::{Read, Write};

    let mut fin = std::fs::File::open(src)?;
    let md = fin.metadata()?;

    let mut fout = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;

    // 预分配：一次性把大小定下来，避免边写边扩展造成的碎片
    if size > 0 {
        fout.set_len(size).ok();
    }

    // 缓冲区随文件大小伸缩：小文件不必浪费 1 MiB，大文件要够大才能跑满带宽
    let buf_size = size.clamp(64 * 1024, 4 * 1024 * 1024) as usize;
    let mut buf = vec![0u8; buf_size];
    loop {
        let n = fin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        fout.write_all(&buf[..n])?;
    }
    fout.flush()?;

    copy_metadata(&md, &fout, dst)?;
    Ok(())
}

/// 复制权限位与时间戳。
pub fn copy_metadata(
    src_md: &std::fs::Metadata,
    dst_file: &std::fs::File,
    dst: &Path,
) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // 权限
    let mode = src_md.mode() & 0o7777;
    if unsafe { libc::fchmod(dst_file.as_raw_fd(), mode as libc::mode_t) } != 0 {
        // 权限设置失败不致命（如目标文件系统不支持），继续设时间戳
        let _ = dst;
    }

    // 时间戳：保留 atime 与 mtime 的纳秒精度
    let times = [
        libc::timespec {
            tv_sec: src_md.atime() as libc::time_t,
            tv_nsec: src_md.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: src_md.mtime() as libc::time_t,
            tv_nsec: src_md.mtime_nsec() as _,
        },
    ];
    unsafe { libc::futimens(dst_file.as_raw_fd(), times.as_ptr()) };

    Ok(())
}

// ---------------------------------------------------------------------------
// 链接
// ---------------------------------------------------------------------------

/// 创建符号链接。
///
/// Unix 上目录链接只能是符号链接 —— POSIX 禁止对目录建硬链接
/// （`link()` 对目录返回 `EPERM`）。macOS 虽有历史遗留的特权接口，
/// 但在 APFS 上已不可用。
pub fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    let t = cpath(target)?;
    let l = cpath(link)?;
    if unsafe { libc::symlink(t.as_ptr(), l.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn link_kind(path: &Path) -> io::Result<Option<crate::platform::LinkKind>> {
    let md = std::fs::symlink_metadata(path)?;
    Ok(if md.file_type().is_symlink() {
        Some(crate::platform::LinkKind::Symlink)
    } else {
        None
    })
}

pub fn read_link_target(link: &Path) -> io::Result<std::path::PathBuf> {
    std::fs::read_link(link)
}
