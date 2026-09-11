//! 并行文件复制。
//!
//! 分两步，顺序不能颠倒：
//!
//! 1. **建目录骨架** —— 按深度顺序依次 `mkdir`。这步是串行的，但只涉及
//!    目录（数量远少于文件）且是纯元数据操作，很快。
//! 2. **并行复制文件** —— 骨架已经在了，文件与文件之间再无任何依赖，
//!    可以完全并行地派发。
//!
//! 先建骨架这一步是关键：它把「复制」从一个有依赖的树形问题变成了
//! 彻底无依赖的平坦问题。（对比删除阶段——那边的依赖方向是反的，
//! 必须叶子优先，所以用的是另一套调度，见 [`crate::remove`]。）

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::FailedItem;
use crate::plan::Plan;
use crate::platform;

/// 复制过程中的进度与失败收集。
pub struct Progress {
    pub bytes_done: AtomicU64,
    pub files_done: AtomicUsize,
    failures: Mutex<Vec<FailedItem>>,
    /// 出现第一个失败后置位，让所有线程尽快收工 ——
    /// 复制阶段任何一项失败都会导致整次迁移中止，继续干活是浪费。
    aborted: std::sync::atomic::AtomicBool,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            bytes_done: AtomicU64::new(0),
            files_done: AtomicUsize::new(0),
            failures: Mutex::new(Vec::new()),
            aborted: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn record_failure(&self, item: FailedItem) {
        self.aborted.store(true, Ordering::SeqCst);
        self.failures.lock().unwrap().push(item);
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }

    pub fn take_failures(&self) -> Vec<FailedItem> {
        std::mem::take(&mut *self.failures.lock().unwrap())
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

/// 把 `plan` 描述的整棵树复制到 `dst_root`。
///
/// `dst_root` 必须已经存在。返回的 `Progress` 里带着失败清单 ——
/// 调用方要检查 `is_aborted()`，非空就意味着整次迁移必须回滚。
pub fn copy_tree(
    src_root: &Path,
    dst_root: &Path,
    plan: &Plan,
    threads: usize,
    progress: Arc<Progress>,
) -> Arc<Progress> {
    // ---- 第 1 步：建目录骨架 ----
    // plan.dirs 已按深度排序，顺着建就能保证父目录先于子目录存在。
    for rel in &plan.dirs {
        let dst = dst_root.join(rel);
        if let Err(e) = platform::create_dir(&dst) {
            // 已存在不算错（比如 dst_root 本来就有部分结构）
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                progress.record_failure(FailedItem {
                    path: dst,
                    error: e.to_string(),
                    is_dir: true,
                });
                return progress;
            }
        }
    }

    // ---- 第 2 步：重建符号链接 ----
    // 放在文件之前：链接是纯元数据，很快，而且先建好不影响后面。
    // 注意目标原样搬过去，不做任何解析 —— 指向源树内部的相对链接
    // 搬完之后依然正确，指向外部的绝对链接也保持原样。
    for item in &plan.symlinks {
        let dst = dst_root.join(&item.rel);
        if let Err(e) = platform::create_dir_symlink(&item.target, &dst) {
            progress.record_failure(FailedItem {
                path: dst,
                error: format!("重建符号链接失败: {e}"),
                is_dir: false,
            });
            return progress;
        }
    }

    // ---- 第 3 步：并行复制文件 ----
    if plan.files.is_empty() {
        return progress;
    }

    // 硬链接去重：一组里第一个正常复制，其余的建硬链接指过去。
    // 这一步必须在派发之前算好，因为组内的复制有先后依赖。
    let groups = crate::plan::hardlink_groups(&plan.files);
    let mut is_group_leader: HashMap<usize, ()> = HashMap::new();
    let mut follower_of: HashMap<usize, usize> = HashMap::new();
    for indices in groups.values() {
        let leader = indices[0];
        is_group_leader.insert(leader, ());
        for &follower in &indices[1..] {
            follower_of.insert(follower, leader);
        }
    }

    let threads = threads.max(1);
    let (tx, rx) = crossbeam_channel::unbounded::<usize>();

    // 先派发所有需要真正复制的文件（含各组的 leader）。
    // plan.files 已按大小降序，所以最大的文件最先被领走 —— LPT 调度，
    // 避免一个 8 GB 的文件在所有小文件干完之后独自拖尾。
    for (i, _) in plan.files.iter().enumerate() {
        if !follower_of.contains_key(&i) {
            tx.send(i).ok();
        }
    }
    drop(tx);

    let src_root = src_root.to_path_buf();
    let dst_root = dst_root.to_path_buf();
    let files = Arc::new(plan.files.clone());

    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let rx = rx.clone();
            let progress = Arc::clone(&progress);
            let files = Arc::clone(&files);
            let src_root = src_root.clone();
            let dst_root = dst_root.clone();
            std::thread::Builder::new()
                .name(format!("cshl-copy-{i}"))
                .spawn(move || {
                    while let Ok(idx) = rx.recv() {
                        if progress.is_aborted() {
                            break;
                        }
                        let f = &files[idx];
                        let src = src_root.join(&f.rel);
                        let dst = dst_root.join(&f.rel);

                        match platform::copy_file(&src, &dst, f.size) {
                            Ok(()) => {
                                progress.bytes_done.fetch_add(f.size, Ordering::Relaxed);
                                progress.files_done.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => {
                                progress.record_failure(FailedItem {
                                    path: src,
                                    error: e.to_string(),
                                    is_dir: false,
                                });
                                break;
                            }
                        }
                    }
                })
                .expect("无法创建复制线程")
        })
        .collect();

