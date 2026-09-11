//! 编排层：迁移与还原的完整流程。
//!
//! # 两条路径
//!
//! - **同卷** —— 一次 `rename`，微秒级。无论目录里有 1 个还是 1000 万个
//!   文件都一样快，零数据搬运、零中间态。
//! - **跨卷** —— 复制 → 换名 → 删除 → 建链，四个阶段。
//!
//! 分流靠「先试后判」：直接发起 rename，返回跨设备错误才走复制。
//!
//! # 跨卷时的阶段顺序
//!
//! 顺序是为「任何时刻崩溃都可恢复」设计的：
//!
//! ```text
//! 0. 台账写入 inflight 记录
//! 1. 扫描源目录
//! 2. 并行复制到同卷临时目录 <target>.cshl-partial-<pid>
//!    ⚠️ 任一文件失败 → 删掉临时目录，源目录原封不动，退出
//! 3. rename(partial → target)   ← 同卷，原子，瞬时
//! 4. 并行删除源目录
//! 5. 在源位置建链接指向 target
//! 6. 台账标记 done
//! ```
//!
//! 崩溃语义：阶段 2 崩 → 源完整；阶段 3 之后崩 → 数据完整在 target，
//! 台账的 inflight 记录会被 `cshl list` 标出来。唯一不可逆窗口是
//! 4 结束到 5 完成之间，只有亚毫秒级，且台账已记下 target。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::cli::{LinkType, MoveArgs, RestoreArgs};
use crate::error::{human_bytes, Error, Result};
use crate::ledger::{self, Entry, State};
use crate::plan::Plan;
use crate::platform::{self, LinkKind};
use crate::{copy, link, remove, safety, volume};

/// 一次迁移解析出来的全部意图。
struct Job {
    /// 用户写的源路径（最终链接就建在这里）
    source: PathBuf,
    /// 真正要搬走的目录。source 是链接时，这里是它指向的真实目录。
    real_source: PathBuf,
    /// source 原本就是链接吗？是的话 real_source != source。
    source_was_link: Option<LinkKind>,
    target: PathBuf,
}

/// `cshl move` 的实现。
pub fn run_move(args: &MoveArgs) -> Result<()> {
    let job = resolve_job(args)?;

    safety::enforce(&job.real_source, args.force)?;

    let threads = Threads::resolve(args);

    if args.dry_run {
        return dry_run(&job, &threads);
    }

    let started = Instant::now();

    // 目标的父目录不存在就自动补齐。放在 dry-run 之后 —— 预演承诺「不做任何
    // 改动」，建目录也算改动。guard 会在后面任何一步失败时把新建的空目录撤掉。
    let created_parents = CreatedParents(create_parents(&job.target)?);
    if args.verbose && !created_parents.0.is_empty() {
        for dir in &created_parents.0 {
            println!("已创建目标父目录：{}", dir.display());
        }
    }

    // 台账：先写 inflight，再动手。崩溃时用户至少知道发生过什么。
    let entry = Entry {
        source: job.source.clone(),
        target: job.target.clone(),
        original_real_path: if job.source_was_link.is_some() {
            Some(job.real_source.clone())
        } else {
            None
        },
        link_kind: String::new(),
        state: State::Inflight,
        moved_at: ledger::now_timestamp(),
        bytes: 0,
        files: 0,
        dirs: 0,
        same_volume: false,
    };
    ledger::begin(entry)?;

    // ---- 分流：先试 rename ----
    let outcome = match platform::rename_no_replace(&job.real_source, &job.target) {
        Ok(()) => {
            if args.verbose {
                println!("同卷迁移：一次原子 rename 完成");
            }
            Outcome {
                same_volume: true,
                bytes: 0,
                files: 0,
                dirs: 0,
            }
        }
        Err(e) if platform::is_cross_device(&e) => cross_volume_move(&job, args, &threads)?,
        Err(e) => {
            // rename 因为别的原因失败（目标已存在、权限不足……）
            rollback_ledger(&job.source);
            return Err(Error::io(&job.real_source, e));
        }
    };

    // ---- 建链接 ----
    let link_kind = if args.no_link {
        None
    } else {
        Some(place_link(&job, args.link_type)?)
    };

    // ---- 台账标记完成 ----
    ledger::complete(&job.source, |e| {
        e.bytes = outcome.bytes;
        e.files = outcome.files;
        e.dirs = outcome.dirs;
        e.same_volume = outcome.same_volume;
        e.link_kind = link_kind
            .map(|k| k.to_string())
            .unwrap_or_else(|| "none".to_string());
    })?;

    // 到这里迁移已彻底完成，自动创建的父目录留下不再回收
    created_parents.commit();

    report_success(&job, &outcome, link_kind, started, args);
    Ok(())
}

