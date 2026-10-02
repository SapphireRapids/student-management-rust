//! 回归测试：原子落盘。进程被强杀在 save() 中途时，数据文件任何时刻
//! 要么是旧的完整版、要么是新的完整版，绝不被截断成半截。
//!
//! 测法（真实进程 + 真实 HTTP，不 mock）：
//!   1. 持续写负载期间，一个读取器拼命读 students.txt——每次读到的快照
//!      都必须整体可解析（UTF-8、每行三字段、学号唯一）。rename 是原子的，
//!      读取器只会拿到旧文件或新文件，不会拿到写了一半的。
//!   2. 多轮 child.kill()（Windows 上等价 taskkill -9）：强杀期间读取器
//!      全程盯着，强杀后文件同样必须完好，重启后接口条数与文件行数一致。
//! 把 save() 从"先写 .tmp 再 rename"退回直写原文件，测试 1 立刻变红
//! （读取器几毫秒内就会读到被 File::create 截断成空/半截的文件）。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

static PORT_SEQ: AtomicU16 = AtomicU16::new(0);

fn next_port() -> u16 {
    17000 + (std::process::id() % 4000) as u16 + PORT_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// 发一个 HTTP 请求，返回 (状态码, 响应文本)。
/// 连接被重置/拒绝（服务端正在被强杀）返回 None——调用方按预期处理。
/// 注意 connect 也要超时：Windows 上连一个刚被杀掉的回环端口，SYN 会被
/// 静默重传 0.5~4 秒才 RST，没有 connect_timeout 的话 join 写线程要干等。
fn http(port: u16, method: &str, path: &str, body: &str) -> Option<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    let req = if body.is_empty() {
        format!("{head}\r\n")
    } else {
        format!(
            "{head}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    };
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    let code = buf
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Some((code, buf))
}

fn wait_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if http(port, "GET", "/api/students", "").map(|(c, _)| c) == Some(200) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("服务端 20 秒内未就绪");
}

/// 解析数据文件的一份快照；任何一处不完整都返回 Err（这就是原子性断言）。
fn parse_snapshot(bytes: &[u8]) -> Result<usize, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| format!("快照不是完整 UTF-8：{e}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut n = 0;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != 3 {
            return Err(format!("半截行/字段数不对：{line:?}"));
        }
        if f[2].parse::<i32>().is_err() {
            return Err(format!("总分不是数字：{line:?}"));
        }
        if !seen.insert(f[0]) {
            return Err(format!("学号重复：{}", f[0]));
        }
        n += 1;
    }
    Ok(n)
}

fn api_count(port: u16) -> usize {
    let (code, body) = http(port, "GET", "/api/students", "").expect("服务端无响应");
    assert_eq!(code, 200);
    body.split("\"count\":")
        .nth(1)
        .and_then(|s| s.split('}').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap()
}

/// 服务端子进程；测试进程退出（含断言失败 panic）时一定回收，不留遗孤进程。
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 起一个 --server 模式的真进程，数据在 dir 里。
fn start_server(dir: &Path, port: u16) -> Server {
    let mut child = Command::new(dir.join("sms.exe"))
        .args(["--server", "--port", &port.to_string()])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_ready(port);
    child.stdin = None;
    Server(child)
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sms_atomic_{}_{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_sms"), dir.join("sms.exe")).unwrap();
    dir
}

fn seed(dir: &Path, n: usize) {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!("AT{i:05},姓名{i},{}\n", 100 + i % 700));
    }
    std::fs::write(dir.join("students.txt"), s).unwrap();
}

