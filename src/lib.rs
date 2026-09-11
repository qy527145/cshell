//! cshell —— 极速跨平台文件夹迁移工具。
//!
//! 核心语义：`cshl <source> <target>` 把整个文件夹搬到 target，并在 source 原地
//! 留下一个链接（Windows 用 junction，Unix 用 symbolic link），使得一切引用
//! source 路径的程序照常工作。典型场景是把系统盘上的 `node_modules` / `AppData`
//! / 模型缓存搬到大容量盘。
//!
//! 迁移分两条路径（详见 [`migrate`]）：
//! - **同卷** —— 单次原子 `rename`，微秒级，零数据搬运。
//! - **跨卷** —— 并行复制（各平台零拷贝/内核内复制）→ 原子换名 → 并行删除 → 建链。
//!
//! 各平台原生 API 封装在 [`platform`]，对上暴露统一签名。

pub mod cli;
pub mod error;
pub mod ledger;
pub mod link;
pub mod migrate;
pub mod plan;
pub mod platform;
pub mod safety;
pub mod volume;

pub mod copy;
pub mod remove;

pub use error::{Error, Result};
