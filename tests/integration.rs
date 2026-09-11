//! 端到端集成测试：用真实的文件系统跑完整的迁移流程。
//!
//! 这里测的是**跨模块的组合行为**与**失败后的状态**，单元测试覆盖不到：
//! 比如「复制失败后源目录是否原封不动」，只有把整条链路跑一遍才算数。
//!
//! 跨卷路径需要第二个卷，本机没有就跳过（见 [`cross_volume_target`]）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 被测的 `cshl` 二进制。
fn cshl() -> Command {
    let mut exe = std::env::current_exe().expect("拿不到测试二进制路径");
    exe.pop(); // deps/
    exe.pop(); // debug/
    exe.push(format!("cshl{}", std::env::consts::EXE_SUFFIX)); // Windows 上是 cshl.exe
    assert!(
        exe.exists(),
        "找不到 cshl 二进制（{}）。先跑 cargo build。",
        exe.display()
    );

    let mut cmd = Command::new(exe);
    // 台账隔离到临时目录，绝不碰用户真实的 ~/.cshell
    cmd.env("CSHELL_HOME", ledger_home());
    cmd
}

fn ledger_home() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cshell_it_home_{}", std::process::id()));
    std::fs::create_dir_all(&d).ok();
    d
}

fn workdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cshell_it_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 造一棵有代表性的树：嵌套、空目录、大小文件、硬链接组、内外符号链接。
fn build_tree(root: &Path) {
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("top.txt"), b"top").unwrap();
    std::fs::write(root.join("a/mid.txt"), b"middle").unwrap();
    std::fs::write(root.join("a/b/c/deep.bin"), vec![9u8; 100_000]).unwrap();

    // 硬链接组
    std::fs::write(root.join("hl_a.bin"), b"shared payload").unwrap();
    std::fs::hard_link(root.join("hl_a.bin"), root.join("hl_b.bin")).unwrap();
}

/// 目录内容的指纹：相对路径 + 文件内容。用来断言迁移前后完全一致。
fn fingerprint(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    collect(root, root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    return out;

    fn collect(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_path_buf();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                // 记链接目标，不跟随
                let t = std::fs::read_link(&p).unwrap_or_default();
                out.push((rel, format!("symlink:{}", t.display()).into_bytes()));
            } else if ft.is_dir() {
                out.push((rel, b"dir".to_vec()));
                collect(root, &p, out);
            } else {
                out.push((rel, std::fs::read(&p).unwrap_or_default()));
            }
        }
    }
}

/// 找一个跨卷的目标位置。找不到就返回 `None`，调用方跳过该测试。
///
/// macOS 上可以用 `hdiutil` 造一个卷；这里只检测已挂载的，不主动创建 ——
/// 集成测试不该擅自改动机器上的磁盘配置。
fn cross_volume_target() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(target_os = "macos") {
        std::fs::read_dir("/Volumes")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .collect()
    } else {
        vec![PathBuf::from("/dev/shm"), PathBuf::from("/run/shm")]
    };

    let here = std::env::temp_dir();
    let here_vol = volume_of(&here)?;

    for c in candidates {
        if !c.is_dir() {
            continue;
        }
        // 卷不同，且可写
        if volume_of(&c) != Some(here_vol) {
            let probe = c.join(format!(".cshell_probe_{}", std::process::id()));
            if std::fs::create_dir(&probe).is_ok() {
                std::fs::remove_dir(&probe).ok();
                return Some(c);
            }
        }
    }
    None
}

