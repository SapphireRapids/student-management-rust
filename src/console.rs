//! 控制台菜单：录入/显示/删除/修改/查找/排序。
//! 读输入时不持数据锁——输入慢不会阻塞网页请求（旧版持锁读确认，会拖住 worker）。

use std::io::{self, BufRead, Write};
use std::sync::Mutex;

use crate::store::{lock, pad, Store, Student};
use crate::{TOTAL_MAX, TOTAL_MIN};

/// 剥掉行尾 \r\n，再按 C++ trim 的语义去掉 ASCII 空白/控制字符。
fn trim_line(s: &str) -> String {
    s.trim_end_matches(['\n', '\r'])
        .trim_matches(|c: char| c <= ' ')
        .to_string()
}

/// 读一行；返回 None 表示 EOF（程序结束）。
fn read_line(prompt: &str) -> Option<String> {
    print!("{}", prompt);
    io::stdout().flush().ok()?;
    let mut line = String::new();
    let n = io::stdin().lock().read_line(&mut line).ok()?;
    if n == 0 {
        return None; // EOF
    }
    Some(trim_line(&line))
}

/// 读总分；非数字/越界要求重输，不会死循环。
/// None = EOF；Some(None) = 直接回车保持原值；Some(Some(v)) = 新输入的值。
fn read_total(prompt: &str, allow_keep: bool) -> Option<Option<i32>> {
    loop {
        let line = read_line(prompt)?;
        if line.is_empty() {
            if allow_keep {
                return Some(None); // 直接回车 = 保持不变
            }
            println!("输入不能为空，请重新输入。");
            continue;
        }
        if !line.bytes().all(|b| b.is_ascii_digit()) {
            println!("输入无效，请输入数字。");
            continue;
        }
        match line.parse::<i64>() {
            Ok(v) if v >= TOTAL_MIN as i64 && v <= TOTAL_MAX as i64 => return Some(Some(v as i32)),
            _ => println!("总分必须在 {}~{} 之间，请重新输入。", TOTAL_MIN, TOTAL_MAX),
        }
    }
}

/// 读 y/n。
fn read_yes_no(prompt: &str) -> Option<bool> {
    loop {
        let line = read_line(prompt)?;
        match line.as_str() {
            "" => continue,
            "y" | "yes" | "是" => return Some(true),
            "n" | "no" | "否" => return Some(false),
            _ => println!("输入无效，请输入 y 或 n。"),
        }
    }
}

fn print_table(list: &[Student]) {
    if list.is_empty() {
        println!("\n暂无学生信息。");
        return;
    }
    println!();
    println!("{}{}{}", pad("学号", 12), pad("姓名", 12), pad("总分", 8));
    for s in list {
        println!(
            "{}{}{}",
            pad(&s.stuno, 12),
            pad(&s.name, 12),
            pad(&s.total.to_string(), 8)
        );
    }
    println!("共 {} 名学生。", list.len());
}

fn op_add(store: &Mutex<Store>) {
    loop {
        let stuno = match read_line("请输入学号: ") {
            Some(s) => s,
            None => return,
        };
        if stuno.is_empty() {
            println!("学号不能为空，请重新输入。");
            continue;
        }
        if stuno.chars().count() > 20 {
            println!("学号过长（最多 20 个字符），请重新输入。");
            continue;
        }
        if !crate::store::valid_field(&stuno) {
            println!("学号含非法字符（不能包含逗号、制表符或控制字符），请重新输入。");
            continue;
        }
        if lock(store).find(&stuno).is_some() {
            println!("学号 {} 已存在，请重新输入。", stuno);
            continue;
        }

        let name = match read_line("请输入姓名: ") {
            Some(s) => s,
            None => return,
        };
        if name.is_empty() {
            println!("姓名不能为空，请重新输入。");
            continue;
        }
        if name.chars().count() > 40 {
            println!("姓名过长（最多 40 个字符），请重新输入。");
            continue;
        }
        if !crate::store::valid_field(&name) {
            println!("姓名含非法字符，请重新输入。");
            continue;
        }

        let prompt = format!("请输入总分({}~{}): ", TOTAL_MIN, TOTAL_MAX);
        let total = match read_total(&prompt, false) {
            Some(Some(t)) => t,
            _ => return, // EOF（allow_keep=false 时不会出现 Some(None)）
        };

        let mut st = lock(store);
        if st.find(&stuno).is_some() {
            // 录入期间学号刚被网页端占用
            println!("学号 {} 刚被占用，录入失败。", stuno);
            return;
        }
        st.students.push(Student {
            stuno: stuno.clone(),
            name: name.clone(),
            total,
        });
        st.save();
        drop(st);
        println!("录入成功：{} {}（总分 {}）。", name, stuno, total);
        return;
    }
}

fn op_show(store: &Mutex<Store>) {
    let st = lock(store);
    print_table(&st.students);
}

