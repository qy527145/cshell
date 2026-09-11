//! 原地链接的创建、读取与替换。
//!
//! 这是整个工具的「留痕」环节：数据搬走之后，源路径上要留下一个链接，
//! 让所有引用旧路径的程序继续正常工作。
//!
//! 平台差异：
//! - **Unix** 只能用符号链接。POSIX 禁止对目录建硬链接（`link()` 返回
//!   `EPERM`），macOS 的历史特权接口在 APFS 上也已不可用。
//! - **Windows** 首选 junction（普通用户即可创建，无需管理员权限或开发者
//!   模式），失败时回退目录符号链接。

use std::io;
use std::path::{Path, PathBuf};

use crate::cli::LinkType;
use crate::platform::{self, LinkKind};

/// 在 `link` 处创建一个指向 `target` 的目录链接。
///
/// `preferred` 只在 Windows 上有意义。返回实际创建出来的链接种类 ——
/// 可能与请求的不同（junction 失败会回退 symlink），调用方要把这个结果
/// 记进台账。
pub fn create_dir_link(target: &Path, link: &Path, preferred: LinkType) -> io::Result<LinkKind> {
    // 链接目标必须是绝对路径：相对路径的符号链接在别的工作目录下会指错地方，
    // 而 junction 根本不支持相对路径。
    let target = absolutize(target)?;

    #[cfg(windows)]
    {
        match preferred {
            LinkType::Junction => match platform::create_junction(&target, link) {
                Ok(()) => Ok(LinkKind::Junction),
                Err(e) => {
                    // junction 不支持 UNC 目标等情况：回退符号链接。
                    // 符号链接需要管理员权限或开发者模式，可能也失败 ——
                    // 那就把两个错误都告诉用户。
                    match platform::create_dir_symlink(&target, link) {
                        Ok(()) => Ok(LinkKind::Symlink),
                        Err(e2) => Err(io::Error::other(format!(
                            "创建 junction 失败（{e}），回退符号链接也失败（{e2}）。\
                             符号链接需要管理员权限或开启开发者模式。"
                        ))),
                    }
                }
            },
            LinkType::Symlink => {
                platform::create_dir_symlink(&target, link)?;
                Ok(LinkKind::Symlink)
            }
        }
    }

    #[cfg(not(windows))]
    {
        // Unix 上 --link-type 无意义，符号链接是唯一选择
        let _ = preferred;
        platform::create_dir_symlink(&target, link)?;
        Ok(LinkKind::Symlink)
    }
}

/// 判断路径是否为链接。
///
/// 注意 Windows 上「是 reparse point」≠「是链接」，见
/// [`platform::link_kind`]。
pub fn kind_of(path: &Path) -> io::Result<Option<LinkKind>> {
    match platform::link_kind(path) {
        Ok(k) => Ok(k),
        // 路径不存在时不算错误，就是「不是链接」
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// 读出链接直接指向的那一跳目标。
pub fn target_of(link: &Path) -> io::Result<PathBuf> {
    platform::read_link_target(link)
}

/// 删除一个目录链接本身（绝不碰它指向的内容）。
pub fn remove_dir_link(link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // junction 与目录符号链接都是目录，走 remove_dir；
        // 底层带了 FILE_FLAG_OPEN_REPARSE_POINT，删的是链接本身。
        platform::remove_dir(link)
    }
    #[cfg(not(windows))]
    {
        // Unix 上符号链接无论指向什么都是用 unlink 删
        platform::remove_file(link)
    }
}

/// 把一个已存在的目录链接改指到新目标。
///
/// Unix 上做得到原子替换：先在同级建一个临时链接，再 `rename` 覆盖过去 ——
/// POSIX 保证这一步是原子的，任何时刻别的进程看到的要么是旧链接要么是新
/// 链接，不会看到「链接不存在」。
///
/// Windows 上 junction 没有原子替换手段，只能先删后建。窗口极短（纯元数据
/// 操作），但确实存在。
pub fn repoint_dir_link(
    link: &Path,
    new_target: &Path,
    preferred: LinkType,
) -> io::Result<LinkKind> {
    let new_target = absolutize(new_target)?;

    #[cfg(not(windows))]
    {
        let _ = preferred;
        let parent = link
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "链接路径没有父目录"))?;

        // 临时名带上 pid，避免并发的两个 cshl 撞车
        let tmp = parent.join(format!(
            ".cshl-relink-{}-{}",
            std::process::id(),
            link.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        // 清掉上一次可能留下的同名残留
        let _ = platform::remove_file(&tmp);

        platform::create_dir_symlink(&new_target, &tmp)?;

        // rename 覆盖：POSIX 保证原子
        if let Err(e) = std::fs::rename(&tmp, link) {
            let _ = platform::remove_file(&tmp);
            return Err(e);
        }
        Ok(LinkKind::Symlink)
    }

    #[cfg(windows)]
    {
        // 先记下原链接种类，失败时好按原样恢复
        let old_kind = kind_of(link)?;
        let old_target = if old_kind.is_some() {
            target_of(link).ok()
        } else {
            None
        };

        remove_dir_link(link)?;

        match create_dir_link(&new_target, link, preferred) {
            Ok(k) => Ok(k),
            Err(e) => {
                // 建新链接失败：尽力把旧链接恢复回去，别让源路径凭空消失
                if let (Some(kind), Some(old)) = (old_kind, old_target) {
                    let lt = match kind {
                        LinkKind::Junction => LinkType::Junction,
                        LinkKind::Symlink => LinkType::Symlink,
                    };
                    let _ = create_dir_link(&old, link, lt);
                }
                Err(e)
            }
        }
    }
}

/// 把路径变成绝对路径，但**不解析其中的符号链接**。
///
/// 不能用 `canonicalize` —— 它会把中间的链接全部展开，而我们要的恰恰是
/// 用户书写的那个路径形式。同时路径可能还不存在（比如刚要创建的 target），
/// `canonicalize` 会直接失败。
pub fn absolutize(path: &Path) -> io::Result<PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // 用户可能自己就写了 \\?\C:\...，先归一成普通形式
    Ok(normalize_dots(&platform::strip_verbatim(&abs)))
}

