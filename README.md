# cshell

**把文件夹搬走，原地留一个链接。**

C 盘满了、系统盘吃紧，但 `node_modules`、`AppData`、模型缓存、`~/Library` 这些目录又不能随便删、也不能随便挪——挪走了程序就找不到路径。

`cshl` 把目录搬到大容量盘，并在原位置留下一个链接（Windows 用 junction，macOS/Linux 用符号链接），所有引用旧路径的程序照常工作。

```bash
cshl ~/Library/Caches/huge-model /Volumes/Data/model-cache
# ✓ 已迁移到 /Volumes/Data/model-cache（12.4 GiB · 8213 文件 · 402 目录，耗时 6.21s）
#   ~/Library/Caches/huge-model → /Volumes/Data/model-cache （symlink）
```

## 安装

```bash
cargo install --git https://github.com/qy527145/cshell
```

## 用法

```bash
cshl <SOURCE> <TARGET>     # 迁移，原地留链接
cshl list                  # 查看所有已迁移的目录
cshl restore <SOURCE>      # 搬回原位并删除链接
```

常用参数：

| 参数 | 说明 |
|---|---|
| `-n, --dry-run` | 预演：扫描并报告将要发生什么，不做任何改动 |
| `-v, --verbose` | 输出每一步的细节与耗时 |
| `-t, --threads <N>` | 工作线程数（默认按本机逻辑核数算出，`cshl --help` 会印出实际数字；机械硬盘上建议 `-t 1` 以免寻道抖动） |
| `--no-link` | 只搬走，不留链接 |
| `--no-follow-link` | source 本身已是链接时报错退出，而不是穿透 |
| `--link-type <junction\|symlink>` | 仅 Windows 有效，默认 junction |
| `-f, --force` | 越过「危险但你可能知道自己在干嘛」的安全检查 |

## 为什么快

**同卷迁移是一次 `rename`。** 无论目录里有 1 个还是 1000 万个文件，都是一次元数据操作，微秒级完成，零数据搬运。绝大多数「把 D 盘的目录挪到 D 盘另一个位置」都走这条路。

```
$ cshl -v node_modules /data/nm      # 3200 文件 / 601 目录 / 13 MB
同卷迁移：一次原子 rename 完成
✓ 已迁移到 /data/nm（同卷 rename，耗时 804.58µs）
```

判定方式是**先试后判**：直接发起 rename，返回跨设备错误才走复制。比预先探测卷号可靠——同卷跨挂载点、APFS firmlink 这类边界情况，预测容易错，实测不会。

**跨卷迁移用各平台最快的原生 API，并且全程并行。**

| 平台 | 复制 | 删除 |
|---|---|---|
| macOS | `copyfile(3)` + `COPYFILE_CLONE`（APFS 写时复制，可用时瞬时完成） | `unlink`/`rmdir` |
| Linux | `FICLONE` reflink → `copy_file_range` 内核内复制 → 大缓冲读写，三级回退 | 同上 |
| Windows | `CopyFile2`，大文件加 `COPY_FILE_NO_BUFFERING` 绕过缓存管理器 | `FILE_DISPOSITION_POSIX_SEMANTICS`，立即从命名空间摘除 |

复制阶段**先建完整目录骨架，再并行复制文件**——骨架建好后文件之间再无依赖，可以完全并行派发。文件按大小降序调度（LPT），避免一个 8 GB 的文件在所有小文件干完之后独自拖尾。

删除阶段的依赖方向正好相反（目录必须先清空才能删），所以用的是另一套调度：维护一张依赖图，每个目录记着还有多少子目录没删完，归零时它自己变成新的叶子进队列。

## 安全性

**复制失败时，源目录原封不动。** 数据先复制到目标卷上的临时目录，全部成功后才用一次原子 rename 落位，然后才删源。任何一个文件复制失败都会中止整个迁移、清掉临时目录、退出——源目录从头到尾没被碰过，修好问题直接重跑即可。

```
$ cshl tree /Volumes/Data/dst
cshl: 迁移已中止，源目录未被改动: /tmp/tree/a/locked.bin: Permission denied (os error 13)（1 项失败）

源目录未被改动，修复下列问题后可直接重试。

前 1 项失败:
  1. [文件] /tmp/tree/a/locked.bin: Permission denied (os error 13)
```

阶段顺序是按「任何时刻崩溃都可恢复」设计的：

```
0. 台账写入 inflight 记录
1. 扫描源目录
2. 并行复制到 <target>.cshl-partial-<pid>   ← 崩在这里：源完好，删掉临时目录即可
3. rename(partial → target)                 ← 同卷原子，瞬时。此后数据已完整就位
4. 并行删除源目录
5. 原地建链接
6. 台账标记 done
```