struct Outcome {
    same_volume: bool,
    bytes: u64,
    files: usize,
    dirs: usize,
}

/// 本次迁移实际采用的线程数。
///
/// 复制和删除的默认值不一样（见 [`default_copy_threads`]），但 `-t` 一旦指定
/// 就两边都用它 —— 集中在这里算一次，省得 `--verbose` / `--dry-run` 各自
/// 推一遍还推错。
struct Threads {
    copy: usize,
    remove: usize,
    /// 是不是用户用 `-t` 显式指定的
    explicit: bool,
}

impl Threads {
    fn resolve(args: &MoveArgs) -> Self {
        match args.threads {
            Some(n) => Threads {
                copy: n,
                remove: n,
                explicit: true,
            },
            None => Threads {
                copy: default_copy_threads(),
                remove: default_remove_threads(),
                explicit: false,
            },
        }
    }

    /// 给 `--verbose` 与 `--dry-run` 用的一行说明。
    fn describe(&self) -> String {
        format!(
            "复制 {}，删除 {}（{}）",
            self.copy,
            self.remove,
            if self.explicit {
                "-t 指定"
            } else {
                "按本机逻辑核数自动选择"
            }
        )
    }
}

/// 跨卷迁移的四个阶段。
fn cross_volume_move(job: &Job, args: &MoveArgs, threads: &Threads) -> Result<Outcome> {
    if args.verbose {
        println!("跨卷迁移：需要复制数据");
        println!("线程数：{}", threads.describe());
    }

    // ---- 阶段 1：扫描 ----
    let scan_start = Instant::now();
    let plan = crate::plan::scan(&job.real_source, threads.copy).inspect_err(|_| {
        rollback_ledger(&job.source);
    })?;
    let scan_time = scan_start.elapsed();

    if args.verbose {
        println!(
            "扫描完成：{} 目录 · {} 文件 · {} 链接 · {} · 耗时 {:.2?}",
            plan.dirs.len(),
            plan.files.len(),
            plan.symlinks.len(),
            human_bytes(plan.total_bytes),
            scan_time
        );
    }
    report_unreadable(&plan);

    // ---- 空间预检 ----
    // 复制到一半才发现写不下，是最糟糕的失败方式。
    if let Some(available) = volume::available_at(&job.target) {
        if plan.total_bytes > available {
            rollback_ledger(&job.source);
            return Err(Error::InsufficientSpace {
                needed: plan.total_bytes,
                available,
                target: job.target.clone(),
            });
        }
    }

    // ---- 阶段 2：复制到同卷临时目录 ----
    // 写临时目录而不是直接写 target：崩在中途时 target 不会留下一个
    // 看起来完整、实则残缺的目录。
    let partial = copy::partial_path(&job.target);
    copy::cleanup_partial(&partial); // 清掉上次崩溃的残留

    if let Err(e) = platform::create_dir(&partial) {
        rollback_ledger(&job.source);
        return Err(Error::io(&partial, e));
    }

    let copy_start = Instant::now();
    let progress = Arc::new(copy::Progress::new());
    let reporter = spawn_progress_reporter(&plan, Arc::clone(&progress), args);

    let progress = copy::copy_tree(&job.real_source, &partial, &plan, threads.copy, progress);

    if let Some(r) = reporter {
        r.finish();
    }

    if progress.is_aborted() {
        // 复制失败：清掉半成品，源目录自始至终没被碰过
        copy::cleanup_partial(&partial);
        rollback_ledger(&job.source);
        let failures = progress.take_failures();
        let reason = failures
            .first()
            .map(|f| format!("{}: {}", f.path.display(), f.error))
            .unwrap_or_else(|| "未知原因".to_string());
        return Err(Error::Aborted { reason, failures });
    }

    let copy_time = copy_start.elapsed();
    if args.verbose {
        let secs = copy_time.as_secs_f64();
        let rate = if secs > 0.0 {
            plan.total_bytes as f64 / secs
        } else {
            0.0
        };
        println!(
            "复制完成：{} · 耗时 {:.2?} · {}/s",
            human_bytes(plan.total_bytes),
            copy_time,
            human_bytes(rate as u64)
        );
    }

    // ---- 阶段 3：原子换名 ----
    // partial 和 target 同级同卷，这一步是瞬时的。
    // 到这里为止，数据已经完整地在目标位置了。
    if let Err(e) = platform::rename_no_replace(&partial, &job.target) {
        copy::cleanup_partial(&partial);
        rollback_ledger(&job.source);
        return Err(Error::io(&job.target, e));
    }

    // ---- 阶段 4：删除源目录 ----
    if args.verbose {
        println!("数据已就位，开始清理源目录…");
    }
    let rm_start = Instant::now();
    let rm_progress = remove::remove_tree(
        &job.real_source,
        &plan,
        threads.remove,
        Arc::new(remove::RemoveProgress::new()),
    );

    let rm_failures = rm_progress.take_failures();
    if !rm_failures.is_empty() {
        // 数据已经安全在 target 了，但源没清干净。
        // 这不能回滚（回滚意味着删掉刚搬好的数据），只能如实报告。
        // 台账保持 inflight，让 cshl list 把它标出来。
        let total = plan.total_items();
        return Err(Error::PartialFailure {
            total,
            failed: rm_failures.len(),
            failures: rm_failures,
        });
    }

    if args.verbose {
        println!("源目录已清理，耗时 {:.2?}", rm_start.elapsed());
    }

    Ok(Outcome {
        same_volume: false,
        bytes: plan.total_bytes,
        files: plan.files.len() + plan.symlinks.len(),
        dirs: plan.dirs.len(),
    })
}

