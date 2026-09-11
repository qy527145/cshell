//! Windows 原生实现。
//!
//! 手法沿用 rmbrr 已在生产验证过的那一套，并做了几处增强：
//!
//! - **`\\?\` 前缀**：所有路径一律加前缀绕过 MAX_PATH（260 字符）限制。不加会在
//!   深层 node_modules 里静默失败，导致父目录删除时报 `ERROR_DIR_NOT_EMPTY`。
//! - **POSIX 语义删除**：`SetFileInformationByHandle(FileDispositionInfoEx)` 带
//!   `FILE_DISPOSITION_POSIX_SEMANTICS`，从命名空间立即摘除，不必等最后一个句柄关闭。
//! - **`FindFirstFileExW`** 配 `FindExInfoBasic`（跳过 8.3 短名查询）与
//!   `FIND_FIRST_EX_LARGE_FETCH`（减少大目录的内核往返）。
//! - **`CopyFile2`**，大文件加 `COPY_FILE_NO_BUFFERING` 绕过缓存管理器。
//! - **reparse tag 白名单**：`FILE_ATTRIBUTE_REPARSE_POINT` 不等于「这是链接」。
//!   OneDrive 占位文件、AppExecLink、重复数据删除都是 reparse point。只有
//!   `IO_REPARSE_TAG_MOUNT_POINT` 与 `IO_REPARSE_TAG_SYMLINK` 才按链接处理 ——
//!   误判会静默毁掉用户的云盘目录。

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};