fn volume_of(p: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(p).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

// ---------------------------------------------------------------------------
// 同卷路径
// ---------------------------------------------------------------------------

#[test]
fn same_volume_move_leaves_working_link() {
    let d = workdir("samevol");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);
    let before = fingerprint(&src);

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();
    assert!(
        out.status.success(),
        "迁移应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 源位置现在是链接
    let md = std::fs::symlink_metadata(&src).unwrap();
    assert!(md.file_type().is_symlink(), "源位置应变成链接");

    // 数据完整落在目标
    assert_eq!(fingerprint(&dst), before, "目标内容应与迁移前完全一致");

    // 透过链接依然能读到
    assert_eq!(std::fs::read(src.join("top.txt")).unwrap(), b"top");
    assert_eq!(
        std::fs::read(src.join("a/b/c/deep.bin")).unwrap().len(),
        100_000
    );

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn same_volume_move_preserves_hardlinks() {
    let d = workdir("samevol_hl");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();
    assert!(out.status.success());

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(dst.join("hl_a.bin")).unwrap();
        let b = std::fs::metadata(dst.join("hl_b.bin")).unwrap();
        assert_eq!(a.ino(), b.ino(), "同卷 rename 天然保留硬链接");
        assert_eq!(a.nlink(), 2);
    }

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn restore_roundtrip_is_lossless() {
    let d = workdir("roundtrip");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);
    let before = fingerprint(&src);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&dst)
        .output()
        .unwrap()
        .status
        .success());

    let out = cshl().arg("restore").arg(&src).output().unwrap();
    assert!(
        out.status.success(),
        "还原应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 源位置变回真实目录
    let md = std::fs::symlink_metadata(&src).unwrap();
    assert!(md.file_type().is_dir(), "还原后应是真实目录");
    assert!(!md.file_type().is_symlink());
    assert!(!dst.exists(), "目标位置应已清空");

    assert_eq!(fingerprint(&src), before, "往返后内容应与最初完全一致");

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn follow_link_redirect_repoints_original_link() {
    // 二次迁移：对一个已经是链接的 source 再迁移一次，
    // 原链接应自动跟进到新位置，且不形成 a→b→c 的链条。
    let d = workdir("redirect");
    let src = d.join("tree");
    let first = d.join("first");
    let second = d.join("second");
    build_tree(&src);
    let before = fingerprint(&src);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&first)
        .output()
        .unwrap()
        .status
        .success());

    // 此时 src 是链接，再迁一次
    let out = cshl().arg("-q").arg(&src).arg(&second).output().unwrap();
    assert!(
        out.status.success(),
        "二次迁移应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 原链接指向最新位置，而不是指向 first
    let target = std::fs::read_link(&src).unwrap();
    assert_eq!(
        std::fs::canonicalize(&target).unwrap(),
        std::fs::canonicalize(&second).unwrap(),
        "原链接应直接指向新目标"
    );
    assert!(!first.exists(), "中间位置应已清空，不该留下链条");
    assert_eq!(fingerprint(&second), before);

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn no_follow_link_refuses_instead_of_redirecting() {
    let d = workdir("nofollow");
    let src = d.join("tree");
    let first = d.join("first");
    let second = d.join("second");
    build_tree(&src);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&first)
        .output()
        .unwrap()
        .status
        .success());

    let out = cshl()
        .arg("-q")
        .arg("--no-follow-link")
        .arg(&src)
        .arg(&second)
        .output()
        .unwrap();
    assert!(!out.status.success(), "带 --no-follow-link 时应拒绝");
    assert!(!second.exists());
    assert!(first.exists(), "拒绝后不该动到已有的数据");

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn no_link_moves_without_leaving_a_link() {
    let d = workdir("nolink");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);

    let out = cshl()
        .arg("-q")
        .arg("--no-link")
        .arg(&src)
        .arg(&dst)
        .output()
        .unwrap();
    assert!(out.status.success());

    assert!(!src.exists(), "--no-link 时源位置应彻底消失");
    assert!(dst.join("top.txt").exists());

    std::fs::remove_dir_all(&d).ok();
}

// ---------------------------------------------------------------------------
// 失败后的状态 —— 最重要的一组
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn copy_failure_leaves_source_completely_untouched() {
    // 复制阶段任何一项失败，都必须回到「什么都没发生」的状态：
    // 源目录仍是真实目录（不是链接）、内容一字不差、目标端无半成品。
    let Some(other_vol) = cross_volume_target() else {
        eprintln!("跳过：本机没有可写的第二个卷");
        return;
    };

    use std::os::unix::fs::PermissionsExt;

    let d = workdir("copyfail");
    let src = d.join("tree");
    build_tree(&src);

    // 弄一个不可读的文件，保证复制必然失败
    let locked = src.join("a/locked.bin");
    std::fs::write(&locked, b"secret").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let before = fingerprint(&src);
    let dst = other_vol.join(format!("cshell_it_dst_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();

    // root 用户能读任何文件，那样这个测试没意义
    if out.status.success() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&dst).ok();
        std::fs::remove_dir_all(&d).ok();
        eprintln!("跳过：当前用户能读取 0o000 的文件（root？）");
        return;
    }

    // 源仍是真实目录，不是链接
    let md = std::fs::symlink_metadata(&src).unwrap();
    assert!(
        md.file_type().is_dir() && !md.file_type().is_symlink(),
        "复制失败后源必须仍是真实目录"
    );
    assert_eq!(fingerprint(&src), before, "源内容必须一字不差");

    // 目标端不该留下任何东西
    assert!(!dst.exists(), "失败后不该留下目标目录");
    let leftovers: Vec<_> = std::fs::read_dir(&other_vol)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("cshl-partial"))
        .collect();
    assert!(leftovers.is_empty(), "不该留下 partial 临时目录");

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).ok();
    std::fs::remove_dir_all(&d).ok();
}