/// 在源位置留下链接。
///
/// 两种情况：
/// - source 原本是普通目录 → 现在它已经被搬走了，直接建新链接
/// - source 原本就是链接 → 它还在原地（被搬走的是它指向的真实目录），
///   要把它改指到新 target
fn place_link(job: &Job, preferred: LinkType) -> Result<LinkKind> {
    if job.source_was_link.is_some() {
        // 穿透重定向：原链接还在，改指即可。
        // Unix 上这一步是原子的（临时链接 + rename 覆盖）。
        link::repoint_dir_link(&job.source, &job.target, preferred)
            .map_err(|e| Error::io(&job.source, e))
    } else {
        link::create_dir_link(&job.target, &job.source, preferred)
            .map_err(|e| Error::io(&job.source, e))
    }
}

/// 解析用户给的两个路径，定出真正要做什么。
fn resolve_job(args: &MoveArgs) -> Result<Job> {
    // source 用 canonical_key 而不是 absolutize：台账以源路径为键，必须唯一。
    // macOS 上 /tmp 是指向 /private/tmp 的符号链接，两种写法指同一个目录，
    // 不统一的话台账里会出现两条记录、restore 也会按用户的写法找不到。
    let source = link::canonical_key(&args.source).map_err(|e| Error::io(&args.source, e))?;
    let target = link::canonical_key(&args.target).map_err(|e| Error::io(&args.target, e))?;

    // 源必须存在
    let src_md = source
        .symlink_metadata()
        .map_err(|_| Error::invalid(&source, "路径不存在"))?;

    // 源是不是一个链接？
    let source_kind = link::kind_of(&source).map_err(|e| Error::io(&source, e))?;

    let real_source = match source_kind {
        Some(kind) => {
            if args.no_follow_link {
                return Err(Error::invalid(
                    &source,
                    format!(
                        "这是一个 {kind}，指向 {}。\
                         去掉 --no-follow-link 可自动穿透到真实目录并改写链接。",
                        link::target_of(&source)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| "<读取失败>".to_string())
                    ),
                ));
            }

            // 穿透：完全解析到真实目录（链套链也能到底）
            let real = link::canonicalize(&source).map_err(|e| {
                Error::io(
                    &source,
                    std::io::Error::new(e.kind(), format!("无法解析链接指向的真实目录: {e}")),
                )
            })?;

            if !real.is_dir() {
                return Err(Error::invalid(&source, "这个链接指向的不是一个目录"));
            }
            real
        }
        None => {
            if !src_md.is_dir() {
                return Err(Error::invalid(&source, "不是一个目录"));
            }
            source.clone()
        }
    };

    validate_paths(&source, &real_source, &target)?;

    Ok(Job {
        source,
        real_source,
        source_was_link: source_kind,
        target,
    })
}

