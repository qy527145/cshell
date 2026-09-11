//! 扫描源目录，产出一份完整的迁移计划。
//!
//! 计划里有三样东西，分别服务于后面三个阶段：
//! - **目录列表**（按深度分层）→ 复制阶段先建骨架用
//! - **文件清单**（含大小）→ 复制阶段的并行任务，按大小排序做 LPT 调度
//! - **硬链接组** → 把源树内部的硬链接在目标树里还原出来
//!
//! 扫描本身是并行的：多个线程同时展开不同的子树。目录树的形状事先不知道，
//! 所以用一个「待扫描队列 + 活跃计数」的模型 —— 队列空且没有线程在干活时
//! 才算扫完。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::error::{Error, Result};
use crate::platform::{self, EntryKind};

/// 一个待复制的文件。
#[derive(Debug, Clone)]
pub struct FileItem {
    /// 相对于源根目录的路径
    pub rel: PathBuf,
    pub size: u64,
    /// 有多个硬链接时的标识，用于去重
    pub hardlink_id: Option<(u64, u64)>,
}

/// 一个待重建的符号链接。
#[derive(Debug, Clone)]
pub struct SymlinkItem {
    pub rel: PathBuf,
    /// 链接指向的原始目标，原样搬过去（不解析）
    pub target: PathBuf,
}

/// 完整的迁移计划。
#[derive(Debug, Default)]
pub struct Plan {
    /// 所有子目录的相对路径，**按深度升序**排列。
    /// 按这个顺序建目录就能保证父目录总是先于子目录存在。
    pub dirs: Vec<PathBuf>,
    /// 所有普通文件
    pub files: Vec<FileItem>,
    /// 所有符号链接（含 Windows 的 junction）
    pub symlinks: Vec<SymlinkItem>,
    /// 文件总字节数，用于空间预检与进度显示
    pub total_bytes: u64,
    /// 扫描期间无法读取的目录（权限不足等），不致命但要报告
    pub unreadable: Vec<(PathBuf, String)>,
}

impl Plan {
    pub fn total_items(&self) -> usize {
        self.dirs.len() + self.files.len() + self.symlinks.len()
    }
}

/// 并行扫描 `root`，产出迁移计划。
///
/// 绝不跟随符号链接 —— 遇到链接就记下它本身，不递归进去。跟过去会把
/// 链接目标（可能在源树之外、可能构成环）整个拷进来。
pub fn scan(root: &Path, threads: usize) -> Result<Plan> {
    let root = root.to_path_buf();

    // 待扫描的目录队列。元素是相对路径，空路径代表根目录本身。
    let (tx, rx): (Sender<PathBuf>, Receiver<PathBuf>) = unbounded();
    tx.send(PathBuf::new()).ok();

    let shared = Arc::new(SharedState {
        root: root.clone(),
        // 唯一的 Sender 归 SharedState 所有。扫完时把它置 None 丢掉，
        // 通道随之断开，所有工作线程的 recv() 一起返回 Err 退出。
        // 若让工作线程各持一个 Sender 副本，通道永远不会断 —— 死锁。
        tx: Mutex::new(Some(tx)),
        // 队列里已有一项待处理，活跃计数从 1 起
        pending: AtomicUsize::new(1),
        result: Mutex::new(Plan::default()),
        failed: Mutex::new(None),
    });

    let threads = threads.max(1);
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let rx = rx.clone();
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name(format!("cshl-scan-{i}"))
                .spawn(move || scan_worker(rx, shared))
                .expect("无法创建扫描线程")
        })
        .collect();

    drop(rx);

    for h in handles {
        h.join().expect("扫描线程 panic");
    }

    let shared = Arc::try_unwrap(shared)
        .map_err(|_| Error::Ledger("扫描结束时仍有线程持有状态（内部错误）".to_string()))?;

    if let Some(e) = shared.failed.into_inner().unwrap() {
        return Err(e);
    }

    let mut plan = shared.result.into_inner().unwrap();

    // 按深度排序：建目录骨架时父目录必须先于子目录存在。
    // 用 components().count() 而不是字符串长度 —— 后者在含多字节字符的
    // 路径上会给出错误的深度顺序。
    plan.dirs.sort_by_key(|p| p.components().count());

    // 按大小降序：LPT 调度，最大的文件最先派发，避免它在最后独自拖尾。
    // 取负号实现降序：sort_unstable_by_key 只能升序，而我们要最大的先派发
    plan.files
        .sort_unstable_by_key(|f| std::cmp::Reverse(f.size));

    Ok(plan)
}

struct SharedState {
    root: PathBuf,
    /// 派发通道的唯一 Sender。扫完时置 None 关闭通道。
    tx: Mutex<Option<Sender<PathBuf>>>,
    /// 还没处理完的目录数（队列中 + 正在处理中）
    pending: AtomicUsize,
    result: Mutex<Plan>,
    /// 第一个致命错误。记下来让所有线程尽快收工。
    failed: Mutex<Option<Error>>,
}

