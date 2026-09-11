//! 各平台原生 API 的统一封装。
//!
//! 上层（[`crate::plan`] / [`crate::copy`] / [`crate::remove`] / [`crate::link`]）
//! 只看见这里导出的函数签名，具体实现按 `cfg` 分发到 [`windows`] / [`macos`] /
//! [`linux`] / [`fallback`]。
//!
//! 之所以不用 `std::fs` 直接写，是因为标准库为了跨平台一致性牺牲了每个平台上
//! 最快的那条路：Windows 的 POSIX 语义删除与无缓冲复制、Linux 的 `copy_file_range`
//! 与 reflink、macOS 的 `clonefile` —— 都只能直接调原生 API 才拿得到。

use std::io;
use std::path::Path;

#[cfg(windows)]
#[path = "windows.rs"]
mod imp;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
#[path = "fallback.rs"]
mod imp;

/// 目录项的类型。
///
/// `Symlink` 特指**真正的链接**：Unix 的符号链接，Windows 的 junction 与
/// symlink。Windows 上其他 reparse tag（OneDrive 占位、AppExecLink、
/// 重复数据删除等）**不算**链接，会被归为 `File` 或 `Dir` 正常复制 ——
/// 误判会静默毁掉用户的云盘目录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// 一个目录项的元数据，由 [`read_dir`] 在单次系统调用中一并取得。
///
/// 尽量避免对每一项再 `stat` 一次 —— 大目录里这是主要开销。
#[derive(Debug, Clone)]
pub struct DirEntryInfo {
    pub name: std::ffi::OsString,
    pub kind: EntryKind,
    /// 文件大小（目录与链接为 0）
    pub size: u64,
    /// 硬链接标识：仅当该文件确实有多个硬链接时为 `Some`。
    /// Unix 是 `(st_dev, st_ino)`，Windows 是 `(VolumeSerialNumber, FileId)`。
    pub hardlink_id: Option<(u64, u64)>,
}

// ---------------------------------------------------------------------------
// 重命名 / 移动
// ---------------------------------------------------------------------------

/// 原子重命名，且**保证不覆盖**已存在的 `dst`。
///
/// 这是同卷迁移的全部内容：无论目录里有一个还是一千万个文件，都是一次元数据
/// 操作。跨卷时返回的错误可以用 [`is_cross_device`] 判定。
///
/// 「不覆盖」由内核保证（Linux `RENAME_NOREPLACE` / macOS `RENAME_EXCL` /
/// Windows 不传 `MOVEFILE_REPLACE_EXISTING`），而不是先 `exists()` 再 rename ——
/// 后者存在 TOCTOU 竞争。
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    imp::rename_no_replace(src, dst)
}

/// 判断一个错误是否意味着「源和目标不在同一个卷上」。
///
/// 这是同卷/跨卷分流的判据。我们采用「先试后判」：直接发起 rename，失败且
/// 命中此判据时才走复制路径。比预先探测卷号可靠 —— 同卷跨挂载点、APFS
/// firmlink 这类边界情况预测容易错，实测不会。
pub fn is_cross_device(e: &io::Error) -> bool {
    imp::is_cross_device(e)
}

// ---------------------------------------------------------------------------
// 复制
// ---------------------------------------------------------------------------

/// 复制单个普通文件，尽可能走各平台最快的那条路。
///
/// - macOS: `copyfile(3)` + `COPYFILE_CLONE`（APFS 写时复制，可用时瞬时完成）
/// - Linux: `FICLONE` reflink → `copy_file_range` 内核内复制 → 大缓冲读写
/// - Windows: `CopyFile2`，大文件加 `COPY_FILE_NO_BUFFERING`
///
/// `size` 是调用方在扫描阶段已经拿到的文件大小，用于选择复制策略，省一次 `stat`。
pub fn copy_file(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    imp::copy_file(src, dst, size)
}

/// 创建目录（不递归，父目录必须已存在）。
pub fn create_dir(path: &Path) -> io::Result<()> {
    imp::create_dir(path)
}

/// 创建硬链接。用于把源树内部的硬链接组在目标树里还原出来。
pub fn create_hard_link(existing: &Path, link: &Path) -> io::Result<()> {
    imp::create_hard_link(existing, link)
}

// ---------------------------------------------------------------------------
// 遍历
// ---------------------------------------------------------------------------

