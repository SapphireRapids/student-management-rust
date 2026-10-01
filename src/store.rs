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

/// 学号/姓名合法性：非空、限长（按字符数，与界面上"最多 N 个字符"的提示一致）、
/// 不能含逗号/制表符/控制字符（数据文件按逗号分隔）。
pub fn valid_field(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= 40
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
        // 上次被强杀在写一半时可能留下的 .tmp：它从不被读取，启动时清掉，
        // 否则会一直躺在数据文件旁边（下次保存会原地复写，但没必要留着）
        let _ = fs::remove_file(data_file.with_extension("txt.tmp"));
        let mut legacy = false;
        // 按原始字节读入再宽松转码：个别非法 UTF-8 字节只让对应字符变成 U+FFFD。
        // 旧版用 std::getline 读原始字节，行为一致；若改用 read_to_string，
        // 一个坏字节就会让整份文件读不出来，之后第一次保存直接把全部数据覆盖丢失。
        if let Ok(bytes) = fs::read(data_file) {
            let text = String::from_utf8_lossy(&bytes);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sms_store_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("students.txt")
    }

    #[test]
    fn valid_field_counts_chars_not_bytes() {
        // 40 个汉字 = 120 字节：按字符算必须放行（否则中文姓名会被误判"非法字符"）
        assert!(valid_field(&"学".repeat(40)));
        assert!(!valid_field(&"学".repeat(41)));
        assert!(valid_field(&"a".repeat(40)));
        assert!(!valid_field(&"a".repeat(41)));
        assert!(!valid_field(""));
        assert!(!valid_field("a,b"));
        assert!(!valid_field("a\tb"));
        assert!(!valid_field("a\u{1}b"));
    }

    #[test]
    fn load_keeps_records_when_file_has_invalid_utf8() {
        // 一个坏字节不该让整份数据读成空文件（那之后第一次保存会把数据全覆盖丢失）
        let f = tmp("bad_utf8");
        let mut bytes = b"S1,\xff\xfe,10\nS2,\xE6\x9D\x8E\xE5\x9B\x9B,20\n".to_vec();
        bytes.extend_from_slice(b"S3,\xE7\x8E\x8B\xE4\xBA\x94,30\n");
        fs::write(&f, &bytes).unwrap();
        let st = Store::load(&f);
        assert_eq!(st.students.len(), 3);
        assert_eq!(st.students[0].stuno, "S1");
        assert_eq!(st.students[0].name.matches('\u{FFFD}').count(), 2); // 两个坏字节各变一个
        assert_eq!(st.students[1].name, "李四"); // 其余记录完好无损
        assert_eq!(st.students[1].total, 20);
        let _ = fs::remove_file(&f);
    }

    #[test]
    fn load_parses_new_and_legacy_formats() {
        let f = tmp("legacy");
        fs::write(&f, "S1,张三,10\nS2,李四,80,90,100\n坏行\nS3,,30\n").unwrap();
        let st = Store::load(&f);
        assert_eq!(st.students.len(), 2); // 姓名为空的坏行被过滤
        assert_eq!(st.students[0].total, 10);
        assert_eq!(st.students[1].total, 270); // 80+90+100
        let _ = fs::remove_file(&f);
    }

    #[test]
    fn load_removes_stale_tmp_left_by_a_kill() {
        let f = tmp("stale_tmp");
        fs::write(&f, "S1,张三,10\n").unwrap();
        let tmpf = f.with_extension("txt.tmp");
        fs::write(&tmpf, "半截数据").unwrap();
        assert!(tmpf.exists());
        let st = Store::load(&f);
        assert_eq!(st.students.len(), 1);
        assert!(!tmpf.exists(), "启动时应清掉上次强杀残留的 .tmp");
        let _ = fs::remove_file(&f);
    }

    #[test]
    fn save_replaces_file_and_leaves_no_tmp() {
        let f = tmp("save");
        let mut st = Store::load(&f);
        st.students.push(Student { stuno: "S1".into(), name: "张三".into(), total: 7 });
        st.save();
        st.students.push(Student { stuno: "S2".into(), name: "李四".into(), total: 8 });
        st.save();
        let back = Store::load(&f);
        assert_eq!(back.students.len(), 2);
        assert_eq!(back.students[1].total, 8);
        assert!(!f.with_extension("txt.tmp").exists());
        let _ = fs::remove_file(&f);
    }
}
