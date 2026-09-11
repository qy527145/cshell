//! CLI 定义与分发。
//!
//! 采用 clap 的「默认子命令」模式：`cshl <SRC> <DST>` 与
//! `cshl move <SRC> <DST>` 等价，前者是日常用法，后者用于消歧。

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::error::Error;

#[derive(Parser, Debug)]
#[command(name = "cshl")]
#[command(version)]
#[command(about = "把文件夹搬走，原地留一个链接")]
#[command(
    long_about = "cshell 把整个文件夹迁移到新位置，并在原位置留下一个链接（Windows: junction，\
Unix: symbolic link），让所有引用旧路径的程序继续正常工作。\n\n\
同卷迁移走单次原子 rename（微秒级，无论目录多大）；跨卷迁移走并行复制 + 并行删除，\
全程使用各平台原生 API。"
)]
#[command(after_help = "示例:\n  \
  cshl ./node_modules /data/nm            把目录搬到 /data/nm，原地留链接\n  \
  cshl -n ./big /mnt/disk2/big            预演：只报告不动手\n  \
  cshl --no-link ./cache /mnt/d/cache     只搬不留链接\n  \
  cshl list                                查看所有已迁移的目录\n  \
  cshl restore ./node_modules              搬回原位并删除链接")]
#[command(args_conflicts_with_subcommands = true)]
// 裸 `cshl` 不带任何参数时直接打印帮助。
// 交给 clap 处理而不是自己判断：默认子命令那组位置参数是「要么都给、
// 要么都不给」，clap 的可选 flatten 组表达不了「整组省略」这种情况。
#[command(arg_required_else_help = true)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,

    /// 默认子命令（等价于 `cshl move`）的参数
    #[command(flatten)]
    pub move_args: Option<MoveArgs>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// 迁移文件夹并在原地留下链接
    #[command(visible_alias = "mv")]
    Move(MoveArgs),
    /// 列出台账里所有已迁移的目录
    #[command(visible_alias = "ls")]
    List {
        /// 输出 JSON 而非表格
        #[arg(long)]
        json: bool,
    },
    /// 把已迁移的目录搬回原位并删除链接
    Restore(RestoreArgs),
}

#[derive(Args, Debug, Clone)]
pub struct MoveArgs {
    /// 要搬走的源文件夹
    pub source: PathBuf,

    /// 目标位置（必须不存在，或是一个空目录）
    pub target: PathBuf,

    /// 工作线程数（默认：复制 = 逻辑核数×4 上限 32，删除 = 逻辑核数）
    #[arg(short = 't', long)]
    pub threads: Option<usize>,

    /// 预演：扫描并报告将要发生什么，但不做任何改动
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// 输出每一步的细节
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// 不显示进度条
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// 越过安全检查（系统目录等），请谨慎使用
    #[arg(short = 'f', long)]
    pub force: bool,

    /// 只迁移，不在原地留下链接
    #[arg(long)]
    pub no_link: bool,

    /// 原地链接的类型（仅 Windows 有效；Unix 恒为 symlink）
    #[arg(long, value_enum, default_value_t = LinkType::Junction)]
    pub link_type: LinkType,

    /// 当 source 本身已是链接时报错退出，而不是穿透到真实目录
    #[arg(long, action = ArgAction::SetTrue)]
    pub no_follow_link: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RestoreArgs {
    /// 之前被迁移的源路径（即现在那个链接所在的位置）
    pub source: PathBuf,

    /// 工作线程数
    #[arg(short = 't', long)]
    pub threads: Option<usize>,

    /// 预演
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    #[arg(short = 'v', long)]
    pub verbose: bool,

    #[arg(short = 'q', long)]
    pub quiet: bool,
}

/// 原地链接的类型。Unix 上只有 symlink 可选 —— POSIX 禁止对目录建硬链接。
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    /// Windows 目录联接：普通用户即可创建，无需管理员权限或开发者模式
    Junction,
    /// 符号链接：Unix 上的唯一选择；Windows 上需要管理员权限或开发者模式
    Symlink,
}