/// 枚举目录的直接子项，每项带上类型、大小与硬链接标识。
///
/// 不递归 —— 递归策略由 [`crate::plan`] 控制，以便并行化。
pub fn read_dir(dir: &Path) -> io::Result<Vec<DirEntryInfo>> {
    imp::read_dir(dir)
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

/// 删除一个文件或链接本身（绝不跟随链接）。
pub fn remove_file(path: &Path) -> io::Result<()> {
    imp::remove_file(path)
}

/// 删除一个**空**目录，或删除一个目录链接本身。
pub fn remove_dir(path: &Path) -> io::Result<()> {
    imp::remove_dir(path)
}

/// 链接的种类。
///
/// Unix 上只有 [`LinkKind::Symlink`] —— POSIX 禁止对目录建硬链接
/// （`link()` 对目录返回 `EPERM`），所以原地留链接只能用符号链接。
/// Windows 上两种都有，[`LinkKind::Junction`] 是首选（普通用户即可创建）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Symlink,
    /// Windows 目录联接
    Junction,
}

impl std::fmt::Display for LinkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkKind::Symlink => write!(f, "symlink"),
            LinkKind::Junction => write!(f, "junction"),
        }
    }
}

// ---------------------------------------------------------------------------
// 链接
// ---------------------------------------------------------------------------

/// 创建指向 `target` 的目录符号链接。
pub fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    imp::create_dir_symlink(target, link)
}

/// 创建指向 `target` 的目录联接（junction）。仅 Windows 有。
#[cfg(windows)]
pub fn create_junction(target: &Path, link: &Path) -> io::Result<()> {
    imp::create_junction(target, link)
}

/// 判断路径是否为链接，是则返回其种类。
///
/// **Windows 上这不等于「有没有 `FILE_ATTRIBUTE_REPARSE_POINT`」** ——
/// OneDrive 占位目录、AppExecLink、重复数据删除的文件都是 reparse point
/// 但都不是链接。实现里按 reparse tag 做了白名单。
pub fn link_kind(path: &Path) -> io::Result<Option<LinkKind>> {
    imp::link_kind(path)
}

/// 读出链接指向的目标（不做进一步解析，只读这一跳）。
pub fn read_link_target(link: &Path) -> io::Result<std::path::PathBuf> {
    imp::read_link_target(link)
}

// ---------------------------------------------------------------------------
// 路径形式
// ---------------------------------------------------------------------------

/// 把路径还原成「适合存下来、适合给人看、适合写进链接」的形式。
///
/// Windows 上去掉 `\\?\` verbatim 前缀 —— `std::fs::canonicalize` 返回的正是
/// 这种形式，而它一旦流进 junction 的 reparse buffer 就会拼出内核解析不了的
/// `\??\\\?\C:\path`，流进台账则会让同一个目录出现两种键。
/// 其他平台上没有这回事，原样返回。
#[cfg(windows)]
pub fn strip_verbatim(path: &Path) -> std::path::PathBuf {
    imp::strip_verbatim(path)
}

/// 见 windows 版本的文档。非 Windows 平台上是恒等变换。
#[cfg(not(windows))]
pub fn strip_verbatim(path: &Path) -> std::path::PathBuf {
    path.to_path_buf()
}

// ---------------------------------------------------------------------------
// 卷信息
// ---------------------------------------------------------------------------

/// 目标路径所在卷的可用字节数，用于跨卷复制前的空间预检。
pub fn available_space(path: &Path) -> io::Result<u64> {
    imp::available_space(path)
}

