//! 迁移台账：`~/.cshell/ledger.json`。
//!
//! 记下每一次迁移，让 `cshl list` 能看、`cshl restore` 能还原。
//!
//! 更重要的是**崩溃可恢复性**：迁移开始前就写入一条 `inflight` 记录，
//! 全部完成后才改成 `done`。中途崩溃留下的 `inflight` 记录会被
//! `cshl list` 标出来，告诉用户数据现在到底在哪一头。
//!
//! 写入用「临时文件 + 原子 rename」：绝不能出现台账文件被写了一半的情况 ——
//! 那等于把用户的迁移历史全部弄丢。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 一条迁移记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 用户当初指定的源路径 —— 现在这里是一个链接
    pub source: PathBuf,
    /// 数据现在所在的位置
    pub target: PathBuf,
    /// 当 source 本身原本就是个链接时，这里记着它当初指向的真实目录。
    /// restore 要把数据搬回**这个**位置，再把 source 指回去。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_real_path: Option<PathBuf>,
    /// 实际创建出来的链接种类（junction 可能回退成 symlink）
    pub link_kind: String,
    pub state: State,
    /// ISO-8601 风格的时间戳
    pub moved_at: String,
    pub bytes: u64,
    pub files: usize,
    pub dirs: usize,
    /// 是否走了同卷快路径（一次 rename），仅用于展示
    #[serde(default)]
    pub same_volume: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// 迁移进行中 —— 见到这个状态说明上次跑到一半崩了
    Inflight,
    Done,
}

impl Entry {
    /// 这条记录是否还留着一个可用的链接。
    pub fn link_exists(&self) -> bool {
        self.source.symlink_metadata().is_ok()
    }
}

/// 台账文件的整体结构。带版本号，方便以后演进格式。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ledger {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

fn default_version() -> u32 {
    1
}

/// 台账文件路径：`~/.cshell/ledger.json`。
pub fn ledger_path() -> Result<PathBuf> {
    // 允许用环境变量覆盖，测试与多环境隔离都要靠它
    if let Some(p) = std::env::var_os("CSHELL_HOME") {
        return Ok(PathBuf::from(p).join("ledger.json"));
    }
    let home = dirs::home_dir().ok_or_else(|| Error::Ledger("找不到用户主目录".to_string()))?;
    Ok(home.join(".cshell").join("ledger.json"))
}

/// 在持有排他文件锁的前提下，对台账做一次读-改-写。
///
/// 没有锁的话，两个并发的 `cshl` 进程会各自「读旧值 → 改 → 写回」，
/// 后写的那个把先写的记录整个覆盖掉 —— 丢记录，且 `complete()` 随后会
/// 报「找不到记录」。同时迁移多个目录是很自然的用法，必须挡住。
fn with_lock<T>(f: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
    let path = ledger_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    // 锁加在单独的 .lock 文件上，而不是 ledger.json 本身 ——
    // 后者会被原子替换（写临时文件再 rename），文件一换，加在旧 inode
    // 上的锁就形同虚设了。
    let lock_path = path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| Error::io(&lock_path, e))?;

    lock_file.lock().map_err(|e| Error::io(&lock_path, e))?;

    // 从这里开始独占。闭包里的任何提前返回都会在 lock_file 析构时解锁。
    let result = (|| {
        let mut ledger = load_from(&path)?;
        let out = f(&mut ledger)?;
        save_to(&path, &ledger)?;
        Ok(out)
    })();

    // 显式解锁，让错误路径也能干净释放（失败无所谓，析构会兜底）
    let _ = lock_file.unlock();

    result
}

/// 读出台账。文件不存在时返回空台账，而不是报错。
pub fn load() -> Result<Ledger> {
    let path = ledger_path()?;
    load_from(&path)
}

pub fn load_from(path: &Path) -> Result<Ledger> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let mut ledger: Ledger = serde_json::from_str(&s).map_err(|e| {
                Error::Ledger(format!(
                    "台账 {} 解析失败: {e}。文件可能已损坏，可手动检查或删除。",
                    path.display()
                ))
            })?;
            migrate_verbatim_paths(&mut ledger);
            Ok(ledger)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger {
            version: 1,
            entries: Vec::new(),
        }),
        Err(e) => Err(Error::io(path, e)),
    }
}