/// 完全解析一个路径（链套链也能到底），并去掉 Windows 的 `\\?\` 前缀。
///
/// 凡是结果会被存进台账、打印给用户、或写进链接的地方，都必须走这个包装而
/// 不是直接调 `std::fs::canonicalize` —— 后者在 Windows 上返回 verbatim
/// 形式，会让 junction 的 SubstituteName 变成 `\??\\\?\C:\path`，建出一个
/// 看着成功、进去却是空的链接。详见 [`platform::strip_verbatim`]。
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|p| platform::strip_verbatim(&p))
}

/// 规范化到「唯一形式」：解析父目录里的所有符号链接，但**保留最后一段**。
///
/// 这是台账的键必须用的形式。理由：
/// - 最后一段不能解析 —— 它自己可能就是我们要操作的那个链接
/// - 父目录必须解析 —— 否则 macOS 上 `/tmp/x` 与 `/private/tmp/x`
///   （`/tmp` 是指向 `/private/tmp` 的符号链接）会被当成两个不同的路径，
///   台账里同一个目录会出现两条记录，`restore` 也会按用户的写法找不到。
///
/// 父目录不存在时退回 [`absolutize`] 的结果 —— 尽力而为，不因此失败。
pub fn canonical_key(path: &Path) -> io::Result<PathBuf> {
    let abs = absolutize(path)?;

    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        // 根目录之类没有父目录/文件名的路径，原样返回
        return Ok(abs);
    };

    match std::fs::canonicalize(parent) {
        Ok(real_parent) => Ok(platform::strip_verbatim(&real_parent).join(name)),
        Err(_) => Ok(abs),
    }
}