use windows::core::PCWSTR;
use windows::Wdk::Storage::FileSystem::{
    FILE_DISPOSITION_DELETE, FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_INFORMATION_EX, FILE_DISPOSITION_INFORMATION_EX_FLAGS,
    FILE_DISPOSITION_POSIX_SEMANTICS,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
use windows::Win32::Storage::FileSystem::{
    CopyFile2, CreateDirectoryW, CreateHardLinkW, FileDispositionInfoEx, FindClose,
    FindFirstFileExW, FindNextFileW, GetDiskFreeSpaceExW, GetFileInformationByHandle,
    GetVolumePathNameW, MoveFileExW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    COPYFILE2_EXTENDED_PARAMETERS, COPY_FILE_FAIL_IF_EXISTS, COPY_FILE_NO_BUFFERING, DELETE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FINDEX_INFO_LEVELS, FINDEX_SEARCH_OPS, FIND_FIRST_EX_LARGE_FETCH,
    OPEN_EXISTING, WIN32_FIND_DATAW,
};

use super::{DirEntryInfo, EntryKind};

/// `FindExInfoBasic` —— 不返回 8.3 短名，省掉内核的一次查表
const FIND_EX_INFO_BASIC: FINDEX_INFO_LEVELS = FINDEX_INFO_LEVELS(1);
/// `FindExSearchNameMatch` —— 不做额外过滤
const FIND_EX_SEARCH_NAME_MATCH: FINDEX_SEARCH_OPS = FINDEX_SEARCH_OPS(0);

/// 只有这两个 tag 才算「链接」。其余 reparse point 一律按普通文件/目录处理。
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// 超过这个大小的文件走无缓冲复制，绕过缓存管理器：既提升大文件吞吐，
/// 又避免把整个文件灌进 standby list 冲垮系统缓存。
const NO_BUFFERING_THRESHOLD: u64 = 16 * 1024 * 1024;

/// `ERROR_NOT_SAME_DEVICE`
const ERROR_NOT_SAME_DEVICE: i32 = 17;

// ---------------------------------------------------------------------------
// 路径转换
// ---------------------------------------------------------------------------

/// 把路径转成带 `\\?\` 前缀的宽字符串。
///
/// 前缀让内核跳过路径解析（包括 MAX_PATH 检查与 `.`/`..` 规范化），是处理
/// 深层目录树的必要条件。注意：加了前缀后路径必须已是绝对且规范的形式，
/// 所以这里对相对路径不加前缀。
///
/// 「跳过路径解析」也意味着 `/` **不再被当成分隔符** —— 它会原样成为文件名的
/// 一部分，于是 `\\?\D:/data` 直接撞上 `ERROR_INVALID_NAME`。而用户写
/// `cshl ./nm D:/data/nm` 是再自然不过的事（Rust 的 `Path` 本身也认 `/`），
/// 所以加前缀时要把分隔符统一成 `\`。
pub fn to_wide(path: &Path) -> Vec<u16> {
    const SLASH: u16 = b'/' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    let s = path.as_os_str();
    let already_prefixed = {
        let bytes: Vec<u16> = s.encode_wide().take(4).collect();
        // \\?\ == [0x5C, 0x5C, 0x3F, 0x5C]
        bytes == [BACKSLASH, BACKSLASH, 0x3F, BACKSLASH]
    };

    if path.is_absolute() && !already_prefixed {
        let mut out: Vec<u16> = r"\\?\".encode_utf16().collect();
        let wide: Vec<u16> = s
            .encode_wide()
            .map(|c| if c == SLASH { BACKSLASH } else { c })
            .collect();
        // UNC 路径 \\server\share 要写成 \\?\UNC\server\share
        if wide.starts_with(&[BACKSLASH, BACKSLASH]) {
            out.extend("UNC".encode_utf16());
            out.extend_from_slice(&wide[1..]); // 保留一个反斜杠
        } else {
            out.extend_from_slice(&wide);
        }
        out.push(0);
        out
    } else {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// 去掉 `\\?\` 前缀，还原成普通的 DOS 路径形式。
///
/// `std::fs::canonicalize` 在 Windows 上返回的一律是 `\\?\C:\...` 这种
/// **verbatim（逐字）** 形式。它适合喂给内核，却不适合出现在任何「会被存下来
/// 或被人看到」的地方：
///
/// - junction 的 SubstituteName 要写成 `\??\C:\path`，直接拼上带前缀的路径
///   会得到 `\??\\\?\C:\path` —— 内核解析不了，链接看着像建好了，进去却是空的
/// - 符号链接的目标同理，`\\?\` 会被当成路径的一部分
/// - 台账以源路径为键，两种写法会被当成两条不同的记录
///
/// 反过来不必担心「丢了前缀就过不了 MAX_PATH」——[`to_wide`] 会在每次调用
/// Win32 API 前重新加上，标准库的 `std::fs` 同样会自己加。
///
/// `\\?\Volume{GUID}\` 这类没有 DOS 等价写法的路径原样返回。
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let mut comps = path.components();

    let Some(Component::Prefix(prefix)) = comps.next() else {
        return path.to_path_buf();
    };

    let mut out = match prefix.kind() {
        // \\?\C:\... → C:\...
        Prefix::VerbatimDisk(letter) => PathBuf::from(format!("{}:\\", letter as char)),
        // \\?\UNC\server\share\... → \\server\share\...
        Prefix::VerbatimUNC(server, share) => {
            let mut s = OsString::from(r"\\");
            s.push(server);
            s.push(r"\");
            s.push(share);
            s.push(r"\");
            PathBuf::from(s)
        }
        // 非 verbatim 前缀（C:、\\server\share）本来就是干净的；
        // \\?\Volume{...} 则没法去掉前缀，两者都原样返回
        _ => return path.to_path_buf(),
    };

    for c in comps {
        // 根已经包含在上面拼好的头部里了
        if !matches!(c, Component::RootDir) {
            out.push(c.as_os_str());
        }
    }
    out
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// `windows` crate 的错误码转成 `io::Error`
fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

// ---------------------------------------------------------------------------
// 重命名 / 移动
// ---------------------------------------------------------------------------

/// 原子重命名，目标已存在时失败。
///
/// 不传 `MOVEFILE_REPLACE_EXISTING`，所以「不覆盖」由内核保证。也不传
/// `MOVEFILE_COPY_ALLOWED` —— 该标志对目录根本不生效（MSDN 明确写了
/// "This value cannot be used with directories"），跨卷时我们要的正是
/// 一个干脆的 `ERROR_NOT_SAME_DEVICE` 好落到复制路径。
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;

    let a = to_wide(src);
    let b = to_wide(dst);
    unsafe {
        MoveFileExW(
            PCWSTR(a.as_ptr()),
            PCWSTR(b.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(win_err)
}

pub fn is_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE)
}

// ---------------------------------------------------------------------------
// 复制
// ---------------------------------------------------------------------------

/// 复制单个文件。大文件走无缓冲路径。
pub fn copy_file(src: &Path, dst: &Path, size: u64) -> io::Result<()> {
    let a = to_wide(src);
    let b = to_wide(dst);

    let mut flags = COPY_FILE_FAIL_IF_EXISTS.0;
    if size >= NO_BUFFERING_THRESHOLD {
        flags |= COPY_FILE_NO_BUFFERING.0;
    }

    let params = COPYFILE2_EXTENDED_PARAMETERS {
        dwSize: std::mem::size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
        dwCopyFlags: windows::Win32::Storage::FileSystem::COPYFILE_FLAGS(flags),
        pfCancel: std::ptr::null_mut(),
        pProgressRoutine: None,
        pvCallbackContext: std::ptr::null_mut(),
    };

    unsafe { CopyFile2(PCWSTR(a.as_ptr()), PCWSTR(b.as_ptr()), Some(&params)) }.map_err(win_err)
}

pub fn create_dir(path: &Path) -> io::Result<()> {
    let w = to_wide(path);
    unsafe { CreateDirectoryW(PCWSTR(w.as_ptr()), None) }.map_err(win_err)
}

pub fn create_hard_link(existing: &Path, link: &Path) -> io::Result<()> {
    let a = to_wide(existing);
    let b = to_wide(link);
    unsafe { CreateHardLinkW(PCWSTR(b.as_ptr()), PCWSTR(a.as_ptr()), None) }.map_err(win_err)
}

// ---------------------------------------------------------------------------
// 遍历
// ---------------------------------------------------------------------------

/// 枚举目录项。
///
/// `WIN32_FIND_DATAW` 一次就带回了属性、大小和 reparse tag，所以整个枚举
/// 不需要对任何一项再开句柄 —— 这是相对 `std::fs::read_dir` 的主要优势。
///
/// 唯一需要额外开句柄的是硬链接检测（要拿 `nNumberOfLinks`），而这只在
/// 调用方明确需要时才做（见 [`hardlink_id`]）。
pub fn read_dir(dir: &Path) -> io::Result<Vec<DirEntryInfo>> {
    let pattern = dir.join("*");
    let w = to_wide(&pattern);

    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(w.as_ptr()),
            FIND_EX_INFO_BASIC,
            &mut data as *mut _ as *mut _,
            FIND_EX_SEARCH_NAME_MATCH,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }
    .map_err(win_err)?;

    // RAII：任何提前返回都要 FindClose
    struct Finder(HANDLE);
    impl Drop for Finder {
        fn drop(&mut self) {
            unsafe { FindClose(self.0) }.ok();
        }
    }
    let _guard = Finder(handle);

    let mut out = Vec::new();
    loop {
        let name = wide_to_os_string(&data.cFileName);
        if name != "." && name != ".." {
            out.push(entry_from_find_data(dir, name, &data)?);
        }

        if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
            // ERROR_NO_MORE_FILES 是正常结束
            break;
        }
    }

    Ok(out)
}

fn entry_from_find_data(
    dir: &Path,
    name: OsString,
    data: &WIN32_FIND_DATAW,
) -> io::Result<DirEntryInfo> {
    let attrs = data.dwFileAttributes;
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;

    // reparse tag 白名单：只有 junction 与 symlink 算链接。
    // dwReserved0 在 FILE_ATTRIBUTE_REPARSE_POINT 置位时存放 tag。
    let is_link = is_reparse
        && matches!(
            data.dwReserved0,
            IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_SYMLINK
        );

    let kind = if is_link {
        EntryKind::Symlink
    } else if is_dir {
        EntryKind::Dir
    } else {
        EntryKind::File
    };

    let size = if kind == EntryKind::File {
        ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64
    } else {
        0
    };

    // 硬链接检测要开句柄，代价不低。只对普通文件做，
    // 且失败时降级为「没有硬链接」而不是让整次扫描失败。
    let hardlink_id = if kind == EntryKind::File {
        hardlink_id(&dir.join(&name)).unwrap_or(None)
    } else {
        None
    };

    Ok(DirEntryInfo {
        name,
        kind,
        size,
        hardlink_id,
    })
}

/// 取文件的硬链接标识 `(VolumeSerialNumber, FileIndex)`，仅当链接数 > 1。
fn hardlink_id(path: &Path) -> io::Result<Option<(u64, u64)>> {
    let w = to_wide(path);
    let handle = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            PCWSTR(w.as_ptr()),
            0, // 不要求任何访问权，只查元数据
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win_err)?;

    struct H(HANDLE);
    impl Drop for H {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }
    let _guard = H(handle);

    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(win_err)?;

    if info.nNumberOfLinks <= 1 {
        return Ok(None);
    }

    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    Ok(Some((info.dwVolumeSerialNumber as u64, index)))
}

fn wide_to_os_string(buf: &[u16]) -> OsString {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..len])
}