/// 那些绝不该被 `--force` 越过的逻辑错误。
///
/// 和 [`safety`] 里的检查不同 —— 那些是「危险但你可能知道自己在干嘛」，
/// 这些是「无论如何都会导致数据损坏」。
fn validate_paths(source: &Path, real_source: &Path, target: &Path) -> Result<()> {
    // 源和目标是同一个地方
    if real_source == target || source == target {
        return Err(Error::invalid(target, "源和目标是同一个路径"));
    }

    // 目标在源内部 → 自吞：搬的过程中会把目标自己也搬进去，无限递归
    if target.starts_with(real_source) {
        return Err(Error::invalid(
            target,
            format!(
                "目标位于源目录内部（{}），这会导致自吞",
                real_source.display()
            ),
        ));
    }

    // 源在目标内部
    if real_source.starts_with(target) && target.symlink_metadata().is_ok() {
        return Err(Error::invalid(target, "源目录位于目标内部，无法迁移"));
    }

    // 目标已存在。允许「已存在但是个空目录」—— 用户先 mkdir 了是很自然的事。
    if let Ok(md) = target.symlink_metadata() {
        if !md.is_dir() {
            return Err(Error::invalid(target, "目标已存在且不是目录"));
        }
        let empty = std::fs::read_dir(target)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !empty {
            return Err(Error::invalid(target, "目标目录已存在且非空"));
        }
        // 目标是空目录：删掉它，好让后面的 rename 能直接落位。
        // rename 到一个已存在的目录在各平台上语义不一，不如先清场。
        std::fs::remove_dir(target).map_err(|e| Error::io(target, e))?;
    }

    // 目标的父目录必须存在 —— 不存在的话由 [`create_parents`] 在真正动手前
    // 补齐（dry-run 不会创建任何东西）。这里只确认 target 本身有父目录可言。
    if target.parent().is_none() {
        return Err(Error::invalid(target, "目标路径没有父目录"));
    }

    Ok(())
}

/// 算出为了让 `target` 落位，需要新建哪几层父目录。**只看不动手。**
///
/// 返回值按「从浅到深」排列。已经存在的层级不会出现在里面 —— 后面
/// [`CreatedParents`] 要靠它决定失败时删哪些，绝不能把用户原有的目录
/// 也当成自己的产物。
///
/// `--dry-run` 和真正执行共用这一个函数，这样预演报的和实际做的不会走偏。
fn missing_parents(target: &Path) -> Result<Vec<PathBuf>> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::invalid(target, "目标路径没有父目录"))?;

    if parent.is_dir() {
        return Ok(Vec::new());
    }

    // 从最深的缺失层一路往上，走到第一个已存在的祖先为止
    let mut missing = Vec::new();
    let mut cur = parent;
    loop {
        if cur.is_dir() {
            break;
        }
        // 存在但不是目录：中间挡着一个文件，再往下建必然失败，
        // 不如在这里给一句说得清的话
        if cur.symlink_metadata().is_ok() {
            return Err(Error::invalid(cur, "目标路径中的这一段已存在，但不是目录"));
        }
        match cur.parent() {
            Some(up) => {
                missing.push(cur.to_path_buf());
                cur = up;
            }
            // 一路走到根都不存在 —— 那是盘符/挂载点的问题，不是「还没建目录」。
            // mkdir 一个不存在的盘根是办不到的，别假装能修。
            None => {
                return Err(Error::invalid(
                    cur,
                    "这个盘符（或根路径）不存在，无法在其中创建目录",
                ))
            }
        }
    }

    // 反过来，从浅到深依次创建
    missing.reverse();
    Ok(missing)
}

/// 逐级补齐 `target` 缺失的父目录，返回**本次新建**的那几层。
///
/// 层级由 [`missing_parents`] 算出；返回值交给 [`CreatedParents`] 在失败时
/// 反着删回去。
fn create_parents(target: &Path) -> Result<Vec<PathBuf>> {
    let missing = missing_parents(target)?;

    let mut created = Vec::with_capacity(missing.len());
    for dir in missing {
        platform::create_dir(&dir).map_err(|e| Error::io(&dir, e))?;
        created.push(dir);
    }
    Ok(created)
}

/// 自动补出来的父目录，迁移失败时负责撤销。
///
/// 用 `Drop` 而不是在每个错误分支手动清理：`cross_volume_move` 里有近十处
/// 提前返回，漏掉任何一处都会在用户打错路径时留下一串空目录 —— 而「怕留下
/// 意外的空目录」正是这里当初直接报错、不肯自动创建的原因。
///
/// 只删**自己建的**、且**仍然为空**的目录：[`platform::remove_dir`] 删不动
/// 非空目录，所以数据真落进去了的话这里会安全地失败。
struct CreatedParents(Vec<PathBuf>);

impl CreatedParents {
    /// 迁移成功，这些目录留下。
    fn commit(mut self) {
        self.0.clear();
    }
}

impl Drop for CreatedParents {
    fn drop(&mut self) {
        // 由深到浅：先删最里面那层，外层才可能变空
        for dir in self.0.iter().rev() {
            if platform::remove_dir(dir).is_err() {
                // 最深的一层都删不掉（非空），外面几层更不可能空
                break;
            }
        }
    }
}

