# cshell 设计说明

面向维护者。README 讲「怎么用、为什么快」，这里讲「为什么这么写」——
那些不写下来、半年后自己都会想改错的决定。

---

## 1. 核心分流：同卷与跨卷

整个工具的性能命门只有一句话：**同卷迁移根本不需要复制数据**。

一次 `rename(2)` 就把目录项从一个父目录挪到另一个，无论目录里有 1 个还是
1000 万个文件都是常数时间。跨卷则必须逐字节搬运。两条路径的代价差了好几个
数量级，所以编排层第一件事就是分流。

### 为什么是「先试后判」而不是「先判后试」

直觉上应该先探测两边的卷 ID，相同就 rename、不同就复制。实际实现用的是反
过来的顺序：**直接发起 rename，返回 `EXDEV` / `ERROR_NOT_SAME_DEVICE` 才走
复制**。

理由是预测不可靠而实测可靠：

- 同一个卷可以挂载在多个挂载点上，`st_dev` 相同但路径毫无关系
- macOS 的 APFS firmlink 让 `/` 与 `/System/Volumes/Data` 互相穿透，卷 ID 的
  语义很微妙
- Linux 的 bind mount、overlayfs 各有各的边界情况
- 网络文件系统的行为取决于具体实现

而一次失败的 rename 只花微秒级，代价可以忽略。卷 ID 探测（`volume.rs`）因此
**只用于 dry-run 的预检报告**，绝不参与实际分流决策——这一点在
`platform::volume_id` 的文档注释里也写明了，避免以后有人拿它去做判断。

### Windows 上的一个额外理由

`MoveFileExW` 即使加了 `MOVEFILE_COPY_ALLOWED`，**对目录跨卷也是不生效的**
（MSDN 明确写了 "This value cannot be used with directories"）。所以跨卷目录
迁移在 Windows 上本来就必须自己实现，没有捷径。这正好让三个平台的跨卷路径
统一成同一套代码。

---

## 2. 跨卷的阶段顺序

```
0. 台账写入 inflight 记录
1. 扫描源目录
2. 并行复制到 <target>.cshl-partial-<pid>
3. rename(partial → target)     ← 同卷原子，瞬时
4. 并行删除源目录
5. 原地建链接
6. 台账标记 done
```

这个顺序是**按崩溃点倒推设计的**，每一步的位置都有理由。

### 为什么复制到临时目录而不是直接写 target

如果直接往 `target` 写，崩在中途会留下一个「看起来完整、实则残缺」的目录。
用户（或者下一次 `cshl` 运行）无从判断它是完整的还是半截的。

写到同级的 `.<name>.cshl-partial-<pid>` 再 rename，就把「完成」这件事压缩成
了一个原子操作：`target` 要么不存在，要么就是完整的。

临时目录必须和 `target` **同级**（`copy::partial_path` 保证了这一点），否则
第 3 步的 rename 就跨卷了，那这一步本身又变成一次全量复制，整个设计就塌了。

### 为什么先删源再建链接，而不是反过来

反过来（先建链接再删源）做不到——源目录还占着那个路径，链接建不上去。

所以第 4→5 之间有一个源已删、链接未建的窗口。这是整个流程里**唯一不可逆的
时刻**，但它只有亚毫秒级（建链接是纯元数据操作），而且台账里已经记下了
`target`，人工恢复只需要重建一个链接。能做到的最好程度就是这样。

### 台账为什么要在动手前写

`inflight` 记录的价值全在崩溃时：用户跑 `cshl list` 能看到「这次迁移没正常
结束，去 A 和 B 两处确认数据在哪一头」。如果等成功了再写，崩溃时台账里什么
都没有，用户只能自己去猜。

对应地，**确认源目录没被碰过的失败路径要撤掉这条记录**（`rollback_ledger`），
否则会留下一条误导性的「半截迁移」记录。

---

## 3. 两个方向相反的调度器

复制和删除的依赖方向正好相反，所以用了两套调度。这不是重复，是必须的。

### 复制：先建骨架，把依赖彻底消除