fn op_delete(store: &Mutex<Store>) {
    let stuno = match read_line("请输入要删除的学生学号: ") {
        Some(s) => s,
        None => return,
    };
    if stuno.is_empty() {
        println!("学号不能为空。");
        return;
    }

    {
        let st = lock(store);
        match st.find(&stuno) {
            Some(i) => {
                let s = &st.students[i];
                println!("找到学生：{} {}（总分 {}）", s.name, s.stuno, s.total);
            }
            None => {
                println!("未找到学号为 {} 的学生。", stuno);
                return;
            }
        }
    }

    let prompt = format!("确认删除学号 {} 的学生吗? (y/n): ", stuno);
    match read_yes_no(&prompt) {
        Some(true) => {}
        Some(false) => {
            println!("已取消删除。");
            return;
        }
        None => return,
    }

    // 等用户确认的这段时间里网页端可能增删过记录， Vec 下标会变。
    // 不能沿用之前的下标（会删错人甚至越界 panic），重新按学号找，找到哪个删哪个。
    let mut st = lock(store);
    match st.find(&stuno) {
        Some(i) => {
            st.students.remove(i);
            st.save();
            println!("删除成功。");
        }
        None => println!("学号 {} 刚被网页端删除，删除取消。", stuno),
    }
}

fn op_modify(store: &Mutex<Store>) {
    let stuno = match read_line("请输入要修改的学生学号: ") {
        Some(s) => s,
        None => return,
    };
    if stuno.is_empty() {
        println!("学号不能为空。");
        return;
    }

    {
        let st = lock(store);
        match st.find(&stuno) {
            Some(i) => {
                let s = &st.students[i];
                println!("当前信息：学号 {}，姓名 {}，总分 {}", s.stuno, s.name, s.total);
            }
            None => {
                println!("未找到学号为 {} 的学生。", stuno);
                return;
            }
        }
    }
    println!("（直接回车表示保持不变）");

    let new_name = match read_line("新姓名: ") {
        Some(s) => s,
        None => return,
    };
    if !new_name.is_empty() {
        if new_name.chars().count() > 40 {
            println!("姓名过长（最多 40 个字符），修改失败。");
            return;
        }
        if !crate::store::valid_field(&new_name) {
            println!("姓名含非法字符，修改失败。");
            return;
        }
    }

    let prompt = format!("新总分({}~{}): ", TOTAL_MIN, TOTAL_MAX);
    // Some(None) = 直接回车保持原值；保持原值的最终取值在写回时按学号现读，
    // 避免把网页端这期间刚改过的总分又覆盖回旧值
    let new_total = match read_total(&prompt, true) {
        Some(t) => t,
        None => return,
    };

    // 读新姓名/新总分期间没有持锁，网页端可能已删掉这个学号或改变了 Vec 顺序，
    // 下标全部作废——重新按学号找，找不到就取消，绝不按旧下标乱改
    let mut st = lock(store);
    match st.find(&stuno) {
        Some(i) => {
            let s = &mut st.students[i];
            if !new_name.is_empty() {
                s.name = new_name;
            }
            if let Some(t) = new_total {
                s.total = t;
            }
            let (name, stuno2, total) = (s.name.clone(), s.stuno.clone(), s.total);
            st.save();
            drop(st);
            println!("修改成功：{} {}（总分 {}）。", name, stuno2, total);
        }
        None => println!("学号 {} 刚被网页端删除，修改已取消。", stuno),
    }
}

fn op_search(store: &Mutex<Store>) {
    let kw = match read_line("请输入学号或姓名关键字: ") {
        Some(s) => s,
        None => return,
    };
    if kw.is_empty() {
        println!("关键字不能为空。");
        return;
    }
    let hits: Vec<Student> = {
        let st = lock(store);
        st.students
            .iter()
            .filter(|s| s.stuno == kw || s.name.contains(&kw))
            .cloned()
            .collect()
    };
    println!("关键字 \"{}\" 查到 {} 条结果。", kw, hits.len());
    print_table(&hits);
}

fn op_sort(store: &Mutex<Store>) {
    println!("\n请选择排序方式:\n1、按总分从高到低\n2、按总分从低到高");
    let line = loop {
        match read_line("请选择(1/2): ") {
            Some(l) if l == "1" || l == "2" => break l,
            Some(_) => println!("输入无效，请输入 1 或 2。"),
            None => return,
        }
    };
    {
        let mut st = lock(store);
        match line.as_str() {
            "1" => {
                st.students.sort_by_key(|s| std::cmp::Reverse(s.total));
                println!("已按总分从高到低排序。");
            }
            _ => {
                st.students.sort_by_key(|s| s.total);
                println!("已按总分从低到高排序。");
            }
        }
        st.save();
        print_table(&st.students);
    }
}

/// 控制台主循环；返回时程序准备退出。
pub fn run(store: &Mutex<Store>) {
    loop {
        println!("\n==================================================");
        println!("             欢迎来到学生成绩管理系统             ");
        println!("==================================================");
        println!("请选择要操作的命令");
        println!("1、录入学生信息");
        println!("2、显示学生信息");
        println!("3、删除学生信息");
        println!("4、修改学生信息");
        println!("5、查找学生信息");
        println!("6、按总分排序");
        println!("0、退出系统");

        let line = match read_line("请输入命令编号: ") {
            Some(l) => l,
            None => return, // EOF，退出
        };
        if line.is_empty() {
            println!("输入不能为空，请输入 0~6 的数字。");
            continue;
        }
        if !line.bytes().all(|b| b.is_ascii_digit()) {
            println!("输入无效，请输入 0~6 的数字。");
            continue;
        }
        match line.parse::<u32>() {
            Ok(1) => op_add(store),
            Ok(2) => op_show(store),
            Ok(3) => op_delete(store),
            Ok(4) => op_modify(store),
            Ok(5) => op_search(store),
            Ok(6) => op_sort(store),
            Ok(0) => {
                println!("再见！");
                return;
            }
            _ => println!("没有该命令，请输入 0~6 的数字。"),
        }
    }
}