/// `--dry-run`：扫描并报告，不做任何改动。
fn dry_run(job: &Job, threads: &Threads) -> Result<()> {
    println!("预演模式 —— 不会做任何改动\n");

    println!("  源:   {}", job.real_source.display());
    if let Some(kind) = job.source_was_link {
        println!(
            "        （{} 是一个 {kind}，将穿透到上面这个真实目录）",
            job.source.display()
        );
    }
    println!("  目标: {}", job.target.display());
    match missing_parents(&job.target) {
        Ok(dirs) if !dirs.is_empty() => println!(
            "        （父目录不存在，将自动创建 {} 层，最深一层是 {}）",
            dirs.len(),
            dirs.last().expect("刚判过非空").display()
        ),
        // 预演的职责是把「真跑会发生什么」如实说出来，包括「会失败」
        Err(e) => println!("        ⚠️  {e}，实际执行会被拒绝"),
        _ => {}
    }

    let same_vol = volume::same_volume(&job.real_source, &job.target);
    match same_vol {
        Some(true) => {
            println!("\n  判定: 同卷 —— 一次原子 rename 即可完成");
            println!("  预计: 微秒级，与目录大小无关，无需复制任何数据");
            println!("\n  （实际执行时仍会先尝试 rename，以返回值为准）");
            return Ok(());
        }
        Some(false) => println!("\n  判定: 跨卷 —— 需要复制数据后删除源目录"),
        None => println!("\n  判定: 无法预先探测卷，执行时以 rename 的返回值为准"),
    }

    println!("  线程: {}", threads.describe());

    let start = Instant::now();
    let plan = crate::plan::scan(&job.real_source, threads.copy)?;
    let elapsed = start.elapsed();

    println!("\n  将要搬运:");
    println!("    {} 个目录", plan.dirs.len());
    println!("    {} 个文件", plan.files.len());
    println!("    {} 个符号链接（原样重建，不跟随）", plan.symlinks.len());
    println!("    共 {}", human_bytes(plan.total_bytes));

    let groups = crate::plan::hardlink_groups(&plan.files);
    if !groups.is_empty() {
        let linked: usize = groups.values().map(|v| v.len()).sum();
        let saved: u64 = groups
            .values()
            .map(|idxs| idxs[1..].iter().map(|&i| plan.files[i].size).sum::<u64>())
            .sum();
        println!(
            "    检测到 {} 组硬链接（{} 个文件），去重可省下 {}",
            groups.len(),
            linked,
            human_bytes(saved)
        );
    }

    if let Some(available) = volume::available_at(&job.target) {
        println!("\n  目标卷可用空间: {}", human_bytes(available));
        if plan.total_bytes > available {
            println!("    ⚠️  空间不足！实际执行会被拒绝。");
        }
    }

    report_unreadable(&plan);

    println!("\n  扫描耗时 {:.2?}", elapsed);
    println!(
        "\n  执行迁移: cshl {} {}",
        job.source.display(),
        job.target.display()
    );

    Ok(())
}

/// `cshl restore` 的实现：把数据搬回原位，去掉链接。
pub fn run_restore(args: &RestoreArgs) -> Result<()> {
    // 和 run_move 用同一种规范化，否则用户换个写法就查不到记录
    let source = link::canonical_key(&args.source).map_err(|e| Error::io(&args.source, e))?;

    let entry = ledger::find(&source)?.ok_or_else(|| {
        Error::Ledger(format!(
            "台账里没有 {} 的迁移记录。`cshl list` 可查看全部记录。",
            source.display()
        ))
    })?;

    if entry.state == State::Inflight {
        return Err(Error::Ledger(format!(
            "{} 的上一次迁移没有正常结束，不能自动还原。\
             请手动检查 {} 与 {} 两处确认数据在哪一头。",
            source.display(),
            source.display(),
            entry.target.display()
        )));
    }

    if !entry.target.is_dir() {
        return Err(Error::invalid(
            &entry.target,
            "数据所在的目标目录已不存在，无法还原",
        ));
    }

    // 数据要搬回哪里？source 当初本身就是链接的话，搬回它原本指向的真实目录。
    let restore_to = entry
        .original_real_path
        .clone()
        .unwrap_or_else(|| source.clone());

    let threads = args.threads.unwrap_or_else(default_copy_threads);

    if args.dry_run {
        println!("预演模式 —— 不会做任何改动\n");
        println!(
            "  将把 {} 搬回 {}",
            entry.target.display(),
            restore_to.display()
        );
        if entry.original_real_path.is_some() {
            println!(
                "  并把链接 {} 改指回 {}",
                source.display(),
                restore_to.display()
            );
        } else {
            println!("  并删除 {} 处的链接", source.display());
        }
        return Ok(());
    }

    let started = Instant::now();

    // 先把挡路的链接摘掉 —— 它占着 restore_to 这个位置。
    // 只有当 source 就是 restore_to（即当初不是链接）时才需要这一步。
    let link_removed = if restore_to == source {
        match link::kind_of(&source).map_err(|e| Error::io(&source, e))? {
            Some(_) => {
                link::remove_dir_link(&source).map_err(|e| Error::io(&source, e))?;
                true
            }
            None => {
                return Err(Error::invalid(
                    &source,
                    "这个位置不是一个链接，不像是 cshl 迁移的结果",
                ))
            }
        }
    } else {
        false
    };

    // 搬回去。同卷一次 rename，跨卷走完整的复制流程。
    let outcome = match platform::rename_no_replace(&entry.target, &restore_to) {
        Ok(()) => Outcome {
            same_volume: true,
            bytes: 0,
            files: 0,
            dirs: 0,
        },
        Err(e) if platform::is_cross_device(&e) => {
            let restore_job = Job {
                source: restore_to.clone(),
                real_source: entry.target.clone(),
                source_was_link: None,
                target: restore_to.clone(),
            };
            match restore_cross_volume(&restore_job, threads, args.verbose) {
                Ok(o) => o,
                Err(err) => {
                    // 还原失败：把刚摘掉的链接补回去，别让用户两头落空
                    if link_removed {
                        let _ = link::create_dir_link(&entry.target, &source, LinkType::Junction);
                    }
                    return Err(err);
                }
            }
        }
        Err(e) => {
            if link_removed {
                let _ = link::create_dir_link(&entry.target, &source, LinkType::Junction);
            }
            return Err(Error::io(&entry.target, e));
        }
    };

    // source 当初本身就是链接：把它改指回真实位置
    if entry.original_real_path.is_some() {
        link::repoint_dir_link(&source, &restore_to, LinkType::Junction)
            .map_err(|e| Error::io(&source, e))?;
    }

    ledger::remove(&source)?;

    println!(
        "✓ 已还原到 {}（耗时 {:.2?}{}）",
        restore_to.display(),
        started.elapsed(),
        if outcome.same_volume {
            "，同卷 rename"
        } else {
            ""
        }
    );

    Ok(())
}

