//! `cshl` —— cshell 的命令行入口。
//!
//! 极薄 main：解析参数、分发、把错误统一渲染并按 [`cshell::Error::exit_code`]
//! 退出。真正的逻辑都在 `cshell::cli`。

fn main() {
    let code = cshell::cli::run();
    std::process::exit(code);
}