/// 归一化旧版本写下的 Windows verbatim 路径。
///
/// 0.1.0 曾把 `std::fs::canonicalize` 的结果直接当成台账的键，于是记下的是
/// `\\?\C:\Users\...`。现在的 `canonical_key` 返回的是 `C:\Users\...`，
/// 两者对不上，`restore` 会报「台账里没有记录」。读的时候顺手改过来，下一次
/// 写回台账就彻底干净了。
fn migrate_verbatim_paths(ledger: &mut Ledger) {
    for e in &mut ledger.entries {
        e.source = crate::platform::strip_verbatim(&e.source);
        e.target = crate::platform::strip_verbatim(&e.target);
        if let Some(real) = &e.original_real_path {
            e.original_real_path = Some(crate::platform::strip_verbatim(real));
        }
    }
}

/// 原子写回台账。
///
/// 先写同目录下的临时文件再 `rename` 覆盖 —— 保证任何时刻台账文件要么是
/// 旧的完整内容，要么是新的完整内容，绝不会是写了一半的残缺 JSON。
pub fn save(ledger: &Ledger) -> Result<()> {
    let path = ledger_path()?;
    save_to(&path, ledger)
}

pub fn save_to(path: &Path, ledger: &Ledger) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    let json = serde_json::to_string_pretty(ledger)
        .map_err(|e| Error::Ledger(format!("台账序列化失败: {e}")))?;

    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json.as_bytes()).map_err(|e| Error::io(&tmp, e))?;

    // rename 覆盖：这里要的就是覆盖语义，所以用标准库的 rename 而非
    // platform::rename_no_replace
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(path, e)
    })?;

    Ok(())
}

/// 追加一条 `inflight` 记录。
///
/// 整个读-改-写在文件锁内完成，并发的 `cshl` 不会互相覆盖。
pub fn begin(entry: Entry) -> Result<()> {
    with_lock(|ledger| {
        // 同一个 source 的旧记录先清掉：重复迁移同一路径时不该堆积
        ledger.entries.retain(|e| e.source != entry.source);
        ledger.entries.push(entry);
        Ok(())
    })
}

/// 把某条记录更新为完成状态。
pub fn complete(source: &Path, update: impl FnOnce(&mut Entry)) -> Result<()> {
    with_lock(|ledger| {
        let Some(entry) = ledger.entries.iter_mut().find(|e| e.source == source) else {
            return Err(Error::Ledger(format!(
                "台账里找不到 {} 的记录",
                source.display()
            )));
        };
        entry.state = State::Done;
        update(entry);
        Ok(())
    })
}

/// 删除某条记录（restore 成功后调用）。
pub fn remove(source: &Path) -> Result<()> {
    with_lock(|ledger| {
        let before = ledger.entries.len();
        ledger.entries.retain(|e| e.source != source);
        if ledger.entries.len() == before {
            return Err(Error::Ledger(format!(
                "台账里找不到 {} 的记录",
                source.display()
            )));
        }
        Ok(())
    })
}

/// 按源路径查一条记录。
pub fn find(source: &Path) -> Result<Option<Entry>> {
    let ledger = load()?;
    Ok(ledger.entries.into_iter().find(|e| e.source == source))
}

/// 当前时间戳，形如 `2026-09-11 15:04:05`。
///
/// 自己算而不是引入 chrono —— 只为了一个展示用的字符串不值得多一个依赖。
pub fn now_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (y, mo, d, h, mi, s) = civil_from_unix(now as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Unix 时间戳 → UTC 年月日时分秒。
///
/// 用的是 Howard Hinnant 的 `civil_from_days` 算法：把闰年规则平移到
/// 以 3 月为起点的纪元，闰日就落在了年末，整个换算退化成几次整数除法。
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);

    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // 纪元平移到 0000-03-01
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    // mp 是以 3 月为 0 的月份，换回 1..12
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y, m as u32, d as u32, h as u32, mi as u32, s as u32)
}