/// 还原时的跨卷搬运。和 [`cross_volume_move`] 同构，但不写台账
/// （台账条目在 restore 成功后整条删掉）。
fn restore_cross_volume(job: &Job, threads: usize, verbose: bool) -> Result<Outcome> {
    let plan = crate::plan::scan(&job.real_source, threads)?;

    if let Some(available) = volume::available_at(&job.target) {
        if plan.total_bytes > available {
            return Err(Error::InsufficientSpace {
                needed: plan.total_bytes,
                available,
                target: job.target.clone(),
            });
        }
    }

    let partial = copy::partial_path(&job.target);
    copy::cleanup_partial(&partial);
    platform::create_dir(&partial).map_err(|e| Error::io(&partial, e))?;

    let progress = copy::copy_tree(
        &job.real_source,
        &partial,
        &plan,
        threads,
        Arc::new(copy::Progress::new()),
    );

    if progress.is_aborted() {
        copy::cleanup_partial(&partial);
        let failures = progress.take_failures();
        let reason = failures
            .first()
            .map(|f| format!("{}: {}", f.path.display(), f.error))
            .unwrap_or_else(|| "未知原因".to_string());
        return Err(Error::Aborted { reason, failures });
    }

    platform::rename_no_replace(&partial, &job.target).map_err(|e| {
        copy::cleanup_partial(&partial);
        Error::io(&job.target, e)
    })?;

    let rm = remove::remove_tree(
        &job.real_source,
        &plan,
        threads,
        Arc::new(remove::RemoveProgress::new()),
    );
    let failures = rm.take_failures();
    if !failures.is_empty() {
        return Err(Error::PartialFailure {
            total: plan.total_items(),
            failed: failures.len(),
            failures,
        });
    }

    if verbose {
        println!("跨卷还原完成：{}", human_bytes(plan.total_bytes));
    }

    Ok(Outcome {
        same_volume: false,
        bytes: plan.total_bytes,
        files: plan.files.len() + plan.symlinks.len(),
        dirs: plan.dirs.len(),
    })
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 复制是 I/O 密集而非 CPU 密集，线程数可以超过核数 —— 高队列深度才能
/// 让 NVMe 跑满。但机械盘上并发过高会导致寻道抖动，那种场景应手动 `-t 1`。
///
/// 公开是为了让 `--help` 能把**算出来的数字**直接印给用户看
/// （见 [`crate::cli`]），而不是只给一条公式让人自己去数核。
pub fn default_copy_threads() -> usize {
    (logical_cores() * 4).min(32)
}

/// 删除以元数据操作为主，按核数来就够。
pub fn default_remove_threads() -> usize {
    logical_cores()
}

/// 本机逻辑核数，探测不到时按 4 估。
pub fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// 迁移失败时把刚写的 inflight 台账记录撤掉。
///
/// 只在「源目录确定没被碰过」的路径上调用 —— 此时留着记录反而会误导用户
/// 以为发生过一次半截的迁移。
fn rollback_ledger(source: &Path) {
    let _ = ledger::remove(source);
}

fn report_unreadable(plan: &Plan) {
    if plan.unreadable.is_empty() {
        return;
    }
    eprintln!(
        "\n⚠️  有 {} 项无法读取（权限不足等），它们不会被迁移:",
        plan.unreadable.len()
    );
    for (p, e) in plan.unreadable.iter().take(10) {
        eprintln!("    {}: {}", p.display(), e);
    }
    if plan.unreadable.len() > 10 {
        eprintln!("    ... 另有 {} 项", plan.unreadable.len() - 10);
    }
}

fn report_success(
    job: &Job,
    outcome: &Outcome,
    link_kind: Option<LinkKind>,
    started: Instant,
    args: &MoveArgs,
) {
    if args.quiet {
        return;
    }

    let elapsed = started.elapsed();

    if outcome.same_volume {
        println!(
            "✓ 已迁移到 {}（同卷 rename，耗时 {:.2?}）",
            job.target.display(),
            elapsed
        );
    } else {
        println!(
            "✓ 已迁移到 {}（{} · {} 文件 · {} 目录，耗时 {:.2?}）",
            job.target.display(),
            human_bytes(outcome.bytes),
            outcome.files,
            outcome.dirs,
            elapsed
        );
    }

    match link_kind {
        Some(kind) => {
            println!(
                "  {} → {} （{kind}）",
                job.source.display(),
                job.target.display()
            );
            if job.source_was_link.is_some() {
                println!("  （原链接已改指到新位置）");
            }
        }
        None => println!("  未创建链接（--no-link）"),
    }
}

/// 复制期间的进度条。`--quiet` 或非终端环境下返回 `None`。
fn spawn_progress_reporter(
    plan: &Plan,
    progress: Arc<copy::Progress>,
    args: &MoveArgs,
) -> Option<ProgressReporter> {
    if args.quiet || plan.total_bytes == 0 {
        return None;
    }

    use indicatif::{ProgressBar, ProgressStyle};

    let bar = ProgressBar::new(plan.total_bytes);
    bar.set_style(
        ProgressStyle::with_template(
            "  {bar:32} {bytes}/{total_bytes} ({bytes_per_sec}) 剩余 {eta}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar()),
    );

    let bar_clone = bar.clone();
    let handle = std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let done = progress.bytes_done.load(Ordering::Relaxed);
            bar_clone.set_position(done);
            if bar_clone.is_finished() {
                break;
            }
        }
    });

    Some(ProgressReporter {
        bar,
        handle: Some(handle),
    })
}