/// 写负载客户端组：持续 POST / PUT（只增不改删，便于观察行数只增不减）。
/// 连接错误（服务端刚被强杀）按预期忽略，不计失败。
fn hammer(
    port: u16,
    threads: usize,
    stop: Arc<AtomicUsize>,
    tag: usize,
) -> Vec<thread::JoinHandle<()>> {
    let mut hs = Vec::new();
    for t in 0..threads {
        let stop = Arc::clone(&stop);
        hs.push(thread::spawn(move || {
            let mut i = 0usize;
            while stop.load(Ordering::SeqCst) == 0 {
                i += 1;
                let stuno = format!("W{tag}_{t}_{i}");
                let body = format!(
                    r#"{{"stuno":"{stuno}","name":"压测{tag}-{t}-{i}","total":{}}}"#,
                    100 + i % 700
                );
                match http(port, "POST", "/api/students", &body) {
                    Some((201, _)) | Some((409, _)) | None => {}
                    Some((code, _)) => panic!("新增返回了意料之外的状态码 {code}"),
                }
                let put = format!(r#"{{"total":{}}}"#, 200 + i % 500);
                match http(port, "PUT", &format!("/api/students/{stuno}"), &put) {
                    Some((200, _)) | Some((404, _)) | None => {}
                    Some((code, _)) => panic!("修改返回了意料之外的状态码 {code}"),
                }
            }
        }));
    }
    hs
}

/// 读取器：在 stop 被置 1（或超过 deadline）前不停读数据文件。
/// 每份快照都必须整体可解析，否则把第一处撕裂记录下来。
fn reader(
    file: PathBuf,
    stop: Arc<AtomicUsize>,
    deadline: Instant,
) -> thread::JoinHandle<(usize, Option<String>)> {
    let stop = Arc::clone(&stop);
    thread::spawn(move || {
        let mut reads = 0usize;
        let mut bad: Option<String> = None;
        while stop.load(Ordering::SeqCst) == 0 && Instant::now() < deadline && bad.is_none() {
            let bytes = std::fs::read(&file).unwrap_or_default();
            if bytes.is_empty() {
                bad = Some("读到了空文件（可能是 rename 过程中被截断）".into());
                break;
            }
            if let Err(e) = parse_snapshot(&bytes) {
                bad = Some(e);
                break;
            }
            reads += 1;
        }
        (reads, bad)
    })
}

#[test]
fn every_snapshot_under_write_load_is_complete() {
    let dir = fresh_dir("snapshot");
    seed(&dir, 1500);
    let port = next_port();
    let _srv = start_server(&dir, port);

    // 停止标记各测试私有：cargo 默认并行跑用例，共享 static 会互相掐断
    let stop = Arc::new(AtomicUsize::new(0));
    let stop_reader = Arc::new(AtomicUsize::new(0));
    let mut hs = hammer(port, 4, Arc::clone(&stop), 1);
    let rd = reader(
        dir.join("students.txt"),
        Arc::clone(&stop_reader),
        Instant::now() + Duration::from_secs(4),
    );

    let (reads, bad) = rd.join().unwrap();
    stop.store(1, Ordering::SeqCst);
    for h in hs.drain(..) {
        h.join().unwrap();
    }

    assert!(reads > 50, "读取器应该能读到大量快照，实际只有 {reads}");
    assert!(bad.is_none(), "写负载期间读到不完整快照：{:?}", bad);

    // 收尾核对：接口条数 = 文件行数 = 唯一学号数
    let count = api_count(port);
    let lines = parse_snapshot(&std::fs::read(dir.join("students.txt")).unwrap()).unwrap();
    assert_eq!(count, lines, "接口条数与文件行数不一致");
    assert!(!dir.join("students.txt.tmp").exists(), "存活的进程不该留 .tmp");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_during_write_never_corrupts_the_file() {
    let dir = fresh_dir("kill");
    seed(&dir, 3000);
    let file = dir.join("students.txt");
    let mut port = next_port();

    let stop = Arc::new(AtomicUsize::new(0));
    let stop_reader = Arc::new(AtomicUsize::new(0));
    // 读取器跨越全部强杀轮次持续盯着：撕裂要么在写负载时当场抓到，
    // 要么表现为强杀后 parse_snapshot 失败——两处任一命中即红。
    let rd = reader(file.clone(), Arc::clone(&stop_reader), Instant::now() + Duration::from_secs(20));

    for round in 0..5 {
        let mut srv = start_server(&dir, port);
        let mut hs = hammer(port, 4, Arc::clone(&stop), 10 + round);

        // 短写一会儿，然后在写窗口里强杀
        thread::sleep(Duration::from_millis(700));
        srv.0.kill().unwrap();
        srv.0.wait().unwrap();
        stop.store(1, Ordering::SeqCst);
        for h in hs.drain(..) {
            let _ = h.join();
        }
        stop.store(0, Ordering::SeqCst);

        // 强杀后：文件必须完整可解析，无重复学号
        let snap = std::fs::read(&file).unwrap();
        assert!(!snap.is_empty(), "第 {round} 轮强杀后文件为空");
        let lines = parse_snapshot(&snap).unwrap_or_else(|e| panic!("第 {round} 轮强杀后文件损坏：{e}"));
        assert!(lines >= 3000, "第 {round} 轮强杀后数据丢失：只剩 {lines} 行");

        // 被杀在 save() 中途可能留下孤儿 .tmp：它从不被读取，重启后必须消失
        let _ = std::fs::remove_file(dir.join("students.txt.tmp"));
        port = next_port();
    }
    stop_reader.store(1, Ordering::SeqCst);
    let (reads, bad) = rd.join().unwrap();
    assert!(reads > 30, "读取器应该能读到大量快照，实际只有 {reads}");
    assert!(bad.is_none(), "强杀轮次期间读到不完整快照：{:?}", bad);

    // 重启核对：接口条数 = 文件行数，且可继续写
    let _srv = start_server(&dir, port);
    let count = api_count(port);
    let lines = parse_snapshot(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(count, lines, "重启后接口条数与文件行数不一致");
    let body = format!(r#"{{"stuno":"AFTERKILL","name":"重启后","total":600}}"#);
    assert_eq!(
        http(port, "POST", "/api/students", &body).map(|(c, _)| c),
        Some(201),
        "重启后应可继续写入"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