唯一不可逆的窗口是第 4 步结束到第 5 步完成之间（源已删、链接未建），只有亚毫秒级，且台账已记下目标位置。中途崩溃留下的 `inflight` 记录会被 `cshl list` 标出来。

其他把关：

- **空间预检**——跨卷前先比对总字节与目标卷可用空间，不足直接拒绝，而不是复制到一半才发现写不下
- **拒绝自吞**——目标在源内部、源在目标内部、两者相同，一律拒绝（`--force` 也不行）
- **拒绝覆盖**——目标已存在且非空时拒绝；「不覆盖」由内核保证（`RENAME_NOREPLACE` / `RENAME_EXCL` / 不传 `MOVEFILE_REPLACE_EXISTING`），而不是先 `exists()` 再 rename，消除 TOCTOU 竞争
- **系统目录保护**——卷根、`/usr`、`C:\Windows` 这类目录禁止迁移，`--force` 也不行
- **自动补齐目标父目录**——目标的父路径不存在时逐级创建；迁移失败会把自己新建的那几层空目录撤回去，打错路径不会在磁盘上留下空壳（`--dry-run` 只报告、不创建）
- **台账文件锁**——并发的多个 `cshl` 不会互相覆盖记录

## 正确处理的边界情况

这几条决定了它能不能真的用在 `node_modules`、`.git`、`AppData` 上：

**Windows 上 reparse point ≠ 链接。** OneDrive 占位目录、AppExecLink、重复数据删除的文件都带 `FILE_ATTRIBUTE_REPARSE_POINT`，但它们不是链接。`cshl` 按 reparse tag 做白名单，只认 `IO_REPARSE_TAG_MOUNT_POINT` 和 `IO_REPARSE_TAG_SYMLINK`，其余按普通文件/目录复制——否则会静默毁掉用户的云盘目录。

**树内的符号链接原样重建，绝不跟随。** 跟过去会把链接目标（可能在源树之外、可能构成环）整个拷进来。

**树内的硬链接组会被还原。** `.git/objects`、pnpm store 大量使用硬链接，不去重的话磁盘占用和耗时都会成倍增长。`cshl` 记录 `(dev, inode)`，组内第一个正常复制、其余建硬链接指过去。目标文件系统不支持硬链接时（如 FAT32）退化为复制，结果仍然正确。

**source 本身已是链接时会穿透重定向。** 二次迁移（D→E 之后再 E→F）时，原链接自动跟进指向最新位置，不会形成 `a→b→c` 的链条：

```bash
cshl ~/cache /Volumes/D/cache     # ~/cache 现在是链接 → /Volumes/D/cache
cshl ~/cache /Volumes/E/cache     # 穿透：搬的是真实数据，~/cache 直接改指 /Volumes/E/cache
```

Unix 上这个改指是原子的（建临时链接 + `rename` 覆盖）。`restore` 会把数据搬回原链接当初指向的真实位置，而不是链接所在处。

**长路径。** Windows 上所有路径加 `\\?\` 前缀绕过 MAX_PATH（260 字符）限制——不加会在深层 `node_modules` 里静默失败。

## 已知边界

- **元数据保留范围**：权限位、mtime/atime、xattr。macOS 上 `COPYFILE_ALL` 会顺带保留 ACL 与 resource fork；Linux/Windows 上 **ownership (uid/gid) 与 ACL 不在当前范围内**。
- **Windows 符号链接**需要管理员权限或开启开发者模式，所以默认用 junction。junction 不支持 UNC 目标，遇到时自动回退符号链接。
- **Unix 上不支持硬链接方案**——POSIX 禁止对目录建硬链接（`link()` 返回 `EPERM`），只能用符号链接。

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 参数/校验/安全检查问题——改一下再来 |
| 2 | I/O 故障，但**源目录未被改动**——修好问题可直接重试 |
| 3 | 做了一半：数据已在目标位置，但源目录没清干净，需人工介入 |

## 开发

```bash
cargo test                                              # 单元 + 集成测试
cargo clippy --all-targets
cargo xwin check --target x86_64-pc-windows-msvc        # 在 macOS/Linux 上检查 Windows 代码路径
```

跨卷测试需要第二个卷，没有就自动跳过。macOS 上可以造一个：

```bash
hdiutil create -size 300m -fs APFS -volname cshltest /tmp/cshltest.dmg
hdiutil attach /tmp/cshltest.dmg        # → /Volumes/cshltest
cargo test
hdiutil detach /Volumes/cshltest
```

## License

MIT