复制的依赖是「父目录必须先存在」。朴素做法是边遍历边复制，那样并行度受树的
形状限制。

`copy::copy_tree` 的做法是**先按深度顺序把所有目录建出来**（`plan.dirs` 在
扫描结束时就按 `components().count()` 排好序了），然后所有文件之间再无任何
依赖，可以完全并行地派发。目录数量远少于文件数，且建目录是纯元数据操作，
这一步的串行代价可以忽略。

文件按大小**降序**派发（LPT, Longest Processing Time first）。这是经典的贪心
调度：最大的任务最先分配，避免它在所有小任务干完之后独自拖尾。一个 8 GB 的
文件如果最后才开始复制，后面的时间里只有一个线程在干活。

### 删除：依赖图 + 叶子优先

删除的依赖是「目录必须先清空才能删」，方向朝上，没法用「先建骨架」那种
消除法。`remove::Broker` 维护一张依赖图：每个目录记着还有多少子目录没删完，
归零时它自己变成新的叶子进队列。

这套模型移植自 rmbrr（`/Users/xuqiao/code/dev/rmbrr/src/broker.rs`）。

### 扫描：活跃计数模型

扫描时树的形状还不知道，所以既不能预先分配任务，也没有依赖图。用的是
「待扫描队列 + 活跃计数」：`pending` 记录「队列中 + 正在处理中」的目录数，
归零即扫完。

**这里踩过一个坑**：早期版本让每个工作线程各持一个 `Sender` 副本，结果通道
永远不会断开——所有线程扫完后一起阻塞在 `recv()` 上，死锁。现在 `Sender`
由 `SharedState` 独占持有（`Mutex<Option<Sender>>`），`pending` 归零时置 `None`
丢掉它，通道断开，所有线程的 `recv()` 一起返回 `Err` 退出。`remove::Broker`
用的是同一个模式。

另一个细节：**必须先 `fetch_add` 再入队**。顺序反了会出现「队列已空、计数
归零」的假完成，导致扫描提前结束、漏掉一整棵子树。

---

## 4. 平台层

`src/platform/` 对上暴露统一签名，按 `cfg` 分发。不直接用 `std::fs` 的原因
是标准库为了跨平台一致性牺牲了每个平台上最快的那条路。

### macOS

`copyfile(3)` 加 `COPYFILE_CLONE`：内核在可能时自动走 `clonefile(2)` 的 APFS
写时复制（瞬时完成、不占额外空间），做不到时无缝回退普通复制。**无条件带上
这个标志没有任何损失**，是一条零成本的快路径。

`COPYFILE_ALL` 一次带走权限、时间戳、xattr、ACL、resource fork，不用逐项处理。

`renamex_np(RENAME_EXCL)` 让「目标已存在就失败」由内核保证。

### Linux

三级回退，每级比下一级快一个数量级：`FICLONE` ioctl（btrfs/XFS reflink，
瞬时）→ `copy_file_range(2)`（内核内复制，天然保留稀疏空洞）→ 大缓冲读写。

注意第三级之前必须 `set_len(0)` 并删掉目标文件重来——前两级可能已经写进去了
一部分。

`renameat2(RENAME_NOREPLACE)` 直接走 `syscall`，因为部分 libc 版本没有包装函数。

### Windows

沿用 rmbrr 已在生产验证过的手法，三处增强：

