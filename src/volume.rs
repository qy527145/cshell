//! 卷标识探测。
//!
//! **只用于 dry-run 的预检报告**，不作为同卷/跨卷的分流判据。
//!
//! 分流走的是「先试后判」：直接发起 `rename`，失败且命中
//! [`crate::platform::is_cross_device`] 时才落到复制路径。一次 rename
//! 尝试是微秒级的，比任何预测都便宜，而且更可靠 —— 同卷内跨挂载点、
//! APFS firmlink 这类边界情况，预测容易错，实测不会。

use std::path::Path;

use crate::platform;

/// 两个路径看起来是否在同一个卷上。
///
/// `target` 通常还不存在，所以实际检查的是它**最近的已存在祖先**
/// （数据最终会落在那个卷上）。
///
/// 返回 `None` 表示探测失败（路径不存在、权限不足等），此时调用方
/// 不该做任何假设 —— 反正真正的判定是靠 rename 的返回值。
pub fn same_volume(source: &Path, target: &Path) -> Option<bool> {
    let src_id = platform::volume_id(source).ok()?;
    let anchor = nearest_existing_ancestor(target)?;
    let dst_id = platform::volume_id(&anchor).ok()?;
    Some(src_id == dst_id)
}

/// 找到路径链条上最近的一个已存在的祖先。
///
/// 用来回答「这个还不存在的 target 会落在哪个卷上」以及
/// 「那个卷还剩多少空间」。
pub fn nearest_existing_ancestor(path: &Path) -> Option<std::path::PathBuf> {
    let mut cur = crate::link::absolutize(path).ok()?;
    loop {
        if cur.symlink_metadata().is_ok() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// 目标位置所在卷的可用空间。
pub fn available_at(target: &Path) -> Option<u64> {
    let anchor = nearest_existing_ancestor(target)?;
    platform::available_space(&anchor).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_volume_is_true_within_temp_dir() {
        let tmp = std::env::temp_dir();
        let a = tmp.join("cshell_vol_a");
        let b = tmp.join("cshell_vol_b_does_not_exist_yet");
        std::fs::create_dir_all(&a).unwrap();

        assert_eq!(same_volume(&a, &b), Some(true));

        std::fs::remove_dir_all(&a).ok();
    }

    #[test]
    fn nearest_existing_ancestor_walks_up() {
        let tmp = std::env::temp_dir();
        let deep = tmp.join("cshell_anc/nope/nope/nope");
        let found = nearest_existing_ancestor(&deep).unwrap();
        // 至少要找到 temp_dir 本身
        assert!(found.symlink_metadata().is_ok());
        assert!(deep.starts_with(&found));
    }

    #[test]
    fn available_at_reports_space_for_missing_target() {
        let target = std::env::temp_dir().join("cshell_space_target_not_created");
        let space = available_at(&target).expect("应能透过已存在的父目录探出空间");
        assert!(space > 0);
    }
}