// ---------------------------------------------------------------------------
// 校验拒绝
// ---------------------------------------------------------------------------

#[test]
fn rejects_target_inside_source() {
    let d = workdir("selfswallow");
    let src = d.join("tree");
    build_tree(&src);

    let out = cshl()
        .arg("-q")
        .arg(&src)
        .arg(src.join("a/inside"))
        .output()
        .unwrap();

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("自吞"), "应识别出自吞: {stderr}");
    // 源必须没被动过
    assert!(std::fs::symlink_metadata(&src)
        .unwrap()
        .file_type()
        .is_dir());

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn rejects_nonempty_target() {
    let d = workdir("nonempty");
    let src = d.join("tree");
    let dst = d.join("occupied");
    build_tree(&src);
    std::fs::create_dir(&dst).unwrap();
    std::fs::write(dst.join("existing.txt"), b"do not clobber").unwrap();

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(dst.join("existing.txt")).unwrap(),
        b"do not clobber",
        "目标原有内容绝不能被覆盖"
    );

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn accepts_empty_target_dir() {
    // 用户先 mkdir 了目标是很自然的事，空目录应当被接受
    let d = workdir("emptytarget");
    let src = d.join("tree");
    let dst = d.join("prepared");
    build_tree(&src);
    std::fs::create_dir(&dst).unwrap();

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();
    assert!(
        out.status.success(),
        "空目录目标应被接受: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dst.join("top.txt").exists());

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn rejects_missing_source() {
    let d = workdir("missing");
    let out = cshl()
        .arg("-q")
        .arg(d.join("ghost"))
        .arg(d.join("dst"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn rejects_file_as_source() {
    let d = workdir("fileasrc");
    let f = d.join("a.txt");
    std::fs::write(&f, b"x").unwrap();

    let out = cshl()
        .arg("-q")
        .arg(&f)
        .arg(d.join("dst"))
        .output()
        .unwrap();
    assert!(!out.status.success(), "源必须是目录");
    assert!(f.exists(), "被拒绝后源文件应完好");

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn dry_run_changes_nothing() {
    let d = workdir("dryrun");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);
    let before = fingerprint(&src);

    let out = cshl().arg("-n").arg(&src).arg(&dst).output().unwrap();
    assert!(out.status.success());

    assert!(
        std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_dir(),
        "预演不该动源目录"
    );
    assert!(!dst.exists(), "预演不该创建目标");
    assert_eq!(fingerprint(&src), before);

    std::fs::remove_dir_all(&d).ok();
}

// ---------------------------------------------------------------------------
// 跨卷路径
// ---------------------------------------------------------------------------

#[test]
fn cross_volume_move_is_lossless() {
    let Some(other_vol) = cross_volume_target() else {
        eprintln!("跳过：本机没有可写的第二个卷");
        return;
    };

    let d = workdir("crossvol");
    let src = d.join("tree");
    build_tree(&src);

    // 加一个指向源树外部的符号链接：绝不能被跟随展开
    let outside = d.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("precious.txt"), b"must survive").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, src.join("escape")).unwrap();

    let before = fingerprint(&src);
    let dst = other_vol.join(format!("cshell_it_cv_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);

    let out = cshl().arg("-q").arg(&src).arg(&dst).output().unwrap();
    assert!(
        out.status.success(),
        "跨卷迁移应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(fingerprint(&dst), before, "跨卷复制后内容应完全一致");
    assert!(
        std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_symlink(),
        "源位置应变成链接"
    );

    // 硬链接组必须还原成同一 inode，而不是两份独立拷贝
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(dst.join("hl_a.bin")).unwrap();
        let b = std::fs::metadata(dst.join("hl_b.bin")).unwrap();
        assert_eq!(a.ino(), b.ino(), "硬链接组应被还原，不能变成两份拷贝");
    }

    // 外部链接原样重建，其目标内容未被拷入也未被破坏
    #[cfg(unix)]
    {
        let md = std::fs::symlink_metadata(dst.join("escape")).unwrap();
        assert!(md.file_type().is_symlink(), "外部链接应原样重建");
    }
    assert_eq!(
        std::fs::read(outside.join("precious.txt")).unwrap(),
        b"must survive",
        "链接目标的内容绝不能被动到"
    );

    std::fs::remove_dir_all(&dst).ok();
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn cross_volume_restore_roundtrip() {
    let Some(other_vol) = cross_volume_target() else {
        eprintln!("跳过：本机没有可写的第二个卷");
        return;
    };

    let d = workdir("crossvol_restore");
    let src = d.join("tree");
    build_tree(&src);
    let before = fingerprint(&src);

    let dst = other_vol.join(format!("cshell_it_cvr_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&dst)
        .output()
        .unwrap()
        .status
        .success());

    let out = cshl().arg("restore").arg(&src).output().unwrap();
    assert!(
        out.status.success(),
        "跨卷还原应成功: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_dir(),
        "还原后应是真实目录"
    );
    assert_eq!(fingerprint(&src), before, "跨卷往返后内容应完全一致");
    assert!(!dst.exists());

    std::fs::remove_dir_all(&d).ok();
}

// ---------------------------------------------------------------------------
// 台账
// ---------------------------------------------------------------------------

#[test]
fn ledger_records_and_clears() {
    let d = workdir("ledger");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&dst)
        .output()
        .unwrap()
        .status
        .success());

    let out = cshl().arg("list").arg("--json").output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("moved"), "台账应记下这次迁移: {json}");

    assert!(cshl()
        .arg("restore")
        .arg(&src)
        .output()
        .unwrap()
        .status
        .success());

    let out = cshl().arg("list").arg("--json").output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(
        !json.contains(&dst.display().to_string()),
        "还原后台账里不该再有这条记录: {json}"
    );

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn concurrent_migrations_do_not_lose_ledger_entries() {
    // 同时迁移多个目录是很自然的用法。没有文件锁的话，几个进程各自
    // 「读旧值 → 改 → 写回」，后写的会把先写的记录整个覆盖掉。
    let d = workdir("concurrent");

    let n = 6;
    let mut children = Vec::new();
    for i in 0..n {
        let src = d.join(format!("tree{i}"));
        let dst = d.join(format!("moved{i}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.txt"), format!("payload {i}")).unwrap();

        children.push(
            cshl()
                .arg("-q")
                .arg(&src)
                .arg(&dst)
                .spawn()
                .expect("无法启动 cshl"),
        );
    }

    for (i, mut c) in children.into_iter().enumerate() {
        let status = c.wait().unwrap();
        assert!(status.success(), "第 {i} 个迁移应成功");
    }

    // 每一条记录都必须还在
    let out = cshl().arg("list").arg("--json").output().unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    for i in 0..n {
        let marker = format!("moved{i}");
        assert!(
            json.contains(&marker),
            "并发迁移后台账丢了 {marker} 的记录。完整台账:\n{json}"
        );
    }

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn ledger_key_is_stable_across_equivalent_paths() {
    // macOS 上 /tmp 是指向 /private/tmp 的符号链接。同一个目录的两种写法
    // 必须归一成同一个台账键 —— 否则会出现两条记录，restore 也会按用户的
    // 写法找不到。
    let d = workdir("ledgerkey");
    let src = d.join("tree");
    let dst = d.join("moved");
    build_tree(&src);

    assert!(cshl()
        .arg("-q")
        .arg(&src)
        .arg(&dst)
        .output()
        .unwrap()
        .status
        .success());

    // 构造一条经过符号链接的等价路径：<d>/alias → <d>，
    // 于是 <d>/alias/tree 和 <d>/tree 是同一个位置的两种写法。
    // 注意不能对 src 本身 canonicalize —— 它现在是链接，会被解析成 dst。
    let alias = d.join("alias");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&d, &alias).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(&d, &alias).is_err() {
        // Windows 上建目录符号链接可能因权限失败，跳过
        std::fs::remove_dir_all(&d).ok();
        return;
    }

    let via_alias = alias.join("tree");
    let out = cshl().arg("restore").arg(&via_alias).output().unwrap();
    assert!(
        out.status.success(),
        "经由等价路径 {} 也应能 restore: {}",
        via_alias.display(),
        String::from_utf8_lossy(&out.stderr)
    );

    // 确实还原了，而不是只是没报错
    assert!(
        std::fs::symlink_metadata(&src)
            .unwrap()
            .file_type()
            .is_dir(),
        "还原后应是真实目录"
    );
    assert!(!dst.exists());

    std::fs::remove_dir_all(&d).ok();
}
