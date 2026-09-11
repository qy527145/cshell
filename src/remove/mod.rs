//! 并行删除源目录。
//!
//! 删除的依赖方向和复制**正好相反**：目录必须先清空才能删掉，所以要从叶子
//! 往根删。这就没法像复制那样「先建骨架再彻底并行」了，只能维护一张依赖图：
//! 每个目录记着还有多少个子目录没删完，归零时它自己就变成新的叶子，进队列。
//!
//! 这套模型移植自 rmbrr（`/Users/xuqiao/code/dev/rmbrr/src/broker.rs`）。
//!
//! Windows 上还有一个额外的加成：POSIX 语义删除让目录项**立即**从命名空间
//! 消失，不必等最后一个句柄关闭。否则父目录会因为子项句柄还没释放而卡在
//! `ERROR_DIR_NOT_EMPTY`，并行度根本跑不起来。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::error::FailedItem;
use crate::plan::Plan;
use crate::platform;

/// 删除过程中的进度与失败收集。
///
/// 和复制阶段不同，这里**不会**因为单项失败就中止 —— 数据此时已经安全
/// 落在目标位置了，能删多少删多少，剩下的报给用户处理，比留一个删了一半
/// 的目录树要好。
pub struct RemoveProgress {
    pub dirs_done: AtomicUsize,
    pub files_done: AtomicUsize,
    failures: Mutex<Vec<FailedItem>>,
}

impl RemoveProgress {
    pub fn new() -> Self {
        Self {
            dirs_done: AtomicUsize::new(0),
            files_done: AtomicUsize::new(0),
            failures: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, item: FailedItem) {
        self.failures.lock().unwrap().push(item);
    }

    pub fn take_failures(&self) -> Vec<FailedItem> {
        std::mem::take(&mut *self.failures.lock().unwrap())
    }

    pub fn failure_count(&self) -> usize {
        self.failures.lock().unwrap().len()
    }
}

impl Default for RemoveProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// 依赖图：谁还欠着几个子目录，以及谁的父目录是谁。
struct Broker {
    /// 目录 → 还没删完的子目录数
    pending_children: Mutex<HashMap<PathBuf, usize>>,
    /// 目录 → 父目录
    parent: HashMap<PathBuf, PathBuf>,
    /// 派发通道。全部删完时置 None 关闭通道，让工作线程退出。
    tx: Mutex<Option<Sender<PathBuf>>>,
    total: usize,
    completed: AtomicUsize,
}

impl Broker {
    /// 从计划构建依赖图，返回 broker 与工作线程要用的接收端。
    ///
    /// `dirs` 是**相对路径**，空路径代表源根目录本身。
    fn new(plan: &Plan) -> (Arc<Self>, Receiver<PathBuf>) {
        let (tx, rx) = unbounded();

        // 统计每个目录有多少直接子目录
        let mut pending_children: HashMap<PathBuf, usize> = HashMap::new();
        let mut parent: HashMap<PathBuf, PathBuf> = HashMap::new();

        // 根目录（空路径）也要算进去 —— 它是最后一个被删的
        let all: Vec<PathBuf> = std::iter::once(PathBuf::new())
            .chain(plan.dirs.iter().cloned())
            .collect();

        for dir in &plan.dirs {
            let p = dir.parent().map(|p| p.to_path_buf()).unwrap_or_default();
            parent.insert(dir.clone(), p.clone());
            *pending_children.entry(p).or_insert(0) += 1;
        }

        let total = all.len();

        // 叶子目录（没有任何子目录）先入队
        let leaves: Vec<PathBuf> = all
            .iter()
            .filter(|d| !pending_children.contains_key(*d))
            .cloned()
            .collect();

        let broker = Arc::new(Self {
            pending_children: Mutex::new(pending_children),
            parent,
            tx: Mutex::new(Some(tx)),
            total,
            completed: AtomicUsize::new(0),
        });

        for leaf in leaves {
            if let Some(tx) = broker.tx.lock().unwrap().as_ref() {
                tx.send(leaf).ok();
            }
        }

        (broker, rx)
    }