// ---------------------------------------------------------------------------
// 删除
// ---------------------------------------------------------------------------

/// 删除文件或链接本身，走 POSIX 语义（立即从命名空间摘除）。
pub fn remove_file(path: &Path) -> io::Result<()> {
    posix_delete(path, FILE_FLAG_OPEN_REPARSE_POINT, true)
}

/// 删除空目录或目录链接本身，走 POSIX 语义。
pub fn remove_dir(path: &Path) -> io::Result<()> {
    posix_delete(
        path,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        false,
    )
}

/// POSIX 语义删除的共同实现。
///
/// `FILE_DISPOSITION_POSIX_SEMANTICS` 让目录项立即消失，不必等最后一个句柄
/// 关闭 —— 这正是并行删除能跑满的原因：父目录不会因为子项句柄还没释放而
/// 卡在 `ERROR_DIR_NOT_EMPTY`。需要 Windows 10 1607+ 与 NTFS。
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` 保证操作的是链接本身而不是它指向的目标。
fn posix_delete(
    path: &Path,
    flags: FILE_FLAGS_AND_ATTRIBUTES,
    ignore_readonly: bool,
) -> io::Result<()> {
    let w = to_wide(path);

    let handle = unsafe {
        windows::Win32::Storage::FileSystem::CreateFileW(
            PCWSTR(w.as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags,
            None,
        )
    }
    .map_err(win_err)?;

    struct H(HANDLE);
    impl Drop for H {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }
    let _guard = H(handle);

    let mut disposition_flags = FILE_DISPOSITION_DELETE.0 | FILE_DISPOSITION_POSIX_SEMANTICS.0;
    if ignore_readonly {
        // 只读文件直接删掉，不必先 SetFileAttributes 清标志再删（省一次往返）
        disposition_flags |= FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE.0;
    }

    let mut info = FILE_DISPOSITION_INFORMATION_EX {
        Flags: FILE_DISPOSITION_INFORMATION_EX_FLAGS(disposition_flags),
    };

    unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_DISPOSITION_INFORMATION_EX>() as u32,
        )
    }
    .map_err(win_err)
}

// ---------------------------------------------------------------------------
// 卷信息
// ---------------------------------------------------------------------------

pub fn available_space(path: &Path) -> io::Result<u64> {
    let w = to_wide(path);
    let mut free_to_caller = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free_to_caller), None, None) }
        .map_err(win_err)?;
    // 用「调用者可用」而非「卷总空闲」——有磁盘配额时后者会高估
    Ok(free_to_caller)
}

/// 卷标识：取路径所属卷的序列号。
pub fn volume_id(path: &Path) -> io::Result<u64> {
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    let w = to_wide(path);
    let mut mount_point = vec![0u16; MAX_PATH as usize + 1];
    unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut mount_point) }.map_err(win_err)?;

    let mut serial = 0u32;
    unsafe {
        GetVolumeInformationW(
            PCWSTR(mount_point.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            None,
        )
    }
    .map_err(win_err)?;

    Ok(serial as u64)
}

#[allow(dead_code)]
fn unused_last_error() -> io::Error {
    last_error()
}

// ---------------------------------------------------------------------------
// 链接：junction 与 symlink
// ---------------------------------------------------------------------------

/// `FSCTL_SET_REPARSE_POINT`
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
/// `FSCTL_GET_REPARSE_POINT`
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
/// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`
const MAX_REPARSE_BUFFER: usize = 16 * 1024;