/// 消掉路径里的 `.` 与 `..`，纯词法处理，不碰文件系统。
fn normalize_dots(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                match out.components().next_back() {
                    // 上一段是普通目录名：弹掉它
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    // 已经在根（或盘符根）上：根的父目录还是根，丢弃这个 `..`
                    Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                    // 相对路径开头的 `..`，或已经累积的 `..`：只能原样保留
                    _ => out.push(comp.as_os_str()),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_link_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn normalize_dots_removes_cur_and_parent() {
        assert_eq!(normalize_dots(Path::new("/a/./b")), PathBuf::from("/a/b"));
        assert_eq!(
            normalize_dots(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            normalize_dots(Path::new("/a/b/../../c")),
            PathBuf::from("/c")
        );
        // 越过根的 `..`：POSIX 规定根的父目录还是根，直接丢弃
        assert_eq!(normalize_dots(Path::new("/../a")), PathBuf::from("/a"));
        assert_eq!(normalize_dots(Path::new("/../../..")), PathBuf::from("/"));
        // 相对路径开头的 `..` 无处可弹，必须原样保留
        assert_eq!(normalize_dots(Path::new("../a")), PathBuf::from("../a"));
    }

    #[test]
    fn canonical_key_resolves_parent_but_keeps_last_segment() {
        let d = tmpdir("key");
        let real = d.join("real");
        let via_link = d.join("via");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("inner")).unwrap();
        // via → real，所以 via/inner 和 real/inner 是同一个目录
        platform::create_dir_symlink(&real, &via_link).unwrap();

        let k1 = canonical_key(&real.join("inner")).unwrap();
        let k2 = canonical_key(&via_link.join("inner")).unwrap();
        assert_eq!(k1, k2, "父目录里的链接必须被解析，两种写法要归一成同一个键");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn canonical_key_does_not_resolve_the_link_itself() {
        let d = tmpdir("keylink");
        let real = d.join("real");
        let link = d.join("link");
        std::fs::create_dir(&real).unwrap();
        platform::create_dir_symlink(&real, &link).unwrap();

        let k = canonical_key(&link).unwrap();
        assert!(
            k.ends_with("link"),
            "最后一段不能解析 —— 它可能正是我们要操作的那个链接，实际 {}",
            k.display()
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn canonical_key_tolerates_missing_path() {
        let d = tmpdir("keymissing");
        // 目标还不存在是正常情况（迁移的 target）
        let k = canonical_key(&d.join("not-created-yet")).unwrap();
        assert!(k.ends_with("not-created-yet"));
        // 父目录也不存在时退回词法结果，不该失败
        let k2 = canonical_key(&d.join("no/such/parent")).unwrap();
        assert!(k2.ends_with("parent"));

        std::fs::remove_dir_all(&d).ok();
    }

    /// verbatim 前缀绝不能流进任何一处会被存下来或写进链接的路径。
    /// 它曾让 junction 的 SubstituteName 变成 `\??\\\?\C:\path`，
    /// 建出一个看着成功、进去却是空的链接。
    #[test]
    fn canonical_key_has_no_verbatim_prefix() {
        let d = tmpdir("verbatim");
        let inner = d.join("inner");
        std::fs::create_dir(&inner).unwrap();

        for p in [&d, &inner] {
            let k = canonical_key(p).unwrap();
            assert!(
                !k.to_string_lossy().starts_with(r"\\?\"),
                "规范化后的路径不能带 \\\\?\\ 前缀，实际 {}",
                k.display()
            );
        }

        // 用户自己写 \\?\ 前缀时也要归一到同一个键
        #[cfg(windows)]
        {
            let verbatim = PathBuf::from(format!(r"\\?\{}", inner.display()));
            assert_eq!(
                canonical_key(&verbatim).unwrap(),
                canonical_key(&inner).unwrap(),
                "带前缀与不带前缀必须归一成同一个键"
            );
        }

        std::fs::remove_dir_all(&d).ok();
    }

    /// 链接必须真的能读穿 —— 只检查「创建成功」是不够的：
    /// 目标路径写坏的 junction 一样会创建成功，只是进去空空如也。
    #[test]
    fn link_to_canonicalized_target_is_traversable() {
        let d = tmpdir("traverse");
        let target = d.join("target");
        let link = d.join("link");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("payload.txt"), b"payload").unwrap();

        // 关键：目标先过一遍完全解析，模拟 migrate 里真实的调用路径
        let resolved = canonicalize(&target).unwrap();
        create_dir_link(&resolved, &link, LinkType::Junction).unwrap();

        assert_eq!(
            std::fs::read(link.join("payload.txt")).unwrap(),
            b"payload",
            "透过链接读不到内容 —— 链接目标写坏了"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn absolutize_does_not_resolve_symlinks() {
        let d = tmpdir("abs");
        let real = d.join("real");
        let link = d.join("link");
        std::fs::create_dir(&real).unwrap();
        platform::create_dir_symlink(&real, &link).unwrap();

        let abs = absolutize(&link).unwrap();
        // 必须保留 link 这一段，而不是展开成 real
        assert!(
            abs.ends_with("link"),
            "absolutize 不应解析符号链接，实际得到 {}",
            abs.display()
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn create_read_and_remove_dir_link() {
        let d = tmpdir("crud");
        let target = d.join("target");
        let link = d.join("link");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("f.txt"), b"hi").unwrap();

        let kind = create_dir_link(&target, &link, LinkType::Junction).unwrap();

        // 链接应被识别出来
        assert_eq!(kind_of(&link).unwrap(), Some(kind));
        // 透过链接能读到内容
        assert_eq!(std::fs::read(link.join("f.txt")).unwrap(), b"hi");
        // 读出的目标应指向 target
        let t = target_of(&link).unwrap();
        assert!(
            t.ends_with("target"),
            "链接目标应是 target，实际 {}",
            t.display()
        );

        // 删链接不该动到目标内容
        remove_dir_link(&link).unwrap();
        assert!(!link.exists());
        assert!(target.join("f.txt").exists(), "删链接绝不能碰目标内容");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn repoint_switches_target_and_keeps_both_intact() {
        let d = tmpdir("repoint");
        let first = d.join("first");
        let second = d.join("second");
        let link = d.join("link");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("a.txt"), b"first").unwrap();
        std::fs::write(second.join("a.txt"), b"second").unwrap();

        create_dir_link(&first, &link, LinkType::Junction).unwrap();
        assert_eq!(std::fs::read(link.join("a.txt")).unwrap(), b"first");

        repoint_dir_link(&link, &second, LinkType::Junction).unwrap();
        assert_eq!(
            std::fs::read(link.join("a.txt")).unwrap(),
            b"second",
            "改指后应读到新目标的内容"
        );
        // 两个目标目录都应完好
        assert!(first.join("a.txt").exists());
        assert!(second.join("a.txt").exists());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn kind_of_returns_none_for_plain_dir_and_missing_path() {
        let d = tmpdir("kind");
        let plain = d.join("plain");
        std::fs::create_dir(&plain).unwrap();

        assert_eq!(kind_of(&plain).unwrap(), None);
        assert_eq!(kind_of(&d.join("nope")).unwrap(), None);

        std::fs::remove_dir_all(&d).ok();
    }
}
