//! 数据模型与持久化：内存 Vec + 互斥锁保护，每次变更立即原子落盘。

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::MutexGuard;

#[derive(Clone)]
pub struct Student {
    pub stuno: String,
    pub name: String,
    pub total: i32,
}

/// 学号/姓名合法性：非空、限长、不能含逗号/制表符/控制字符（数据文件按逗号分隔）。
pub fn valid_field(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && !s.chars().any(|c| c == ',' || c == '\t' || (c as u32) < 0x20)
}

/// 全局共享数据；控制台线程与 HTTP 线程共用。
pub struct Store {
    pub students: Vec<Student>,
    data_file: PathBuf,
}

/// 锁 poisoning 时不panic：取回内部数据继续用，单个请求崩溃不拖垮整个服务。
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Store {
    /// 从数据文件加载；文件不存在时从空开始。
    pub fn load(data_file: &Path) -> Store {
        let mut store = Store {
            students: Vec::new(),
            data_file: data_file.to_path_buf(),
        };
        let mut legacy = false;
        if let Ok(text) = fs::read_to_string(data_file) {
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                let f: Vec<&str> = line.split(',').collect();
                let s = match f.len() {
                    // 新版：学号,姓名,总分
                    3 => Student {
                        stuno: f[0].to_string(),
                        name: f[1].to_string(),
                        total: f[2].trim().parse().unwrap_or(0),
                    },
                    // 旧版：学号,姓名,语,数,英 → 三科求和
                    5 => {
                        legacy = true;
                        Student {
                            stuno: f[0].to_string(),
                            name: f[1].to_string(),
                            total: ["", "", ""]
                                .iter()
                                .zip(f.iter().skip(2))
                                .map(|(_, v)| v.trim().parse::<i32>().unwrap_or(0))
                                .sum(),
                        }
                    }
                    _ => continue,
                };
                if valid_field(&s.stuno) && valid_field(&s.name) {
                    store.students.push(s);
                }
            }
        }
        // 旧格式立刻转存成新格式，之后断电、强杀都不会把两种格式混在一起
        if legacy {
            println!("[提示] 检测到旧版数据（学号,姓名,语文,数学,英语），已按三科之和转为总分并保存为新格式。");
            store.save();
        }
        store
    }

    /// 原子落盘：先写 .tmp 并刷盘，再 rename 替换——任何时刻磁盘上
    /// 要么是旧的完整文件，要么是新的完整文件，不会被截断成半截。
    pub fn save(&self) {
        let tmp = self.data_file.with_extension("txt.tmp");
        let result = (|| -> std::io::Result<()> {
            let f = File::create(&tmp)?;
            let mut w = std::io::BufWriter::new(f);
            for s in &self.students {
                writeln!(w, "{},{},{}", s.stuno, s.name, s.total)?;
            }
            w.flush()?;
            w.into_inner()?.sync_all()?; // 数据真正落盘后再替换（等价旧版 MOVEFILE_WRITE_THROUGH）
            Ok(())
        })();
        match result {
            Err(e) => {
                println!("[警告] 写入 {} 失败（{}），本次修改只保存在内存中。", tmp.display(), e);
                let _ = fs::remove_file(&tmp);
            }
            Ok(()) => {
                if let Err(e) = fs::rename(&tmp, &self.data_file) {
                    println!(
                        "[警告] 替换 {} 失败（{}），本次修改只保存在内存中。",
                        self.data_file.display(),
                        e
                    );
                    let _ = fs::remove_file(&tmp);
                }
            }
        }
    }

    pub fn find(&self, stuno: &str) -> Option<usize> {
        self.students.iter().position(|s| s.stuno == stuno)
    }
}

/// 把数据文件/前端页面定位到 EXE 所在目录（避免依赖当前工作目录）。
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

// ---------------------------------------------------------------- 控制台表格对齐
// 显示宽度（中文按 2 列计）

fn utf8_char_len(b: &[u8], i: usize) -> usize {
    match b[i] {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn cp_of(b: &[u8], i: usize, n: usize) -> u32 {
    let mut cp = 0u32;
    for k in 0..n {
        let c = b[i + k] as u32;
        cp = if k == 0 { c & (0xFF >> (n + 1)) } else { (cp << 6) | (c & 0x3F) };
    }
    cp
}

fn wide_cp(cp: u32) -> bool {
    (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE4F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp)
}

pub fn disp_width(s: &str) -> usize {
    let b = s.as_bytes();
    let mut w = 0;
    let mut i = 0;
    while i < b.len() {
        let mut n = utf8_char_len(b, i);
        if i + n > b.len() {
            n = 1;
        }
        w += if wide_cp(cp_of(b, i, n)) { 2 } else { 1 };
        i += n;
    }
    w
}

pub fn pad(s: &str, width: usize) -> String {
    let d = disp_width(s);
    if d >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - d))
    }
}