/// `REPARSE_DATA_BUFFER` 中，从结构体开头到 `PathBuffer` 的字节数
/// （MountPoint 变体）：ReparseTag(4) + ReparseDataLength(2) + Reserved(2)
/// + 四个 USHORT(8) = 16。
const MOUNT_POINT_HEADER: usize = 16;
/// 同上，SymbolicLink 变体多一个 ULONG Flags 字段
const SYMLINK_HEADER: usize = 20;

/// 创建目录联接（junction）。
///
/// Win32 没有现成 API，必须自己建一个空目录、打开它、然后用
/// `DeviceIoControl(FSCTL_SET_REPARSE_POINT)` 写进去一个手工拼的
/// `REPARSE_DATA_BUFFER`。
///
/// 相比目录符号链接，junction 的最大好处是**普通用户就能创建** ——
/// 不需要管理员权限，也不需要开启开发者模式。代价是只支持本地绝对路径
/// （不支持 UNC），所以调用方要准备好回退到 symlink。
pub fn create_junction(target: &Path, link: &Path) -> io::Result<()> {
    // junction 的目标必须是绝对路径，且要写成 NT 命名空间形式
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()?.join(target)
    };

    // 先建一个空目录，reparse point 是附加在目录上的属性
    create_dir(link)?;

    // 建目录成功之后的任何失败都要把这个空目录清掉，不留垃圾
    match set_mount_point(link, &target) {
        Ok(()) => Ok(()),
        Err(e) => {
            remove_dir(link).ok();
            Err(e)
        }
    }
}

