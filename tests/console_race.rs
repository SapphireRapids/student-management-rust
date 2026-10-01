//! 回归测试：控制台删除/修改的过程中，网页端并发增删记录。
//!
//! 旧实现在「按学号查到的手上下标」和「真正删/改」之间要等用户输 y/n、新姓名、新总分，
//! 而等待期间数据锁是放开的（这是有意设计：输入慢不该挡住网页请求）。
//! 网页端在这期间删掉前面一条记录，下标就指向了别人——轻则删错/改错人，
//! 重则 Vec 变短后越界 panic（退出码 101，连正在处理的网页请求一起死）。
//! 现在改成写回前重新按学号查找，这里用真实进程 + 真实 HTTP 请求复现场景。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const SEED: &str = "SEED0001,甲,10\nSEED0002,乙,20\nSEED0003,丙,30\n";

static PORT_SEQ: AtomicU16 = AtomicU16::new(0);

/// 每个用例一个独立端口（同进程内的测试是并发线程跑的）。
fn next_port() -> u16 {
    20000 + (std::process::id() % 15000) as u16 + PORT_SEQ.fetch_add(1, Ordering::SeqCst)
}

/// 发一个 HTTP 请求，返回 (状态码, 响应文本)。
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

fn wait_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if http(port, "GET", "/api/students", "").0 == 200 {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("服务端 20 秒内未就绪");
}

/// 一个"控制台菜单 + HTTP 服务"的真实进程；数据文件放在它自己的临时目录里，
/// 互不干扰，退出（含 panic）时统一回收。
struct App {
    child: Child,
    stdin: std::process::ChildStdin,
    out: Receiver<String>, // 子进程 stdout 的按行副本（读线程投递）
    dir: PathBuf,
    port: u16,
}

impl App {
    fn start(tag: &str, seed: &str) -> App {
        let dir: PathBuf = std::env::temp_dir().join(format!(
            "sms_race_{}_{}_{}",
            tag,
            std::process::id(),
            next_port()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = next_port();
        std::fs::write(dir.join("students.txt"), seed).unwrap();
        // 数据文件按 EXE 所在目录定位，所以把 exe 拷贝进临时目录再跑
        std::fs::copy(env!("CARGO_BIN_EXE_sms"), dir.join("sms.exe")).unwrap();

        let mut child = Command::new(dir.join("sms.exe"))
            // 注意：不能加 --server（那是纯服务器模式，没有控制台菜单）；
            // 默认模式 = 控制台菜单 + HTTP 服务同时跑，正是要测的组合
            .args(["--port", &port.to_string()])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        // 单独一个线程按行读 stdout：提示符不带换行，直接 read_line 会一直阻塞，
        // 走队列 + recv_timeout 才能做到"等不到就快速失败"而不是挂死整个测试
        let (tx, out) = mpsc::channel();
        thread::spawn(move || {
            let mut r = std::io::BufReader::new(stdout);
            let mut line = String::new();
            while let Ok(n) = std::io::BufRead::read_line(&mut r, &mut line) {
                if n == 0 {
                    break;
                }
                if tx.send(line.clone()).is_err() {
                    break;
                }
                line.clear();
            }
        });

        let app = App {
            child,
            stdin,
            out,
            dir,
            port,
        };
        wait_ready(port);
        app
    }

    /// 往控制台喂一行输入。
    fn send(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
    }

    /// 等 stdout 出现含 want 的一行（只能等带换行的输出行；提示符不带换行，等不到）。
    fn wait_for(&mut self, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let line = self
                .out
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("等待子进程输出超时");
            if line.contains(want) {
                return line;
            }
        }
    }

    fn data(&self) -> String {
        std::fs::read_to_string(self.dir.join("students.txt")).unwrap()
    }

    fn alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn console_delete_survives_concurrent_web_delete() {
    let mut app = App::start("delete", SEED);
    app.send("3"); // 3、删除学生信息
    app.send("SEED0002");
    // 已找到人、正在等 y/n——此刻锁是放开的，网页端刚好插进来删掉 SEED0001
    app.wait_for("找到学生：乙");
    assert_eq!(http(app.port, "DELETE", "/api/students/SEED0001", "").0, 200);
    app.send("y");

    app.wait_for("删除成功。");
    assert!(app.alive(), "进程不该退出（越界 panic 会以退出码 101 死掉）");
    // SEED0001 由网页端删掉，SEED0002 由控制台删掉，SEED0003 必须原样留着。
    // 若沿用旧下标，被删掉的会是 SEED0003，而 SEED0002 还留在文件里。
    assert_eq!(app.data(), "SEED0003,丙,30\n");
}

#[test]
fn console_modify_survives_concurrent_web_delete() {
    let mut app = App::start("modify", SEED);
    app.send("4"); // 4、修改学生信息
    app.send("SEED0002");
    app.wait_for("当前信息：学号 SEED0002");
    app.wait_for("直接回车表示保持不变"); // 到这行说明已放开锁、停在"新姓名"输入
    app.send(""); // 新姓名：直接回车，保持不变
    assert_eq!(http(app.port, "DELETE", "/api/students/SEED0001", "").0, 200);
    app.send("999"); // 之后控制台才会去拿锁写回，删除必定落在窗口内

    app.wait_for("修改成功：乙 SEED0002（总分 999）");
    assert!(app.alive(), "进程不该退出（越界 panic 会以退出码 101 死掉）");
    // 改的必须是乙本人。沿用旧下标会改成丙（SEED0003），乙原地不动。
    assert_eq!(app.data(), "SEED0002,乙,999\nSEED0003,丙,30\n");
}

#[test]
fn console_delete_aborts_when_web_already_deleted_target() {
    let mut app = App::start("delete_gone", SEED);
    app.send("3");
    app.send("SEED0002");
    app.wait_for("找到学生：乙");
    // 网页端抢先删掉了同一个人，控制台确认时应当取消而不是删错人
    assert_eq!(http(app.port, "DELETE", "/api/students/SEED0002", "").0, 200);
    assert_eq!(http(app.port, "DELETE", "/api/students/SEED0001", "").0, 200);
    app.send("y");

    app.wait_for("刚被网页端删除，删除取消");
    assert!(app.alive());
    assert_eq!(app.data(), "SEED0003,丙,30\n");
}

#[test]
fn console_add_rejects_overlong_chinese_name() {
    // 姓名上限按字符数算：40 个汉字（120 字节）必须放行，41 个才提示过长
    let mut app = App::start("long_name", SEED);
    app.send("1"); // 1、录入学生信息
    app.send("NEW0001");
    app.send(&"名".repeat(40));
    app.send("88"); // 总分
    app.wait_for("录入成功");
    let (code, body) = http(app.port, "GET", "/api/students", "");
    assert_eq!(code, 200);
    assert!(body.contains(&"名".repeat(40)), "40 个汉字的姓名应能录入");
}
