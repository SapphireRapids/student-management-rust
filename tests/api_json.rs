//! 回归测试：走真实 HTTP 接口验证 v1.4.1 的 JSON / 长度校验修复。
//!
//! 覆盖：
//! - 键名出现在字符串值里时仍能取到真正的键（旧版直接返回 None → 400）；
//! - 贴在字符串末尾的 `\uD83D\uDE00` 代理对能合成一个字符（旧版差一，变成两个 U+FFFD）；
//! - 学号/姓名上限按字符数（旧版按字节，40 个汉字的姓名会被误判非法）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static PORT_SEQ: AtomicU16 = AtomicU16::new(0);

fn next_port() -> u16 {
    30000 + (std::process::id() % 15000) as u16 + PORT_SEQ.fetch_add(1, Ordering::SeqCst)
}

fn http(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("连接服务端失败");
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
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    let code = buf
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (code, buf)
}

struct Server {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl Server {
    fn start(tag: &str, seed: &str) -> Server {
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "sms_api_{}_{}_{}",
            tag,
            std::process::id(),
            next_port()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = next_port();
        std::fs::write(dir.join("students.txt"), seed).unwrap();
        std::fs::copy(env!("CARGO_BIN_EXE_sms"), dir.join("sms.exe")).unwrap();
        // 首页由 sms.exe 从自己所在目录提供，前端页面也得在这儿
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("index.html"),
            dir.join("index.html"),
        )
        .unwrap();
        let child = Command::new(dir.join("sms.exe"))
            .args(["--server", "--port", &port.to_string()])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let s = Server { child, dir, port };
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if http(port, "GET", "/api/students", "").0 == 200 {
                return s;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("服务端 20 秒内未就绪");
    }

    fn get(&self, path: &str) -> (u16, String) {
        http(self.port, "GET", path, "")
    }
    fn post(&self, body: &str) -> (u16, String) {
        http(self.port, "POST", "/api/students", body)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn post_with_key_name_inside_string_value() {
    let s = Server::start("key_in_value", "SEED0001,甲,10\n");
    // name 的值里含有文本 "stuno":"FAKE1"——旧版遇到它就以为取到键并判定结构不符，返回 400
    let body = "{\"name\":\"\\\"stuno\\\":\\\"FAKE1\\\"\",\"stuno\":\"COL1\",\"total\":66}";
    let (code, text) = s.post(body);
    assert_eq!(code, 201, "应录入成功，实际 {} {}", code, text);
    let (code, text) = s.get("/api/students");
    assert_eq!(code, 200);
    assert!(text.contains("\"stuno\":\"COL1\""), "应存的是 COL1: {}", text);
    assert!(text.contains("\"total\":66"), "总分应为 66: {}", text);
    // 姓名原文完整保留（响应里引号被转义成 \"stuno\":\"FAKE1\"）
    assert!(text.contains("FAKE1"), "姓名原文应完整保留: {}", text);
}

#[test]
fn post_merges_surrogate_pair_at_end_of_string() {
    let s = Server::start("emoji", "SEED0001,甲,10\n");
    // 😀 = \uD83D\uDE00，低代理贴在字符串末尾（旧版 i+6 < len 差一，不合并）
    let body = "{\"stuno\":\"EMO1\",\"name\":\"\\uD83D\\uDE00\",\"total\":5}";
    let (code, text) = s.post(body);
    assert_eq!(code, 201, "应录入成功，实际 {} {}", code, text);
    let (code, text) = s.get("/api/students");
    assert_eq!(code, 200);
    assert!(text.contains("\"name\":\"😀\""), "姓名应是单个 😀: {}", text);
    // 数据文件里也必须是 4 字节的 UTF-8，而不是两个 U+FFFD
    let file = std::fs::read_to_string(s.dir.join("students.txt")).unwrap();
    assert!(file.contains("EMO1,😀,5"), "落盘内容不对: {:?}", file);
}

#[test]
fn name_length_limit_counts_characters() {
    let s = Server::start("name_len", "SEED0001,甲,10\n");
    let ok = format!(
        "{{\"stuno\":\"CN40\",\"name\":\"{}\",\"total\":1}}",
        "名".repeat(40)
    );
    assert_eq!(s.post(&ok).0, 201, "40 个汉字的姓名应放行");
    let too_long = format!(
        "{{\"stuno\":\"CN41\",\"name\":\"{}\",\"total\":1}}",
        "名".repeat(41)
    );
    let (code, text) = s.post(&too_long);
    assert_eq!(code, 400, "41 个汉字应被拒，实际 {}", code);
    assert!(text.contains("姓名过长"), "提示应为'姓名过长': {}", text);
    // 修改接口同样按字符数
    let put = format!("{{\"name\":\"{}\"}}", "姓".repeat(40));
    assert_eq!(http(s.port, "PUT", "/api/students/SEED0001", &put).0, 200);
}

#[test]
fn list_sort_and_search_still_work() {
    let s = Server::start("sort", "B2,乙,20\nA1,甲,30\nC3,丙,10\n");
    let (code, text) = s.get("/api/students?sort=total_asc");
    assert_eq!(code, 200);
    let idx = |name: &str| text.find(name).unwrap();
    assert!(idx("丙") < idx("乙") && idx("乙") < idx("甲"), "升序不对: {}", text);
    let (_, text) = s.get("/api/students?sort=stuno");
    assert!(text.find("A1").unwrap() < text.find("B2").unwrap(), "按学号排序不对: {}", text);
    let (code, text) = s.get("/api/students?q=乙");
    assert_eq!(code, 200);
    assert!(text.contains("B2") && !text.contains("A1"), "按姓名查找不对: {}", text);
}

#[test]
fn basic_endpoints_behave() {
    let s = Server::start("basic", "SEED0001,甲,10\n");
    assert_eq!(s.get("/").0, 200, "首页应 200");
    assert_eq!(s.get("/index.html").0, 200, "index.html 应 200");
    assert_eq!(s.get("/nope").0, 404, "不存在路径应 404");
    assert_eq!(http(s.port, "PATCH", "/api/students", "").0, 405, "不支持的方法应 405");
    assert_eq!(s.get("/api/students/NOPE").0 == 200, false);
    let (code, _) = http(s.port, "DELETE", "/api/students/NOPE", "");
    assert_eq!(code, 404, "删不存在的学号应 404");
    assert_eq!(s.post("not json").0, 400, "非 JSON body 应 400");
}