fn scan_worker(rx: Receiver<PathBuf>, shared: Arc<SharedState>) {
    while let Ok(rel) = rx.recv() {
        let abs = shared.root.join(&rel);

        match platform::read_dir(&abs) {
            Ok(entries) => {
                let mut dirs = Vec::new();
                let mut files = Vec::new();
                let mut symlinks = Vec::new();
                let mut bytes = 0u64;

                for e in entries {
                    let child_rel = rel.join(&e.name);
                    match e.kind {
                        EntryKind::Dir => dirs.push(child_rel),
                        EntryKind::File => {
                            bytes += e.size;
                            files.push(FileItem {
                                rel: child_rel,
                                size: e.size,
                                hardlink_id: e.hardlink_id,
                            });
                        }
                        EntryKind::Symlink => {
                            // 读出链接目标原样保留；读不出来就跳过这一项，
                            // 不让一个坏链接毁掉整次迁移
                            match platform::read_link_target(&abs.join(&e.name)) {
                                Ok(target) => symlinks.push(SymlinkItem {
                                    rel: child_rel,
                                    target,
                                }),
                                Err(err) => {
                                    let mut plan = shared.result.lock().unwrap();
                                    plan.unreadable.push((child_rel, err.to_string()));
                                }
                            }
                        }
                    }
                }

                // 先把新发现的子目录计入活跃数，再入队。
                // 顺序反了会出现「队列已空、计数归零」的假完成。
                if !dirs.is_empty() {
                    shared.pending.fetch_add(dirs.len(), Ordering::SeqCst);
                    let guard = shared.tx.lock().unwrap();
                    match guard.as_ref() {
                        Some(tx) => {
                            for d in &dirs {
                                tx.send(d.clone()).ok();
                            }
                        }
                        None => {
                            // 通道已关（正常流程下不会到这里，因为 pending
                            // 还没归零）。把刚加的计数退回去。
                            shared.pending.fetch_sub(dirs.len(), Ordering::SeqCst);
                        }
                    }
                }

                {
                    let mut plan = shared.result.lock().unwrap();
                    plan.dirs.extend(dirs);
                    plan.files.extend(files);
                    plan.symlinks.extend(symlinks);
                    plan.total_bytes += bytes;
                }
            }
            Err(e) => {
                // 单个目录读不了不致命（权限不足是常见情况），记下来继续。
                // 但根目录读不了就是致命的 —— 整次迁移没有意义了。
                if rel.as_os_str().is_empty() {
                    let mut failed = shared.failed.lock().unwrap();
                    if failed.is_none() {
                        *failed = Some(Error::io(&abs, e));
                    }
                } else {
                    let mut plan = shared.result.lock().unwrap();
                    plan.unreadable.push((rel.clone(), e.to_string()));
                }
            }
        }

        // 这个目录处理完了。活跃计数归零 = 全部扫完：丢掉 Sender 关闭通道，
        // 所有工作线程（包括自己）的 recv() 会一起返回 Err 从而退出。
        // 这里不能只 break —— 那样只有当前这一个线程退出，其余线程会
        // 永远阻塞在 recv() 上。
        if shared.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            *shared.tx.lock().unwrap() = None;
        }
    }
}