    /// 标记一个目录已删除，把它的父目录的欠账减一；减到零就派发父目录。
    fn mark_done(&self, dir: &Path) {
        let done = self.completed.fetch_add(1, Ordering::SeqCst) + 1;
        if done == self.total {
            // 全部删完：关掉通道，工作线程的 recv() 会返回 Err 从而退出
            *self.tx.lock().unwrap() = None;
            return;
        }

        let Some(parent) = self.parent.get(dir).cloned() else {
            return; // 根目录没有父目录
        };

        let newly_free = {
            let mut counts = self.pending_children.lock().unwrap();
            match counts.get_mut(&parent) {
                Some(n) => {
                    *n -= 1;
                    if *n == 0 {
                        counts.remove(&parent);
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };

        if newly_free {
            if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                tx.send(parent).ok();
            }
        }
    }
}

/// 删除整棵源目录树。
///
/// `plan` 是之前扫描出来的那份 —— 复用它就不必再遍历一次文件系统。
pub fn remove_tree(
    root: &Path,
    plan: &Plan,
    threads: usize,
    progress: Arc<RemoveProgress>,
) -> Arc<RemoveProgress> {
    // 先按目录把文件和符号链接归拢，这样每个工作线程拿到一个目录时
    // 能一次性知道要删哪些项，不必再读一遍目录。
    let mut files_by_dir: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for f in &plan.files {
        let parent = f.rel.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        files_by_dir.entry(parent).or_default().push(f.rel.clone());
    }
    for s in &plan.symlinks {
        let parent = s.rel.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        files_by_dir.entry(parent).or_default().push(s.rel.clone());
    }
    let files_by_dir = Arc::new(files_by_dir);

    let (broker, rx) = Broker::new(plan);
    let threads = threads.max(1);
    let root = root.to_path_buf();

    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let rx = rx.clone();
            let broker = Arc::clone(&broker);
            let progress = Arc::clone(&progress);
            let files_by_dir = Arc::clone(&files_by_dir);
            let root = root.clone();
            std::thread::Builder::new()
                .name(format!("cshl-rm-{i}"))
                .spawn(move || {
                    while let Ok(rel) = rx.recv() {
                        let abs = root.join(&rel);

                        // 先删这个目录里的所有文件与链接
                        if let Some(items) = files_by_dir.get(&rel) {
                            for item_rel in items {
                                let item = root.join(item_rel);
                                match platform::remove_file(&item) {
                                    Ok(()) => {
                                        progress.files_done.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                        // 已经没了，当作成功
                                        progress.files_done.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => progress.record(FailedItem {
                                        path: item,
                                        error: e.to_string(),
                                        is_dir: false,
                                    }),
                                }
                            }
                        }

                        // 再删目录本身
                        match platform::remove_dir(&abs) {
                            Ok(()) => {
                                progress.dirs_done.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                progress.dirs_done.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(e) => progress.record(FailedItem {
                                path: abs,
                                error: e.to_string(),
                                is_dir: true,
                            }),
                        }

                        // 不论成败都要 mark_done —— 否则父目录会永远等下去，
                        // 整个删除阶段卡死。
                        broker.mark_done(&rel);
                    }
                })
                .expect("无法创建删除线程")
        })
        .collect();

    drop(rx);
    for h in handles {
        h.join().expect("删除线程 panic");
    }

    progress
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_rm_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn remove_tree_deletes_everything() {
        let d = tmpdir("full");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("a/b/c")).unwrap();
        std::fs::create_dir_all(src.join("x/y")).unwrap();
        std::fs::write(src.join("top.txt"), b"1").unwrap();
        std::fs::write(src.join("a/mid.txt"), b"2").unwrap();
        std::fs::write(src.join("a/b/c/deep.txt"), b"3").unwrap();
        std::fs::write(src.join("x/y/leaf.txt"), b"4").unwrap();

        let plan = crate::plan::scan(&src, 4).unwrap();
        let progress = remove_tree(&src, &plan, 4, Arc::new(RemoveProgress::new()));

        assert_eq!(progress.failure_count(), 0, "不该有删除失败");
        assert!(!src.exists(), "源目录应被完全删除");
        assert_eq!(progress.files_done.load(Ordering::Relaxed), 4);
        // 5 个目录：根 + a + a/b + a/b/c + x + x/y = 6
        assert_eq!(progress.dirs_done.load(Ordering::Relaxed), 6);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_tree_handles_deep_nesting() {
        // 深树：依赖链最长，最容易暴露 broker 的死锁或提前收工
        let d = tmpdir("deep");
        let src = d.join("src");
        let mut p = src.clone();
        for i in 0..50 {
            p = p.join(format!("l{i}"));
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("bottom.txt"), b"end").unwrap();

        let plan = crate::plan::scan(&src, 8).unwrap();
        let progress = remove_tree(&src, &plan, 8, Arc::new(RemoveProgress::new()));

        assert_eq!(progress.failure_count(), 0);
        assert!(!src.exists());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_tree_handles_wide_tree() {
        let d = tmpdir("wide");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        for i in 0..80 {
            let sub = src.join(format!("d{i}"));
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("f.txt"), b"x").unwrap();
        }

        let plan = crate::plan::scan(&src, 8).unwrap();
        let progress = remove_tree(&src, &plan, 8, Arc::new(RemoveProgress::new()));

        assert_eq!(progress.failure_count(), 0);
        assert!(!src.exists());
        assert_eq!(progress.files_done.load(Ordering::Relaxed), 80);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_tree_deletes_symlink_not_its_target() {
        let d = tmpdir("symlink");
        let src = d.join("src");
        let outside = d.join("outside");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious.txt"), b"do not delete").unwrap();
        platform::create_dir_symlink(&outside, &src.join("link")).unwrap();

        let plan = crate::plan::scan(&src, 2).unwrap();
        let progress = remove_tree(&src, &plan, 2, Arc::new(RemoveProgress::new()));

        assert_eq!(progress.failure_count(), 0);
        assert!(!src.exists());
        assert!(
            outside.join("precious.txt").exists(),
            "删链接绝不能删到它指向的内容"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_tree_handles_empty_dir() {
        let d = tmpdir("empty");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();

        let plan = crate::plan::scan(&src, 2).unwrap();
        let progress = remove_tree(&src, &plan, 2, Arc::new(RemoveProgress::new()));

        assert_eq!(progress.failure_count(), 0);
        assert!(!src.exists());

        std::fs::remove_dir_all(&d).ok();
    }
}