struct ProgressReporter {
    bar: indicatif::ProgressBar,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProgressReporter {
    fn finish(mut self) {
        self.bar.finish_and_clear();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cshell_mig_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn validate_rejects_target_inside_source() {
        let d = tmpdir("selfswallow");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        let target = src.join("inner/deep");

        let err = validate_paths(&src, &src, &target).unwrap_err();
        assert!(err.to_string().contains("自吞"), "应识别出自吞: {err}");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn validate_rejects_identical_paths() {
        let d = tmpdir("same");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();

        assert!(validate_paths(&src, &src, &src).is_err());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn validate_rejects_nonempty_target() {
        let d = tmpdir("nonempty");
        let src = d.join("src");
        let target = d.join("target");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("occupied.txt"), b"x").unwrap();

        let err = validate_paths(&src, &src, &target).unwrap_err();
        assert!(err.to_string().contains("非空"), "应拒绝非空目标: {err}");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn validate_accepts_and_clears_empty_target() {
        let d = tmpdir("emptytarget");
        let src = d.join("src");
        let target = d.join("target");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&target).unwrap();

        // 空目标应被接受，并且被清掉好让 rename 落位
        validate_paths(&src, &src, &target).unwrap();
        assert!(!target.exists(), "空目标应被移除以便 rename 直接落位");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn create_parents_builds_the_whole_chain() {
        let d = tmpdir("mkparents");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        let target = d.join("a/b/c/target");

        // 缺失的父目录不再是错误
        validate_paths(&src, &src, &target).unwrap();

        let created = create_parents(&target).unwrap();
        assert!(target.parent().unwrap().is_dir(), "整条路径都该被建出来");
        assert_eq!(created.len(), 3, "应记下自己新建的三层：{created:?}");
        assert!(created[0].ends_with("a"), "返回值要从浅到深排列");
        assert!(created[2].ends_with("c"));
        // target 本身不该被创建 —— 那是 rename 的落点
        assert!(!target.exists());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn create_parents_only_reports_dirs_it_made() {
        let d = tmpdir("mkpartial");
        let existing = d.join("already");
        std::fs::create_dir(&existing).unwrap();
        let target = existing.join("new/target");

        let created = create_parents(&target).unwrap();
        assert_eq!(created.len(), 1, "已存在的层级不能算进来：{created:?}");
        assert!(created[0].ends_with("new"));

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn create_parents_is_a_noop_when_parent_exists() {
        let d = tmpdir("mknoop");
        let target = d.join("target");
        assert!(create_parents(&target).unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn create_parents_rejects_a_file_in_the_way() {
        let d = tmpdir("mkblocked");
        let blocker = d.join("blocker");
        std::fs::write(&blocker, b"i am not a directory").unwrap();

        let err = create_parents(&blocker.join("sub/target")).unwrap_err();
        assert!(
            err.to_string().contains("不是目录"),
            "路径中间挡着文件时要说清楚: {err}"
        );

        std::fs::remove_dir_all(&d).ok();
    }

    /// 一路缺到根，说明是盘符/挂载点不存在 —— 那不是「还没建目录」，
    /// mkdir 一个不存在的盘根是办不到的，不能假装能自动补。
    #[test]
    fn create_parents_refuses_to_invent_a_missing_root() {
        #[cfg(windows)]
        let target = Path::new(r"Q:\nope\deeper\target");
        #[cfg(not(windows))]
        let target = Path::new("/nonexistent_root_xyz/nope/target");

        // 真有这个盘/目录的机器上这个断言没意义，跳过
        #[cfg(windows)]
        if Path::new(r"Q:\").is_dir() {
            return;
        }
        #[cfg(not(windows))]
        if Path::new("/nonexistent_root_xyz").exists() {
            return;
        }

        #[cfg(windows)]
        {
            let err = missing_parents(target).unwrap_err();
            assert!(
                err.to_string().contains("盘符"),
                "应指出是盘符不存在而不是说要自动创建: {err}"
            );
        }
        // Unix 上 / 总是存在，缺的层级是可以真建出来的，
        // 所以这里只确认它不报错地给出待建列表
        #[cfg(not(windows))]
        assert!(!missing_parents(target).unwrap().is_empty());
    }

    /// 预演算出来的待建层级，必须和真跑时建出来的完全一致。
    #[test]
    fn missing_parents_matches_what_create_parents_makes() {
        let d = tmpdir("mkmatch");
        let target = d.join("m/n/o/target");

        let planned = missing_parents(&target).unwrap();
        let created = create_parents(&target).unwrap();
        assert_eq!(planned, created, "预演与实际必须一致");
        // 算完之后再算一次：这次一层都不缺了
        assert!(missing_parents(&target).unwrap().is_empty());

        std::fs::remove_dir_all(&d).ok();
    }

    /// 迁移失败时，自动建出来的空目录要撤干净 —— 打错一个路径不该在磁盘上
    /// 留下一串空壳。
    #[test]
    fn created_parents_are_rolled_back_on_failure() {
        let d = tmpdir("mkrollback");
        let target = d.join("x/y/z/target");

        let created = create_parents(&target).unwrap();
        assert!(d.join("x/y/z").is_dir());

        drop(CreatedParents(created));

        assert!(!d.join("x").exists(), "失败后自动建的目录应全部撤掉");
        assert!(d.is_dir(), "原本就存在的目录不能动");

        std::fs::remove_dir_all(&d).ok();
    }

    /// 反过来：数据已经落进去了就绝不能删。
    #[test]
    fn created_parents_rollback_spares_nonempty_dirs() {
        let d = tmpdir("mkkeep");
        let target = d.join("x/y/target");

        let created = create_parents(&target).unwrap();
        // 模拟 rename 已经把数据放到位
        std::fs::create_dir(&target).unwrap();

        drop(CreatedParents(created));

        assert!(target.is_dir(), "非空目录必须原样保留");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn created_parents_commit_keeps_everything() {
        let d = tmpdir("mkcommit");
        let target = d.join("x/y/target");

        let created = create_parents(&target).unwrap();
        CreatedParents(created).commit();

        assert!(d.join("x/y").is_dir(), "commit 之后目录必须留下");

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn default_thread_counts_are_sane() {
        let c = default_copy_threads();
        let r = default_remove_threads();
        assert!((1..=32).contains(&c), "复制线程数应在 1..=32：{c}");
        assert!(r >= 1, "删除线程数至少为 1：{r}");
        assert!(c >= r, "复制是 I/O 密集，并发度应不低于删除");
    }
}