/// 把硬链接组整理成 `标识 -> 该组所有文件` 的映射。
///
/// 复制阶段用它来决定：一组里第一个文件正常复制，其余的直接建硬链接。
/// `.git/objects`、pnpm store 这类目录大量使用硬链接，不去重的话磁盘
/// 占用和耗时都会成倍增长。
pub fn hardlink_groups(files: &[FileItem]) -> HashMap<(u64, u64), Vec<usize>> {
    let mut groups: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if let Some(id) = f.hardlink_id {
            groups.entry(id).or_default().push(i);
        }
    }
    // 只留下真正成组的（同一 inode 在本次迁移范围内出现了多次）。
    // 单独出现的即便 nlink > 1，它的另一个链接也在源树之外，
    // 我们只能当普通文件复制。
    groups.retain(|_, v| v.len() > 1);
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_plan_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_finds_all_dirs_files_and_bytes() {
        let d = tmpdir("basic");
        std::fs::create_dir_all(d.join("a/b/c")).unwrap();
        std::fs::create_dir_all(d.join("x")).unwrap();
        std::fs::write(d.join("root.txt"), b"1234567890").unwrap(); // 10
        std::fs::write(d.join("a/one.txt"), b"abc").unwrap(); // 3
        std::fs::write(d.join("a/b/c/deep.bin"), vec![0u8; 1000]).unwrap(); // 1000

        let plan = scan(&d, 4).unwrap();

        // a, a/b, a/b/c, x
        assert_eq!(plan.dirs.len(), 4, "目录数不对: {:?}", plan.dirs);
        assert_eq!(plan.files.len(), 3);
        assert_eq!(plan.total_bytes, 1013);
        assert!(plan.unreadable.is_empty());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dirs_are_sorted_parents_before_children() {
        let d = tmpdir("order");
        std::fs::create_dir_all(d.join("a/b/c/d/e")).unwrap();

        let plan = scan(&d, 4).unwrap();

        // 逐个检查：每个目录的父目录要么是根（空路径），要么已在前面出现过
        let mut seen: Vec<&PathBuf> = Vec::new();
        for dir in &plan.dirs {
            if let Some(parent) = dir.parent() {
                if !parent.as_os_str().is_empty() {
                    assert!(
                        seen.iter().any(|s| s.as_path() == parent),
                        "{} 的父目录 {} 没有排在它前面",
                        dir.display(),
                        parent.display()
                    );
                }
            }
            seen.push(dir);
        }

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn files_are_sorted_largest_first_for_lpt() {
        let d = tmpdir("lpt");
        std::fs::write(d.join("small"), vec![0u8; 10]).unwrap();
        std::fs::write(d.join("huge"), vec![0u8; 100_000]).unwrap();
        std::fs::write(d.join("medium"), vec![0u8; 5_000]).unwrap();

        let plan = scan(&d, 2).unwrap();

        let sizes: Vec<u64> = plan.files.iter().map(|f| f.size).collect();
        assert_eq!(
            sizes,
            vec![100_000, 5_000, 10],
            "文件应按大小降序，最大的先派发"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn symlinks_are_recorded_not_followed() {
        let d = tmpdir("symlink");
        let outside = tmpdir("symlink_outside");
        std::fs::write(outside.join("secret.txt"), b"should not be copied").unwrap();
        std::fs::create_dir(d.join("inner")).unwrap();

        // 一个指向源树外部的目录链接
        platform::create_dir_symlink(&outside, &d.join("escape")).unwrap();

        let plan = scan(&d, 2).unwrap();

        assert_eq!(plan.symlinks.len(), 1);
        assert_eq!(plan.symlinks[0].rel, PathBuf::from("escape"));
        // 关键：绝不能把链接目标里的文件算进来
        assert_eq!(
            plan.files.len(),
            0,
            "不该跟随链接把外部文件扫进来: {:?}",
            plan.files
        );
        // escape 不该被当成普通目录
        assert!(!plan.dirs.iter().any(|p| p == Path::new("escape")));

        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn hardlinks_are_detected() {
        let d = tmpdir("hardlink");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        std::fs::write(&a, b"shared content").unwrap();
        platform::create_hard_link(&a, &b).unwrap();

        let plan = scan(&d, 2).unwrap();

        assert_eq!(plan.files.len(), 2);
        let ids: Vec<_> = plan.files.iter().map(|f| f.hardlink_id).collect();
        assert!(ids.iter().all(|id| id.is_some()), "两个硬链接都该带上标识");
        assert_eq!(ids[0], ids[1], "同一组硬链接的标识应相同");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn lone_file_has_no_hardlink_id() {
        let d = tmpdir("nolink");
        std::fs::write(d.join("solo.txt"), b"x").unwrap();

        let plan = scan(&d, 2).unwrap();

        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].hardlink_id, None,
            "单链接文件不该进去重表，省开销"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn scan_handles_empty_dir() {
        let d = tmpdir("empty");
        let plan = scan(&d, 4).unwrap();
        assert_eq!(plan.total_items(), 0);
        assert_eq!(plan.total_bytes, 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn scan_fails_loudly_on_missing_root() {
        let d = tmpdir("missing");
        let ghost = d.join("does-not-exist");
        assert!(scan(&ghost, 2).is_err(), "根目录不存在必须报错");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn wide_tree_scans_completely_under_concurrency() {
        // 宽而浅的树最容易暴露「活跃计数」模型的竞争问题
        let d = tmpdir("wide");
        for i in 0..60 {
            let sub = d.join(format!("d{i}"));
            std::fs::create_dir(&sub).unwrap();
            for j in 0..5 {
                std::fs::write(sub.join(format!("f{j}")), vec![0u8; 100]).unwrap();
            }
        }

        let plan = scan(&d, 8).unwrap();
        assert_eq!(plan.dirs.len(), 60);
        assert_eq!(plan.files.len(), 300);
        assert_eq!(plan.total_bytes, 30_000);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn deep_tree_scans_completely() {
        // 深而窄的树：并行度用不上，考验的是不会提前收工
        let d = tmpdir("deep");
        let mut p = d.clone();
        for i in 0..40 {
            p = p.join(format!("l{i}"));
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("bottom.txt"), b"end").unwrap();

        let plan = scan(&d, 8).unwrap();
        assert_eq!(plan.dirs.len(), 40);
        assert_eq!(plan.files.len(), 1);

        std::fs::remove_dir_all(&d).ok();
    }
}
