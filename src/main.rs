//! 学生成绩管理系统（Rust 版）v1.4.1
//!
//! 控制台菜单 + 内置 HTTP 服务，网页前端共用同一份数据：
//!   1、录入 2、显示 3、删除 4、修改 5、查找 6、排序 0、退出
//!   网页端: http://127.0.0.1:4399/ （仅监听本机）
//!
//! 用法:
//!   sms.exe               控制台菜单 + HTTP 服务（默认端口 4399）
//!   sms.exe --server      仅启动 HTTP 服务（供网页前端使用）
//!   sms.exe --port 9000   指定端口
//!   sms.exe --threads 2   HTTP 工作线程数（默认 1，最多 2，范围 1~2）
//!   sms.exe --help        显示帮助
//!
//! 数据文件: students.txt（UTF-8，每行 学号,姓名,总分；先写 .tmp 再 rename 原子替换，
//!          断电/强杀不会截断）；旧版五字段格式读入时自动按三科之和转成总分。
//! 前端页面: index.html（与程序同目录）。

mod console;
mod http;
mod json;
mod store;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use http::Ctx;
use store::Store;

const DEFAULT_PORT: u32 = 4399;
pub const TOTAL_MIN: i32 = 0; // 总分下限
pub const TOTAL_MAX: i32 = 1_000_000; // 总分上限
const HTTP_THREADS_MIN: usize = 1;
const HTTP_THREADS_MAX: usize = 2;

struct Opts {
    server_only: bool,
    port: u32,
    threads: usize,
}

fn print_usage() {
    println!(
        "学生成绩管理系统  v1.4.1\n\
         用法:\n\
        \x20 sms.exe               控制台菜单 + HTTP 服务(默认端口 {})\n\
        \x20 sms.exe --server      仅启动 HTTP 服务(供网页前端使用)\n\
        \x20 sms.exe --port N      指定端口\n\
        \x20 sms.exe --threads N   HTTP 工作线程数，默认 1，范围 {}~{}\n\
        \x20 sms.exe --help        显示帮助",
        DEFAULT_PORT, HTTP_THREADS_MIN, HTTP_THREADS_MAX
    );
}

fn parse_args(argv: &[String]) -> Result<Opts, String> {
    let mut opts = Opts {
        server_only: false,
        port: DEFAULT_PORT,
        threads: 1,
    };
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--server" {
            opts.server_only = true;
        } else if a == "--port" {
            i += 1;
            match argv.get(i).and_then(|v| v.parse::<u32>().ok()) {
                Some(p) => opts.port = p,
                None => return Err(format!("端口无效: {}", argv.get(i).cloned().unwrap_or_default())),
            }
        } else if let Some(v) = a.strip_prefix("--port=") {
            opts.port = v.parse().unwrap_or(0);
        } else if a == "--threads" {
            i += 1;
            match argv.get(i).and_then(|v| v.parse::<usize>().ok()) {
                Some(t) => opts.threads = t,
                None => return Err(format!("线程数无效: {}", argv.get(i).cloned().unwrap_or_default())),
            }
        } else if let Some(v) = a.strip_prefix("--threads=") {
            opts.threads = v.parse().unwrap_or(0);
        } else if a == "--help" || a == "-h" {
            print_usage();
            std::process::exit(0);
        } else {
            return Err(format!("未知参数: {}", a));
        }
        i += 1;
    }
    if opts.port < 1 || opts.port > 65535 {
        return Err(format!("端口无效: {}", opts.port));
    }
    if opts.threads < HTTP_THREADS_MIN || opts.threads > HTTP_THREADS_MAX {
        return Err(format!(
            "线程数无效: {}（允许 {}~{}）",
            opts.threads, HTTP_THREADS_MIN, HTTP_THREADS_MAX
        ));
    }
    Ok(opts)
}

/// Windows 控制台默认按 GBK 解释字节，中文输出/输入会乱码；
/// 切成 UTF-8 代码页（等价旧版的 SetConsoleOutputCP/SetConsoleCP）。
#[cfg(windows)]
fn set_console_utf8() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleOutputCP(codepage: u32) -> i32;
        fn SetConsoleCP(codepage: u32) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

#[cfg(not(windows))]
fn set_console_utf8() {}

fn main() {
    set_console_utf8();

    let argv: Vec<String> = std::env::args().collect();
    let opts = match parse_args(&argv) {
        Ok(o) => o,
        Err(e) => {
            println!("{}", e);
            print_usage();
            std::process::exit(1);
        }
    };

    // 数据文件与前端页面定位到 EXE 所在目录（从任何目录启动行为一致）
    let dir = store::exe_dir().unwrap_or_else(|| PathBuf::from("."));
    let data_file = dir.join("students.txt");
    let index_file = dir.join("index.html");

    // 只加载一次，控制台与 HTTP 服务通过 Arc 共用同一份数据
    let store: Arc<Mutex<Store>> = Arc::new(Mutex::new(Store::load(&data_file)));
    let ctx = Arc::new(Ctx {
        store: Arc::clone(&store),
        index_file,
    });

    let server = http::start(opts.port as u16, opts.threads, Arc::clone(&ctx));
    match &server {
        Some(_) => {
            println!(
                "\n[提示] HTTP 服务已启动: http://127.0.0.1:{}/ （仅监听本机，局域网内其他设备无法访问）",
                opts.port
            );
            println!("[提示] 控制台与网页共用同一份数据；关闭本窗口或输入 0 退出，网页服务同时停止。");
        }
        None => println!(
            "\n[提示] HTTP 服务启动失败（端口 {} 可能被占用），仅使用控制台功能。",
            opts.port
        ),
    }

    if opts.server_only {
        println!("[提示] 服务器模式运行中，关闭本窗口或按 Ctrl+C 退出。");
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    } else {
        console::run(&store);
    }

    if let Some(s) = &server {
        s.stop(); // 先停止接收新连接，再落盘
    }
    {
        let st = store.lock().unwrap_or_else(|e| e.into_inner());
        st.save();
    }
    println!("数据已保存。再见！");
}