    drop(rx);
    for h in handles {
        h.join().expect("复制线程 panic");
    }

    if progress.is_aborted() {
        return progress;
    }

    // ---- 第 4 步：还原硬链接 ----
    // 所有 leader 都复制完了，现在把组内其余文件建成硬链接。
    for (&follower, &leader) in &follower_of {
        let leader_dst = dst_root.join(&files[leader].rel);
        let follower_dst = dst_root.join(&files[follower].rel);

        match platform::create_hard_link(&leader_dst, &follower_dst) {
            Ok(()) => {
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                // 目标文件系统可能不支持硬链接（如 FAT32）。
                // 这不该让整次迁移失败 —— 退化成复制一份，
                // 结果是正确的，只是多占一份空间。
                let src = src_root.join(&files[follower].rel);
                if let Err(e2) = platform::copy_file(&src, &follower_dst, files[follower].size) {
                    progress.record_failure(FailedItem {
                        path: src,
                        error: format!("建硬链接失败({e})，退化复制也失败({e2})"),
                        is_dir: false,
                    });
                    return progress;
                }
                progress
                    .bytes_done
                    .fetch_add(files[follower].size, Ordering::Relaxed);
                progress.files_done.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    progress
}

/// 复制失败后清理掉半成品。
///
/// 因为复制始终写在一个临时目录里（`<target>.cshl-partial-<pid>`），
/// 清理就是把那个临时目录整个删掉 —— 源目录自始至终没被碰过。
pub fn cleanup_partial(partial: &Path) {
    if !partial.exists() {
        return;
    }
    // 这里用标准库的递归删除即可：是在清理我们自己刚建的东西，
    // 不值得为它启动并行删除引擎。
    let _ = std::fs::remove_dir_all(partial);
}

/// 为一次迁移生成临时目录名。
///
/// 必须和最终 target 在同一个父目录下 —— 这样最后那步
/// `rename(partial → target)` 才是同卷的原子操作。
pub fn partial_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "target".to_string());
    parent.join(format!(".{}.cshl-partial-{}", name, std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_copy_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一棵有代表性的测试树，返回 (根目录, 预期文件总字节)
    fn build_tree(root: &Path) -> u64 {
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("top.txt"), b"top").unwrap(); // 3
        std::fs::write(root.join("a/mid.txt"), b"middle").unwrap(); // 6
        std::fs::write(root.join("a/b/deep.bin"), vec![7u8; 5000]).unwrap(); // 5000
        3 + 6 + 5000
    }

    #[test]
    fn copy_tree_reproduces_structure_and_content() {
        let d = tmpdir("structure");
        let src = d.join("src");
        let dst = d.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        let expected_bytes = build_tree(&src);

        let plan = crate::plan::scan(&src, 4).unwrap();
        let progress = copy_tree(&src, &dst, &plan, 4, Arc::new(Progress::new()));

        assert!(!progress.is_aborted(), "复制不该失败");
        assert_eq!(progress.bytes_done.load(Ordering::Relaxed), expected_bytes);

        assert_eq!(std::fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(dst.join("a/mid.txt")).unwrap(), b"middle");
        assert_eq!(
            std::fs::read(dst.join("a/b/deep.bin")).unwrap(),
            vec![7u8; 5000]
        );
        assert!(dst.join("empty").is_dir(), "空目录也要建出来");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_preserves_hardlink_groups() {
        let d = tmpdir("hardlink");
        let src = d.join("src");
        let dst = d.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();

        std::fs::write(src.join("a.bin"), b"shared").unwrap();
        platform::create_hard_link(&src.join("a.bin"), &src.join("b.bin")).unwrap();
        platform::create_hard_link(&src.join("a.bin"), &src.join("c.bin")).unwrap();

        let plan = crate::plan::scan(&src, 2).unwrap();
        let progress = copy_tree(&src, &dst, &plan, 2, Arc::new(Progress::new()));
        assert!(!progress.is_aborted());

        // 三个文件内容一致
        for n in ["a.bin", "b.bin", "c.bin"] {
            assert_eq!(std::fs::read(dst.join(n)).unwrap(), b"shared");
        }

        // 关键：目标端它们应当仍是同一个 inode，而不是三份独立拷贝
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ino_a = std::fs::metadata(dst.join("a.bin")).unwrap().ino();
            let ino_b = std::fs::metadata(dst.join("b.bin")).unwrap().ino();
            let ino_c = std::fs::metadata(dst.join("c.bin")).unwrap().ino();
            assert_eq!(ino_a, ino_b, "硬链接组应还原成同一 inode");
            assert_eq!(ino_a, ino_c);
            assert_eq!(
                std::fs::metadata(dst.join("a.bin")).unwrap().nlink(),
                3,
                "链接数应为 3"
            );
        }

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_recreates_symlinks_without_following() {
        let d = tmpdir("symlink");
        let src = d.join("src");
        let dst = d.join("dst");
        let outside = d.join("outside");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("big.txt"), vec![0u8; 99999]).unwrap();

        platform::create_dir_symlink(&outside, &src.join("link")).unwrap();
        std::fs::write(src.join("real.txt"), b"real").unwrap();

        let plan = crate::plan::scan(&src, 2).unwrap();
        let progress = copy_tree(&src, &dst, &plan, 2, Arc::new(Progress::new()));
        assert!(!progress.is_aborted());

        // 链接被重建成链接，而不是把 99999 字节的内容拷过来
        let md = std::fs::symlink_metadata(dst.join("link")).unwrap();
        assert!(md.file_type().is_symlink(), "应重建为链接本身");
        assert_eq!(
            progress.bytes_done.load(Ordering::Relaxed),
            4,
            "只该复制 real.txt 的 4 字节，不该跟随链接"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn partial_path_is_sibling_of_target() {
        let target = Path::new("/data/disk2/moved");
        let p = partial_path(target);
        assert_eq!(
            p.parent(),
            target.parent(),
            "临时目录必须和 target 同级，最后那步 rename 才是同卷原子操作"
        );
        assert_ne!(p, target.to_path_buf());
    }

    #[test]
    fn cleanup_partial_removes_everything() {
        let d = tmpdir("cleanup");
        let partial = d.join("partial");
        std::fs::create_dir_all(partial.join("a/b")).unwrap();
        std::fs::write(partial.join("a/b/f.txt"), b"x").unwrap();

        cleanup_partial(&partial);
        assert!(!partial.exists());

        // 对不存在的路径调用也不该 panic
        cleanup_partial(&d.join("never-existed"));

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn copy_tree_handles_empty_plan() {
        let d = tmpdir("emptyplan");
        let src = d.join("src");
        let dst = d.join("dst");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&dst).unwrap();

        let plan = crate::plan::scan(&src, 2).unwrap();
        let progress = copy_tree(&src, &dst, &plan, 2, Arc::new(Progress::new()));

        assert!(!progress.is_aborted());
        assert_eq!(progress.files_done.load(Ordering::Relaxed), 0);

        std::fs::remove_dir_all(&d).ok();
    }
}
