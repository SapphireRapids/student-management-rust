//! 内置 HTTP 服务：固定工作线程 + 连接队列 + 收发超时，仅监听 127.0.0.1。
//! 架构与旧版 C++ 一致：accept 线程只负责收连接，worker 线程顺序处理，
//! 浏览器多开标签页反复刷新也只是排队，不会一个连接起一个线程。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use crate::json::{self, Value};
use crate::store::{lock, valid_field, Store, Student};
use crate::{TOTAL_MAX, TOTAL_MIN};

const SOCK_TIMEOUT: Duration = Duration::from_secs(10); // 单连接单次 recv/send 超时
const MAX_PENDING: usize = 64; // 排队等待处理的连接上限（超出直接断开，有意背压）
const MAX_HEADER: usize = 1 << 20; // 请求头上限
const MAX_TOTAL: usize = 2 << 20; // 头部 + body 总上限
const RECV_BUF: usize = 8192;

/// HTTP 服务依赖的共享状态（store 与控制台线程共用同一把锁）。
pub struct Ctx {
    pub store: Arc<Mutex<Store>>,
    pub index_file: PathBuf,
}

struct Inner {
    listener: TcpListener,
    pending: Mutex<VecDeque<TcpStream>>,
    cv: Condvar,
    stop: AtomicBool,
    ctx: Arc<Ctx>,
}

pub struct HttpServer {
    inner: Arc<Inner>,
}

/// 绑定 127.0.0.1 并启动 accept 线程 + 固定数量的 worker 线程。
/// 端口被占用等失败时返回 None（调用方退化为仅控制台模式）。
pub fn start(port: u16, workers: usize, ctx: Arc<Ctx>) -> Option<HttpServer> {
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    let inner = Arc::new(Inner {
        listener,
        pending: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
        stop: AtomicBool::new(false),
        ctx,
    });
    thread::Builder::new()
        .name("accept".into())
        .spawn({
            let inner = Arc::clone(&inner);
            move || accept_loop(&inner)
        })
        .ok()?;
    for i in 0..workers {
        thread::Builder::new()
            .name(format!("worker-{}", i))
            .spawn({
                let inner = Arc::clone(&inner);
                move || worker_loop(&inner)
            })
            .ok()?;
    }
    Some(HttpServer { inner })
}

impl HttpServer {
    /// 停止接收新连接、清空排队连接并唤醒 worker；正在处理的连接
    /// 受超时约束自行结束，进程退出时统一回收（与旧版行为一致）。
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        {
            let mut q = lock(&self.inner.pending);
            while q.pop_front().is_some() {} // drop 即关闭
        }
        self.inner.cv.notify_all();
    }
}

fn accept_loop(inner: &Arc<Inner>) {
    while let Ok((stream, _)) = inner.listener.accept() {
        if inner.stop.load(Ordering::SeqCst) {
            break;
        }
        stream.set_nonblocking(false).ok(); //  accepted socket 不继承监听属性
        let _ = stream.set_read_timeout(Some(SOCK_TIMEOUT));
        let _ = stream.set_write_timeout(Some(SOCK_TIMEOUT));

        let mut q = lock(&inner.pending);
        if q.len() >= MAX_PENDING {
            continue; // 排队已满，直接断开这个连接
        }
        q.push_back(stream);
        drop(q);
        inner.cv.notify_one();
    }
}