/// 路径所在卷的标识。**仅用于 dry-run 的预检报告**，不作为分流判据
/// （分流见 [`rename_no_replace`] + [`is_cross_device`]）。
pub fn volume_id(path: &Path) -> io::Result<u64> {
    imp::volume_id(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_handles_disk_unc_and_unprefixed() {
        use std::path::PathBuf;

        // 最常见的一种：canonicalize 的返回值
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\C:\Users\a\b")),
            PathBuf::from(r"C:\Users\a\b")
        );
        // 盘根
        assert_eq!(strip_verbatim(Path::new(r"\\?\C:\")), PathBuf::from(r"C:\"));
        // UNC
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\dir")),
            PathBuf::from(r"\\server\share\dir")
        );
        // 本来就干净的路径原样返回
        assert_eq!(
            strip_verbatim(Path::new(r"C:\Users\a")),
            PathBuf::from(r"C:\Users\a")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\server\share\d")),
            PathBuf::from(r"\\server\share\d")
        );
        // 相对路径不动
        assert_eq!(strip_verbatim(Path::new(r"a\b")), PathBuf::from(r"a\b"));
        // 卷 GUID 没有 DOS 等价形式，必须原样保留
        let vol = r"\\?\Volume{11111111-2222-3333-4444-555555555555}\x";
        assert_eq!(strip_verbatim(Path::new(vol)), PathBuf::from(vol));
    }

    #[test]
    fn cross_device_detects_exdev() {
        #[cfg(unix)]
        {
            let e = io::Error::from_raw_os_error(libc::EXDEV);
            assert!(is_cross_device(&e));
            let e = io::Error::from_raw_os_error(libc::ENOENT);
            assert!(!is_cross_device(&e));
        }
        #[cfg(windows)]
        {
            // ERROR_NOT_SAME_DEVICE = 17
            assert!(is_cross_device(&io::Error::from_raw_os_error(17)));
            assert!(!is_cross_device(&io::Error::from_raw_os_error(2)));
        }
    }

    #[test]
    fn read_dir_reports_kinds_and_sizes() {
        let tmp = std::env::temp_dir().join(format!("cshell_pf_readdir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("f.txt"), b"hello world").unwrap();

        let mut entries = read_dir(&tmp).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].name, std::ffi::OsString::from("f.txt"));
        assert_eq!(entries[0].kind, EntryKind::File);
        assert_eq!(entries[0].size, 11);

        assert_eq!(entries[1].name, std::ffi::OsString::from("sub"));
        assert_eq!(entries[1].kind, EntryKind::Dir);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn read_dir_reports_symlinks_without_following() {
        let tmp = std::env::temp_dir().join(format!("cshell_pf_symlink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("real.txt"), b"data").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.join("real.txt"), tmp.join("link.txt")).unwrap();
        #[cfg(windows)]
        {
            // Windows 上建符号链接可能因权限失败，失败就跳过这个断言
            if std::os::windows::fs::symlink_file(tmp.join("real.txt"), tmp.join("link.txt"))
                .is_err()
            {
                std::fs::remove_dir_all(&tmp).ok();
                return;
            }
        }

        let entries = read_dir(&tmp).unwrap();
        let link = entries
            .iter()
            .find(|e| e.name == "link.txt")
            .expect("应枚举到链接本身");
        assert_eq!(link.kind, EntryKind::Symlink);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn copy_file_roundtrips_content() {
        let tmp = std::env::temp_dir().join(format!("cshell_pf_copy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let src = tmp.join("src.bin");
        let dst = tmp.join("dst.bin");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &payload).unwrap();

        copy_file(&src, &dst, payload.len() as u64).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), payload);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn copy_file_handles_empty_file() {
        let tmp = std::env::temp_dir().join(format!("cshell_pf_copy0_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let src = tmp.join("empty");
        let dst = tmp.join("empty2");
        std::fs::write(&src, b"").unwrap();

        copy_file(&src, &dst, 0).unwrap();
        assert_eq!(std::fs::metadata(&dst).unwrap().len(), 0);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn rename_no_replace_refuses_existing_target() {
        let tmp = std::env::temp_dir().join(format!("cshell_pf_rename_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let a = tmp.join("a");
        let b = tmp.join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        // b 已存在 → 必须失败，且 a 仍在原处
        assert!(rename_no_replace(&a, &b).is_err());
        assert!(a.exists());

        // 换个不存在的目标 → 成功
        let c = tmp.join("c");
        rename_no_replace(&a, &c).unwrap();
        assert!(!a.exists());
        assert!(c.is_dir());

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn available_space_is_positive_for_temp_dir() {
        let space = available_space(&std::env::temp_dir()).unwrap();
        assert!(space > 0, "临时目录所在卷应有可用空间");
    }

    #[test]
    fn volume_id_is_stable_and_matches_for_same_volume() {
        let tmp = std::env::temp_dir();
        let a = volume_id(&tmp).unwrap();
        let b = volume_id(&tmp).unwrap();
        assert_eq!(a, b, "同一路径的卷标识应稳定");
    }
}