/// 进程入口：解析、分发、渲染错误，返回退出码。
pub fn run() -> i32 {
    let cli = Cli::parse();

    let result = match cli.cmd {
        Some(Cmd::Move(args)) => crate::migrate::run_move(&args),
        Some(Cmd::List { json }) => crate::ledger::run_list(json),
        Some(Cmd::Restore(args)) => crate::migrate::run_restore(&args),
        None => match cli.move_args {
            Some(args) => crate::migrate::run_move(&args),
            None => {
                // 无参数：打印帮助而不是报一个含糊的错误
                use clap::CommandFactory;
                Cli::command().print_help().ok();
                println!();
                return 1;
            }
        },
    };

    match result {
        Ok(()) => 0,
        Err(e) => {
            render_error(&e);
            e.exit_code()
        }
    }
}

/// 把错误渲染到 stderr。做了一半的错误要额外给出补救指引。
fn render_error(e: &Error) {
    eprintln!("cshl: {}", e);

    match e {
        Error::Aborted { failures, .. } => {
            eprintln!("\n源目录未被改动，修复下列问题后可直接重试。");
            print_failures(failures);
        }
        Error::PartialFailure { failures, .. } => {
            eprintln!(
                "\n⚠️  数据已经全部就位于目标位置，但源目录没有清理干净。\n\
                 请手动检查残留，然后删除它。`cshl list` 可查看本次迁移的记录。"
            );
            print_failures(failures);
        }
        Error::Unsafe { can_force, .. } => {
            if *can_force {
                eprintln!("\n确认无误可加 --force 越过检查。");
            } else {
                eprintln!("\n这条限制无法越过 —— 加 --force 也不行。");
            }
        }
        _ => {}
    }
}

fn print_failures(failures: &[crate::error::FailedItem]) {
    if failures.is_empty() {
        return;
    }
    let show = failures.len().min(10);
    eprintln!("\n前 {} 项失败:", show);
    for (i, f) in failures.iter().take(show).enumerate() {
        let kind = if f.is_dir { "目录" } else { "文件" };
        eprintln!("  {}. [{}] {}: {}", i + 1, kind, f.path.display(), f.error);
    }
    if failures.len() > show {
        eprintln!("  ... 另有 {} 项失败", failures.len() - show);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_two_positionals_parse_as_move() {
        let cli = Cli::try_parse_from(["cshl", "/a", "/b"]).unwrap();
        assert!(cli.cmd.is_none());
        let args = cli.move_args.expect("应解析出默认 move 参数");
        assert_eq!(args.source, PathBuf::from("/a"));
        assert_eq!(args.target, PathBuf::from("/b"));
        // 默认值
        assert!(!args.dry_run);
        assert!(!args.no_link);
        assert_eq!(args.link_type, LinkType::Junction);
    }

    #[test]
    fn explicit_move_subcommand_parses() {
        let cli = Cli::try_parse_from(["cshl", "move", "/a", "/b", "-n"]).unwrap();
        match cli.cmd {
            Some(Cmd::Move(args)) => {
                assert_eq!(args.source, PathBuf::from("/a"));
                assert!(args.dry_run);
            }
            other => panic!("期望 Move，实际 {:?}", other),
        }
    }

    #[test]
    fn list_and_restore_parse() {
        assert!(matches!(
            Cli::try_parse_from(["cshl", "list"]).unwrap().cmd,
            Some(Cmd::List { json: false })
        ));
        assert!(matches!(
            Cli::try_parse_from(["cshl", "ls", "--json"]).unwrap().cmd,
            Some(Cmd::List { json: true })
        ));
        match Cli::try_parse_from(["cshl", "restore", "/a"]).unwrap().cmd {
            Some(Cmd::Restore(a)) => assert_eq!(a.source, PathBuf::from("/a")),
            other => panic!("期望 Restore，实际 {:?}", other),
        }
    }

    #[test]
    fn no_args_prints_help_instead_of_a_cryptic_error() {
        let err = Cli::try_parse_from(["cshl"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
            "裸 cshl 应当打印帮助，而不是报「缺少参数」"
        );
    }

    #[test]
    fn incomplete_move_args_are_rejected_by_clap() {
        // 只给了源没给目标：必须在解析期就被挡下，不能留到运行时
        let err = Cli::try_parse_from(["cshl", "/only-source"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn link_type_accepts_both_values() {
        let cli = Cli::try_parse_from(["cshl", "/a", "/b", "--link-type", "symlink"]).unwrap();
        assert_eq!(cli.move_args.unwrap().link_type, LinkType::Symlink);
    }
}