/// 把一个已存在的空目录变成指向 `target` 的 junction。
fn set_mount_point(link: &Path, target: &Path) -> io::Result<()> {
    // SubstituteName 用 NT 命名空间形式 \??\C:\path，
    // PrintName 用用户可读的 C:\path。两者都不带 \\?\ 前缀 ——
    // 带了会拼出 \??\\\?\C:\path 这种内核解析不了的东西。
    let target = strip_verbatim(target);
    let target_str = target.as_os_str();
    let print_name: Vec<u16> = target_str.encode_wide().collect();
    let substitute_name: Vec<u16> = r"\??\"
        .encode_utf16()
        .chain(print_name.iter().copied())
        .collect();

    // PathBuffer 布局：SubstituteName\0 PrintName\0
    let mut path_buffer: Vec<u16> = Vec::new();
    let substitute_offset = 0usize;
    path_buffer.extend_from_slice(&substitute_name);
    path_buffer.push(0);
    let print_offset = path_buffer.len() * 2;
    path_buffer.extend_from_slice(&print_name);
    path_buffer.push(0);

    let path_bytes = path_buffer.len() * 2;
    let total = MOUNT_POINT_HEADER + path_bytes;
    if total > MAX_REPARSE_BUFFER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "junction 目标路径过长",
        ));
    }

    // 手工拼 REPARSE_DATA_BUFFER。用字节数组而不是 repr(C) 结构体，
    // 因为末尾的 PathBuffer 是变长的，Rust 没法直接表达。
    let mut buf = vec![0u8; total];
    // ReparseTag
    buf[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    // ReparseDataLength：不含前 8 字节的头部
    buf[4..6].copy_from_slice(&((total - 8) as u16).to_le_bytes());
    // Reserved = 0（buf 已归零）
    // SubstituteNameOffset / Length
    buf[8..10].copy_from_slice(&(substitute_offset as u16).to_le_bytes());
    buf[10..12].copy_from_slice(&((substitute_name.len() * 2) as u16).to_le_bytes());
    // PrintNameOffset / Length
    buf[12..14].copy_from_slice(&(print_offset as u16).to_le_bytes());
    buf[14..16].copy_from_slice(&((print_name.len() * 2) as u16).to_le_bytes());
    // PathBuffer
    for (i, w) in path_buffer.iter().enumerate() {
        let b = w.to_le_bytes();
        buf[MOUNT_POINT_HEADER + i * 2] = b[0];
        buf[MOUNT_POINT_HEADER + i * 2 + 1] = b[1];
    }

    let handle = open_reparse_handle(link, true)?;
    struct H(HANDLE);
    impl Drop for H {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }
    let _guard = H(handle);

    let mut returned = 0u32;
    unsafe {
        windows::Win32::System::IO::DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            Some(buf.as_ptr() as *const _),
            buf.len() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(win_err)
}

/// 打开一个目录的句柄用于读写 reparse point。
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` 保证拿到的是链接本身而非它指向的目标；
/// `FILE_FLAG_BACKUP_SEMANTICS` 是打开目录句柄的必要条件。
fn open_reparse_handle(path: &Path, write: bool) -> io::Result<HANDLE> {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_GENERIC_READ};

    let w = to_wide(path);
    let access = if write {
        GENERIC_WRITE.0
    } else {
        FILE_GENERIC_READ.0
    };

    unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(win_err)
}

/// 创建目录符号链接。
///
/// 需要管理员权限，或系统开启了开发者模式（此时
/// `SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE` 才生效）。
/// 所以这是 junction 的回退方案而非首选。
pub fn create_dir_symlink(target: &Path, link: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        CreateSymbolicLinkW, SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
        SYMBOLIC_LINK_FLAG_DIRECTORY,
    };

    // 符号链接的目标原样写入，不加 \\?\ 前缀 —— 前缀会被当成路径的一部分
    let l = to_wide(link);
    let target = strip_verbatim(target);
    let t: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // 注意：CreateSymbolicLinkW 是少数不返回 Result 的 Win32 包装，
    // 它返回 bool，失败原因要自己去 GetLastError 取。
    let ok = unsafe {
        CreateSymbolicLinkW(
            PCWSTR(l.as_ptr()),
            PCWSTR(t.as_ptr()),
            SYMBOLIC_LINK_FLAG_DIRECTORY | SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE,
        )
    };
    if !ok {
        return Err(last_error());
    }
    Ok(())
}

/// 判断一个路径是不是链接，是的话属于哪一种。
///
/// 关键在于**只认两个 reparse tag**。OneDrive 占位目录、AppExecLink、
/// 重复数据删除的文件都带 `FILE_ATTRIBUTE_REPARSE_POINT`，但它们不是链接，
/// 必须当作普通文件/目录处理。
pub fn link_kind(path: &Path) -> io::Result<Option<super::LinkKind>> {
    let w = to_wide(path);
    let mut data = WIN32_FIND_DATAW::default();

    // 对路径本身（而非 dir\*）调 FindFirstFileEx，一次就拿到属性与 tag
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(w.as_ptr()),
            FIND_EX_INFO_BASIC,
            &mut data as *mut _ as *mut _,
            FIND_EX_SEARCH_NAME_MATCH,
            None,
            windows::Win32::Storage::FileSystem::FIND_FIRST_EX_FLAGS(0),
        )
    }
    .map_err(win_err)?;
    unsafe { FindClose(handle) }.ok();

    if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
        return Ok(None);
    }

    Ok(match data.dwReserved0 {
        IO_REPARSE_TAG_MOUNT_POINT => Some(super::LinkKind::Junction),
        IO_REPARSE_TAG_SYMLINK => Some(super::LinkKind::Symlink),
        // 其他 reparse tag（OneDrive 占位、AppExecLink、重复数据删除……）
        // 一律不算链接
        _ => None,
    })
}

/// 读出链接指向的目标路径。
///
/// `std::fs::read_link` 在 Windows 上也能处理 junction，但返回的路径带
/// `\\?\` 前缀且不做清理。这里直接解析 reparse buffer 并取 PrintName ——
/// 那正是给人看的、干净的形式。
pub fn read_link_target(path: &Path) -> io::Result<std::path::PathBuf> {
    let handle = open_reparse_handle(path, false)?;
    struct H(HANDLE);
    impl Drop for H {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.ok();
        }
    }
    let _guard = H(handle);

    let mut buf = vec![0u8; MAX_REPARSE_BUFFER];
    let mut returned = 0u32;
    unsafe {
        windows::Win32::System::IO::DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(win_err)?;

    if (returned as usize) < MOUNT_POINT_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse 数据过短",
        ));
    }

    let tag = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let header = match tag {
        IO_REPARSE_TAG_MOUNT_POINT => MOUNT_POINT_HEADER,
        IO_REPARSE_TAG_SYMLINK => SYMLINK_HEADER,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "不是 junction 或符号链接",
            ))
        }
    };

    // 优先用 PrintName（干净的 C:\path 形式）；某些链接没有 PrintName，
    // 这时退回 SubstituteName 并剥掉 NT 命名空间前缀。
    let print_offset = u16::from_le_bytes([buf[12], buf[13]]) as usize;
    let print_len = u16::from_le_bytes([buf[14], buf[15]]) as usize;

    let (offset, len, strip_nt_prefix) = if print_len > 0 {
        (print_offset, print_len, false)
    } else {
        let sub_offset = u16::from_le_bytes([buf[8], buf[9]]) as usize;
        let sub_len = u16::from_le_bytes([buf[10], buf[11]]) as usize;
        (sub_offset, sub_len, true)
    };

    let start = header + offset;
    let end = start + len;
    if end > returned as usize || end > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse 数据中的路径越界",
        ));
    }

    let wide: Vec<u16> = buf[start..end]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let mut s = OsString::from_wide(&wide);

    if strip_nt_prefix {
        let text = s.to_string_lossy().to_string();
        // \??\C:\path → C:\path；\??\UNC\server\share → \\server\share
        let cleaned = if let Some(rest) = text.strip_prefix(r"\??\UNC\") {
            format!(r"\\{}", rest)
        } else if let Some(rest) = text.strip_prefix(r"\??\") {
            rest.to_string()
        } else {
            text
        };
        s = OsString::from(cleaned);
    }

    Ok(std::path::PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(w: &[u16]) -> String {
        let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
        String::from_utf16_lossy(&w[..end])
    }

    #[test]
    fn to_wide_prefixes_absolute_paths() {
        assert_eq!(decode(&to_wide(Path::new(r"C:\a\b"))), r"\\?\C:\a\b");
        // UNC
        assert_eq!(
            decode(&to_wide(Path::new(r"\\server\share\d"))),
            r"\\?\UNC\server\share\d"
        );
        // 已经带前缀的不再叠加
        assert_eq!(decode(&to_wide(Path::new(r"\\?\C:\a"))), r"\\?\C:\a");
        // 相对路径不加前缀（加了就成非法路径了）
        assert_eq!(decode(&to_wide(Path::new(r"a\b"))), r"a\b");
    }

    /// verbatim 前缀会关掉内核的路径解析，`/` 不再是分隔符而会变成文件名的
    /// 一部分 —— 用户写 `D:/data` 会撞 ERROR_INVALID_NAME。
    #[test]
    fn to_wide_normalizes_forward_slashes() {
        assert_eq!(decode(&to_wide(Path::new("C:/a/b"))), r"\\?\C:\a\b");
        assert_eq!(decode(&to_wide(Path::new(r"C:\a/b\c"))), r"\\?\C:\a\b\c");
        assert_eq!(
            decode(&to_wide(Path::new("//server/share/d"))),
            r"\\?\UNC\server\share\d"
        );
    }

    #[test]
    fn strip_verbatim_and_to_wide_roundtrip() {
        for p in [r"C:\a\b", r"\\server\share\d"] {
            let stripped = strip_verbatim(Path::new(&decode(&to_wide(Path::new(p)))));
            assert_eq!(stripped, PathBuf::from(p), "{p} 来回转换后应还原");
        }
    }
}