1. **`\\?\` 前缀**——所有路径一律加，绕过 MAX_PATH（260 字符）。rmbrr 的注释
   记录了不加会在深层 `node_modules` 里静默失败、导致父目录删除报
   `ERROR_DIR_NOT_EMPTY`。UNC 路径要写成 `\\?\UNC\server\share`。
2. **`FIND_FIRST_EX_LARGE_FETCH`**——rmbrr 传的是 `FIND_FIRST_EX_FLAGS(0)`，
   开这个标志能显著减少大目录的内核往返。配 `FindExInfoBasic` 跳过 8.3 短名查询。
3. **`COPY_FILE_NO_BUFFERING`**——大文件（≥16 MiB）绕过缓存管理器，既提升
   吞吐又避免把整个文件灌进 standby list 冲垮系统缓存。

删除走 `SetFileInformationByHandle(FileDispositionInfoEx)` 加
`FILE_DISPOSITION_POSIX_SEMANTICS`：目录项立即从命名空间消失，不必等最后一个
句柄关闭。**这是并行删除能跑起来的前提**——否则父目录会因为子项句柄还没释放
而卡在 `ERROR_DIR_NOT_EMPTY`。

#### junction 是手工构造的

Win32 没有创建 junction 的 API。`platform::windows::set_mount_point` 手工拼了
一个 `REPARSE_DATA_BUFFER`：

```
偏移  大小  字段
0     4     ReparseTag = IO_REPARSE_TAG_MOUNT_POINT
4     2     ReparseDataLength（不含前 8 字节）
6     2     Reserved
8     2     SubstituteNameOffset
10    2     SubstituteNameLength
12    2     PrintNameOffset
14    2     PrintNameLength
16    ...   PathBuffer: SubstituteName\0 PrintName\0
```

`SubstituteName` 用 NT 命名空间形式 `\??\C:\path`，`PrintName` 用普通形式
`C:\path`，两者都**不带** `\\?\` 前缀。

用字节数组而不是 `repr(C)` 结构体，因为末尾的 `PathBuffer` 是变长的，Rust
表达不了。

这段代码只有真机能验证——类型检查通不过不了「系统到底接不接受这个 buffer」。
CI 里有一个专门的 `windows-junction` job 跑 `fsutil reparsepoint query` 确认
tag 是 Mount Point。

---

## 5. 几个容易写错的语义

### reparse point ≠ 链接（Windows）

`FILE_ATTRIBUTE_REPARSE_POINT` 只说明「这个项上挂了一个 reparse point」，
不代表它是链接。OneDrive 占位目录、AppExecLink、重复数据删除的文件全都带这个
属性。

必须按 **reparse tag 做白名单**：只有 `IO_REPARSE_TAG_MOUNT_POINT` 和
`IO_REPARSE_TAG_SYMLINK` 算链接，其余按普通文件/目录复制。

漏了这条会静默毁掉用户的云盘目录——把占位符当链接删掉，或者当链接重建，
两种都是数据丢失。

### 树内的符号链接不能跟随

遇到链接要原样重建成链接，绝不递归进去。跟过去会把链接目标（可能在源树
之外、可能构成环）整个拷进来。`plan::scan` 把链接记进 `symlinks` 而不是
`dirs`，`copy::copy_tree` 用 `create_dir_symlink` 原样重建目标字符串。

### 硬链接必须去重

`.git/objects`、pnpm store、部分 `node_modules` 大量使用硬链接。不去重的话
一组 N 个硬链接会变成 N 份独立拷贝，磁盘占用和耗时都成倍增长。

`plan::hardlink_groups` 按 `(dev, inode)` 分组，**只保留组内成员数 > 1 的组**
——一个文件 `nlink > 1` 但它的其他链接在源树之外时，我们只能当普通文件复制。

组内第一个正常复制，其余的 `create_hard_link` 指过去。目标文件系统不支持
硬链接时（FAT32）退化为复制：结果仍然正确，只是多占空间。

### 台账的键必须归一

macOS 上 `/tmp` 是指向 `/private/tmp` 的符号链接，两种写法指同一个目录。
如果直接拿用户写的路径当台账键，同一个目录会出现两条记录，`restore` 也会
按用户的写法找不到。

`link::canonical_key` 解析**父目录**里的所有链接，但**保留最后一段**——
因为最后那一段可能正是我们要操作的那个链接，解析了就指到别处去了。

### 台账需要文件锁

早期版本的 `begin()` / `complete()` 各自做「读 → 改 → 写回」。两个并发的
`cshl` 会互相覆盖：后写的把先写的记录整个抹掉，然后 `complete()` 报「找不到
记录」。同时迁移多个目录是很自然的用法。

现在所有写操作都走 `ledger::with_lock`，用 `File::lock()`（Rust 1.89 稳定）。
锁加在单独的 `.lock` 文件上而不是 `ledger.json` 本身——后者会被原子替换
（写临时文件再 rename），文件一换，加在旧 inode 上的锁就形同虚设了。

`tests/integration.rs::concurrent_migrations_do_not_lose_ledger_entries`
锁住这个行为，去掉锁它会立刻失败。

---

## 6. 穿透重定向

`source` 本身已经是链接时（二次迁移场景），流程是：

```
real = canonicalize(source)        // 完全解析，链套链也能到底
校验 real 是目录、real ≠ target、target 不在 real 内部
把 real 迁移到 target
把 source 改指到 target
```

终态是 `source → target` **直接指向**，而不是 `source → real → target` 的
链条——`real` 原位置迁走后不留链接。

Unix 上改指是原子的：在同级建一个临时符号链接，再 `rename` 覆盖过去，POSIX
保证这一步原子，别的进程不会看到「链接不存在」的中间态。Windows 上 junction
没有原子替换手段，只能先删后建，窗口极短但确实存在（`link::repoint_dir_link`
在建新链接失败时会尽力把旧链接恢复回去）。

台账记下 `original_real_path`，`restore` 据此把数据搬回 `real` 的位置，再把
`source` 指回去——而不是搬到 `source` 处。

---

## 7. 两类不同的拒绝

`safety.rs` 管的是「危险但你可能知道自己在干嘛」，分两级：

- `Verdict::Risky` —— 主目录、包含 cwd 的目录，`--force` 可越过
- `Verdict::Forbidden` —— 卷根、系统关键目录，`--force` **也不行**

`migrate::validate_paths` 管的是另一类：自吞、源目标相同、目标非空。这些不是
「危险」，是**纯粹的逻辑错误**，无论如何都会导致数据损坏，所以根本没有 force
选项。目标的父目录不存在**不**属于这一类 —— 那只是还没建，`migrate::create_parents`
会在真正动手前逐级补齐（预演模式下不建）。

`Error::Unsafe` 带 `can_force` 字段就是为了让提示语说对话——对一个
`Forbidden` 的路径提示「加 --force 试试」会把用户带进沟里。

---

## 8. 测试策略

- **单元测试**（72 项）跟着模块走，测的是单个函数的契约
- **集成测试**（18 项，`tests/integration.rs`）跑真实的 `cshl` 二进制，测
  跨模块的组合行为和**失败后的状态**——「复制失败后源目录是否原封不动」
  这种事，只有把整条链路跑一遍才算数
- 台账通过 `CSHELL_HOME` 环境变量隔离到临时目录，绝不碰用户真实的 `~/.cshell`
- 跨卷用例在找不到第二个卷时自动跳过（`cross_volume_target` 返回 `None`），
  不会因为开发机没挂盘就红

本地能验证的和不能验证的：

```bash
cargo test                                        # 三平台通用
cargo xwin check --target x86_64-pc-windows-msvc  # 在 mac 上类型检查 Windows 代码
```

`cargo xwin check` 能抓住绝大多数 `windows` crate 的 API 签名错误，但抓不住
运行时行为——手工构造的 reparse buffer 到底能不能被系统接受，只有 CI 的
`windows-junction` job 说了算。

---

## 9. 明确不做的

- **ownership (uid/gid) 与 ACL**：macOS 的 `COPYFILE_ALL` 会顺带保留，
  Linux/Windows 上不额外处理。要做的话得处理「非 root 时 `fchown` 必然失败」
  以及 Windows 上 `GetNamedSecurityInfo`/`SetNamedSecurityInfo` 的一整套
  安全描述符复制，代价和收益不匹配。
- **数据校验**：不做哈希比对。每个系统调用的返回值都检查，任何一个失败都会
  中止并保留源目录，这已经能挡住绝大多数事故；再读一遍全部数据做比对会让
  大目录的耗时翻倍。
- **断点续传**：崩溃后残留的 `partial` 目录直接删掉重来，不尝试增量恢复。
  台账里的 `inflight` 记录负责告诉用户发生了什么。