fn worker_loop(inner: &Arc<Inner>) {
    loop {
        let stream = {
            let mut q = lock(&inner.pending);
            while !inner.stop.load(Ordering::SeqCst) && q.is_empty() {
                q = inner.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            match q.pop_front() {
                Some(s) => s,
                None => return, // 只有停止时才会空着醒来
            }
        };
        handle_connection(stream, &inner.ctx);
    }
}

// ---------------------------------------------------------------- 请求解析

struct HttpRequest {
    method: String,
    path: String,
    query: String,
    body: String,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim_ascii(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// 读满头部（\r\n\r\n 为止），再按 Content-Length 读 body；
/// 读出错/超时/超限全部按关闭连接处理。
fn read_request(stream: &mut TcpStream) -> Option<HttpRequest> {
    let mut buf: Vec<u8> = Vec::with_capacity(RECV_BUF);
    let mut tmp = [0u8; RECV_BUF];
    let mut header_end = None;
    while header_end.is_none() {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > MAX_HEADER {
            return None;
        }
        header_end = find(&buf, b"\r\n\r\n");
    }
    let header_end = header_end?;

    let line_end = find(&buf, b"\r\n")?;
    let req_line = String::from_utf8_lossy(&buf[..line_end]).into_owned();
    let mut it = req_line.split_whitespace();
    let method = it.next()?.to_string();
    let target = it.next()?.to_string();

    let mut headers = HashMap::new();
    let mut pos = line_end + 2;
    while pos < header_end {
        let e = find(&buf[pos..header_end], b"\r\n").map(|o| pos + o).unwrap_or(header_end);
        let h = String::from_utf8_lossy(&buf[pos..e]).into_owned();
        if let Some(c) = h.find(':') {
            let k = h[..c].trim_ascii().to_ascii_lowercase();
            headers.insert(k, trim_ascii(&h[c + 1..]).to_string());
        }
        pos = e + 2;
    }

    let (path, query) = match target.find('?') {
        Some(qp) => (target[..qp].to_string(), target[qp + 1..].to_string()),
        None => (target, String::new()),
    };

    let body_start = header_end + 4;
    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() - body_start < content_len {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if buf.len() > MAX_TOTAL {
            break;
        }
    }
    let end = (body_start + content_len).min(buf.len());
    let body = if content_len > 0 && end > body_start {
        String::from_utf8_lossy(&buf[body_start..end]).into_owned()
    } else {
        String::new()
    };

    Some(HttpRequest {
        method,
        path,
        query,
        body,
    })
}

// ---------------------------------------------------------------- 响应

fn send_response(stream: &mut TcpStream, code: u16, status: &str, ctype: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        code,
        status,
        ctype,
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn send_json(stream: &mut TcpStream, code: u16, status: &str, js: &str) {
    send_response(stream, code, status, "application/json; charset=utf-8", js);
}

fn err_json(msg: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{}\"}}", json::escape(msg))
}

fn student_json(s: &Student) -> String {
    format!(
        "{{\"stuno\":\"{}\",\"name\":\"{}\",\"total\":{}}}",
        json::escape(&s.stuno),
        json::escape(&s.name),
        s.total
    )
}

// ---------------------------------------------------------------- 工具

fn hex_val(c: u8) -> i32 {
    match c {
        b'0'..=b'9' => (c - b'0') as i32,
        b'a'..=b'f' => (c - b'a' + 10) as i32,
        b'A'..=b'F' => (c - b'A' + 10) as i32,
        _ => -1,
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = hex_val(b[i + 1]);
            let l = hex_val(b[i + 2]);
            if h >= 0 && l >= 0 {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for kv in q.split('&') {
        if kv.is_empty() {
            continue;
        }
        match kv.find('=') {
            Some(e) => {
                m.insert(url_decode(&kv[..e]), url_decode(&kv[e + 1..]));
            }
            None => {
                m.insert(url_decode(kv), String::new());
            }
        }
    }
    m
}

// ---------------------------------------------------------------- 接口

/// 从请求体取总分：必须是 TOTAL_MIN~TOTAL_MAX 范围内的整数。
fn parse_total(body: &str) -> Result<i32, String> {
    match json::get(body, "total") {
        Some(Value::Num(dv)) => {
            // 先比范围再取整：1e300 这类天文数字直接拒绝，不会在取整时溢出
            if dv < TOTAL_MIN as f64 || dv > TOTAL_MAX as f64 || dv.fract() != 0.0 {
                Err(format!("总分必须是 {}~{} 的整数", TOTAL_MIN, TOTAL_MAX))
            } else {
                Ok(dv as i32)
            }
        }
        _ => Err("缺少或非法的总分".to_string()),
    }
}

/// 从请求体取非空字符串字段（学号/姓名）；label 是出错提示里的中文字段名。
fn parse_str_field(body: &str, key: &str, label: &str) -> Result<String, String> {
    match json::get(body, key) {
        Some(Value::Str(s)) if !s.is_empty() => Ok(s),
        _ => Err(format!("{}不能为空", label)),
    }
}

fn handle_api(stream: &mut TcpStream, ctx: &Ctx, req: &HttpRequest) {
    let path = req.path.clone();

    if path == "/api/students" {
        if req.method == "GET" {
            let q = parse_query(&req.query);
            let sort = q.get("sort").cloned().unwrap_or_else(|| "total_desc".into());
            let kw = q.get("q").cloned().unwrap_or_default();
            let mut list = lock(&ctx.store).students.clone();
            if !kw.is_empty() {
                list.retain(|s| s.stuno == kw || s.name.contains(&kw));
            }
            match sort.as_str() {
                "total_asc" => list.sort_by_key(|s| s.total),
                "stuno" => list.sort_by_key(|s| s.stuno.clone()),
                _ => list.sort_by_key(|s| std::cmp::Reverse(s.total)),
            }
            let items: Vec<String> = list.iter().map(student_json).collect();
            let js = format!(
                "{{\"students\":[{}],\"count\":{}}}",
                items.join(","),
                list.len()
            );
            send_json(stream, 200, "OK", &js);
            return;
        }
        if req.method == "POST" {
            let body = &req.body;
            if !body.contains('{') {
                send_json(stream, 400, "Bad Request", &err_json("请求体不是有效的 JSON"));
                return;
            }
            let stuno = match parse_str_field(body, "stuno", "学号") {
                Ok(v) => v,
                Err(e) => {
                    send_json(stream, 400, "Bad Request", &err_json(&e));
                    return;
                }
            };
            if stuno.len() > 20 {
                send_json(stream, 400, "Bad Request", &err_json("学号过长（最多 20 个字符）"));
                return;
            }
            if !valid_field(&stuno) {
                send_json(
                    stream,
                    400,
                    "Bad Request",
                    &err_json("学号含非法字符（不能包含逗号、制表符或控制字符）"),
                );
                return;
            }
            let name = match parse_str_field(body, "name", "姓名") {
                Ok(v) => v,
                Err(e) => {
                    send_json(stream, 400, "Bad Request", &err_json(&e));
                    return;
                }
            };
            if !valid_field(&name) {
                send_json(stream, 400, "Bad Request", &err_json("姓名含非法字符"));
                return;
            }
            let total = match parse_total(body) {
                Ok(v) => v,
                Err(e) => {
                    send_json(stream, 400, "Bad Request", &err_json(&e));
                    return;
                }
            };
            let mut store = lock(&ctx.store);
            if store.find(&stuno).is_some() {
                send_json(stream, 409, "Conflict", &err_json("学号已存在"));
                return;
            }
            let s = Student {
                stuno: stuno.clone(),
                name,
                total,
            };
            store.students.push(s.clone());
            store.save();
            send_json(
                stream,
                201,
                "Created",
                &format!("{{\"ok\":true,\"student\":{}}}", student_json(&s)),
            );
            return;
        }
        send_json(stream, 405, "Method Not Allowed", &err_json("方法不被支持"));
        return;
    }

    // /api/students/<学号>
    let prefix = "/api/students/";
    if let Some(rest) = path.strip_prefix(prefix) {
        let stuno = url_decode(rest);
        if stuno.is_empty() {
            send_json(stream, 400, "Bad Request", &err_json("缺少学号"));
            return;
        }
        let mut store = lock(&ctx.store);
        let idx = store.find(&stuno);
        match req.method.as_str() {
            "DELETE" => match idx {
                None => send_json(stream, 404, "Not Found", &err_json("未找到该学生")),
                Some(i) => {
                    store.students.remove(i);
                    store.save();
                    send_json(
                        stream,
                        200,
                        "OK",
                        &format!("{{\"ok\":true,\"stuno\":\"{}\"}}", json::escape(&stuno)),
                    );
                }
            },
            "PUT" => match idx {
                None => send_json(stream, 404, "Not Found", &err_json("未找到该学生")),
                Some(i) => {
                    let body = &req.body;
                    let mut s = store.students[i].clone();
                    // name 给了才校验/修改
                    if json::get(body, "name").is_some() {
                        match parse_str_field(body, "name", "姓名") {
                            Ok(name) => {
                                if !valid_field(&name) {
                                    send_json(stream, 400, "Bad Request", &err_json("姓名含非法字符"));
                                    return;
                                }
                                s.name = name;
                            }
                            Err(e) => {
                                send_json(stream, 400, "Bad Request", &err_json(&e));
                                return;
                            }
                        }
                    }
                    // total 给了才校验/修改
                    if json::get(body, "total").is_some() {
                        match parse_total(body) {
                            Ok(total) => s.total = total,
                            Err(e) => {
                                send_json(stream, 400, "Bad Request", &err_json(&e));
                                return;
                            }
                        }
                    }
                    store.students[i] = s.clone();
                    store.save();
                    send_json(
                        stream,
                        200,
                        "OK",
                        &format!("{{\"ok\":true,\"student\":{}}}", student_json(&s)),
                    );
                }
            },
            _ => send_json(stream, 405, "Method Not Allowed", &err_json("方法不被支持")),
        }
        return;
    }

    send_json(stream, 404, "Not Found", &err_json("接口不存在"));
}

fn handle_connection(mut stream: TcpStream, ctx: &Ctx) {
    let req = match read_request(&mut stream) {
        Some(r) => r,
        None => return, // 读失败/超时/超限：直接关闭
    };

    if req.path == "/" || req.path == "/index.html" || req.path == "/sms" || req.path == "/sms.html" {
        if req.method != "GET" && req.method != "HEAD" {
            send_json(&mut stream, 405, "Method Not Allowed", &err_json("方法不被支持"));
            return;
        }
        let html = std::fs::read(&ctx.index_file).unwrap_or_default();
        if html.is_empty() {
            let msg = "缺少 index.html，请将网页前端文件放到程序同目录。";
            if req.method == "HEAD" {
                send_response(&mut stream, 404, "Not Found", "text/plain; charset=utf-8", "");
            } else {
                send_response(
                    &mut stream,
                    404,
                    "Not Found",
                    "text/plain; charset=utf-8",
                    msg,
                );
            }
        } else if req.method == "HEAD" {
            // HEAD 只发头部，Content-Length 仍按实体长度（旧版会把 body 也发出去）
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                html.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        } else {
            let body = String::from_utf8_lossy(&html).into_owned();
            send_response(&mut stream, 200, "OK", "text/html; charset=utf-8", &body);
        }
        return;
    }

    if req.path.starts_with("/api/") {
        handle_api(&mut stream, ctx, &req);
        return;
    }

    if req.path == "/favicon.ico" {
        send_response(&mut stream, 204, "No Content", "image/x-icon", "");
        return;
    }

    send_json(&mut stream, 404, "Not Found", &err_json("接口不存在"));
}