/// `cshl list` 的实现。
pub fn run_list(json: bool) -> Result<()> {
    let ledger = load()?;

    if json {
        let out = serde_json::to_string_pretty(&ledger)
            .map_err(|e| Error::Ledger(format!("序列化失败: {e}")))?;
        println!("{out}");
        return Ok(());
    }

    if ledger.entries.is_empty() {
        println!("台账为空 —— 还没有用 cshl 迁移过任何目录。");
        return Ok(());
    }

    println!("共 {} 条迁移记录：\n", ledger.entries.len());

    for e in &ledger.entries {
        let status = match e.state {
            State::Done => {
                if e.link_exists() {
                    "✓"
                } else {
                    // 数据在 target，但 source 处的链接不见了。
                    // 引用旧路径的程序现在全都会失败。
                    "⚠ 链接缺失"
                }
            }
            State::Inflight => "⚠ 未完成",
        };

        println!("  {status}  {}", e.source.display());
        println!("      → {}", e.target.display());
        if let Some(real) = &e.original_real_path {
            println!("      （原链接指向 {}）", real.display());
        }
        println!(
            "      {} · {} 文件 · {} 目录 · {} · {}",
            crate::error::human_bytes(e.bytes),
            e.files,
            e.dirs,
            e.link_kind,
            e.moved_at
        );
        if e.state == State::Inflight {
            println!(
                "      ⚠️  上次迁移未正常结束。请检查 {} 与 {} 两处，确认数据在哪一头。",
                e.source.display(),
                e.target.display()
            );
        }
        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_ledger(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_ledger_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("ledger.json")
    }

    fn sample(source: &str, target: &str) -> Entry {
        Entry {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            original_real_path: None,
            link_kind: "symlink".to_string(),
            state: State::Done,
            moved_at: now_timestamp(),
            bytes: 1024,
            files: 3,
            dirs: 2,
            same_volume: false,
        }
    }

    #[test]
    fn load_missing_file_yields_empty_ledger() {
        let p = tmp_ledger("missing");
        let l = load_from(&p).unwrap();
        assert!(l.entries.is_empty());
        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn save_and_load_roundtrip() {
        let p = tmp_ledger("roundtrip");
        let mut l = Ledger {
            version: 1,
            ..Default::default()
        };
        l.entries.push(sample("/a", "/b"));
        l.entries.push(sample("/c", "/d"));

        save_to(&p, &l).unwrap();
        let back = load_from(&p).unwrap();

        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].source, PathBuf::from("/a"));
        assert_eq!(back.entries[1].target, PathBuf::from("/d"));
        assert_eq!(back.entries[0].bytes, 1024);

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let p = tmp_ledger("notemp");
        let mut l = Ledger::default();
        l.entries.push(sample("/a", "/b"));
        save_to(&p, &l).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写入不该留下临时文件");

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_ledger_reports_clearly() {
        let p = tmp_ledger("corrupt");
        std::fs::write(&p, b"{ this is not json").unwrap();

        let err = load_from(&p).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("解析失败"), "错误信息应说清是解析问题: {msg}");

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn inflight_entry_survives_roundtrip() {
        let p = tmp_ledger("inflight");
        let mut l = Ledger::default();
        let mut e = sample("/a", "/b");
        e.state = State::Inflight;
        e.original_real_path = Some(PathBuf::from("/real"));
        l.entries.push(e);

        save_to(&p, &l).unwrap();
        let back = load_from(&p).unwrap();

        assert_eq!(back.entries[0].state, State::Inflight);
        assert_eq!(
            back.entries[0].original_real_path,
            Some(PathBuf::from("/real"))
        );

        std::fs::remove_dir_all(p.parent().unwrap()).ok();
    }

    #[test]
    fn timestamp_has_expected_shape() {
        let ts = now_timestamp();
        // YYYY-MM-DD HH:MM:SS
        assert_eq!(ts.len(), 19, "时间戳格式不对: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], " ");
        assert_eq!(&ts[13..14], ":");
    }

    #[test]
    fn civil_from_unix_matches_known_dates() {
        // Unix 纪元
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2000-03-01 00:00:00 UTC —— 跨过世纪闰年规则
        assert_eq!(civil_from_unix(951_868_800), (2000, 3, 1, 0, 0, 0));
        // 2024-02-29 12:34:56 UTC —— 闰日
        assert_eq!(civil_from_unix(1_709_210_096), (2024, 2, 29, 12, 34, 56));
        // 2026-09-11 00:00:00 UTC
        assert_eq!(civil_from_unix(1_789_084_800), (2026, 9, 11, 0, 0, 0));
    }
}
