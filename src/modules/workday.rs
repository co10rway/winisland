//! 工作日判定 + 上班状态 + 节日
//!
//! 数据分三层，逐层覆盖：
//! 1. 内置节假日/调休表（国务院办公厅 2025、2026 安排，离线可用）
//! 2. 内置节日表（公历固定节日 + 可计算节日 + 农历节日，覆盖 2025–2027）
//! 3. 联网同步的官方日历（缓存到 %APPDATA%\WinIsland\holidays.json，可离线复用）
//!
//! 自定义工作日（设置里勾选周几）作为「基础规则」；节假日/调休表作为覆盖层：
//! 放假日一律休息，调休上班日一律上班。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{OnceLock, RwLock};

use windows::Win32::Foundation::SYSTEMTIME;

use crate::config::Salary;
use crate::modules::clock;

/// 法定放假日（含调休放假的工作日）；周末本身已按周规则休息，不必重复列
static BUILTIN_REST: &[(i32, u32, u32, &str)] = &[
    // ── 2025 ──
    (2025, 1, 1, "元旦"),
    (2025, 1, 28, "春节"),
    (2025, 1, 29, "春节"),
    (2025, 1, 30, "春节"),
    (2025, 1, 31, "春节"),
    (2025, 2, 3, "春节"),
    (2025, 2, 4, "春节"),
    (2025, 4, 4, "清明节"),
    (2025, 5, 1, "劳动节"),
    (2025, 5, 2, "劳动节"),
    (2025, 5, 5, "劳动节"),
    (2025, 6, 2, "端午节"),
    (2025, 10, 1, "国庆节"),
    (2025, 10, 2, "国庆节"),
    (2025, 10, 3, "国庆节"),
    (2025, 10, 6, "国庆节"),
    (2025, 10, 7, "国庆节"),
    (2025, 10, 8, "国庆节"),
    // ── 2026 ──
    (2026, 1, 1, "元旦"),
    (2026, 1, 2, "元旦"),
    (2026, 2, 16, "春节"),
    (2026, 2, 17, "春节"),
    (2026, 2, 18, "春节"),
    (2026, 2, 19, "春节"),
    (2026, 2, 20, "春节"),
    (2026, 2, 23, "春节"),
    (2026, 4, 6, "清明节"),
    (2026, 5, 1, "劳动节"),
    (2026, 5, 4, "劳动节"),
    (2026, 5, 5, "劳动节"),
    (2026, 6, 19, "端午节"),
    (2026, 9, 25, "中秋节"),
    (2026, 10, 1, "国庆节"),
    (2026, 10, 2, "国庆节"),
    (2026, 10, 5, "国庆节"),
    (2026, 10, 6, "国庆节"),
    (2026, 10, 7, "国庆节"),
];

/// 调休上班日（本该休息的周末要上班）
static BUILTIN_WORK: &[(i32, u32, u32)] = &[
    // ── 2025 ──
    (2025, 1, 26),
    (2025, 2, 8),
    (2025, 4, 27),
    (2025, 9, 28),
    (2025, 10, 11),
    // ── 2026 ──
    (2026, 1, 4),
    (2026, 2, 14),
    (2026, 2, 28),
    (2026, 5, 9),
    (2026, 9, 20),
    (2026, 10, 10),
];

/// 公历固定节日（月, 日, 名称）
static FEST_FIXED: &[(u32, u32, &str)] = &[
    (1, 1, "元旦"),
    (2, 14, "情人节"),
    (3, 8, "妇女节"),
    (3, 12, "植树节"),
    (4, 1, "愚人节"),
    (4, 22, "地球日"),
    (5, 1, "劳动节"),
    (5, 4, "青年节"),
    (6, 1, "儿童节"),
    (7, 1, "建党节"),
    (8, 1, "建军节"),
    (9, 10, "教师节"),
    (10, 1, "国庆节"),
    (10, 31, "万圣节"),
    (11, 11, "双十一"),
    (12, 12, "双十二"),
    (12, 24, "平安夜"),
    (12, 25, "圣诞节"),
];

/// 农历节日（离线表，覆盖 2025–2027：除夕/春节/元宵/清明/端午/七夕/中秋/重阳）
static FEST_LUNAR: &[(i32, u32, u32, &str)] = &[
    (2025, 1, 28, "除夕"),
    (2025, 1, 29, "春节"),
    (2025, 2, 12, "元宵节"),
    (2025, 4, 4, "清明节"),
    (2025, 5, 31, "端午节"),
    (2025, 8, 29, "七夕"),
    (2025, 10, 6, "中秋节"),
    (2025, 10, 29, "重阳节"),
    (2026, 2, 16, "除夕"),
    (2026, 2, 17, "春节"),
    (2026, 3, 3, "元宵节"),
    (2026, 4, 5, "清明节"),
    (2026, 6, 19, "端午节"),
    (2026, 8, 19, "七夕"),
    (2026, 9, 25, "中秋节"),
    (2026, 10, 18, "重阳节"),
    (2027, 2, 5, "除夕"),
    (2027, 2, 6, "春节"),
    (2027, 2, 20, "元宵节"),
    (2027, 4, 5, "清明节"),
    (2027, 6, 9, "端午节"),
    (2027, 8, 8, "七夕"),
    (2027, 9, 15, "中秋节"),
    (2027, 10, 8, "重阳节"),
];

// ───────────────────────── 日历数据 ─────────────────────────

#[derive(Clone)]
struct Day {
    date: (i32, u32, u32),
    name: String,
    off: bool,
}

#[derive(Default)]
struct Cal {
    /// 放假日 -> 假期名
    rest: HashMap<(i32, u32, u32), String>,
    /// 调休上班日
    work: HashSet<(i32, u32, u32)>,
    /// 最近一次联网同步成功的时间（yyyy-mm-dd hh:mm）
    fetched: Option<String>,
}

static CAL: OnceLock<RwLock<Cal>> = OnceLock::new();

fn cal() -> &'static RwLock<Cal> {
    CAL.get_or_init(|| {
        let mut c = Cal::default();
        for (y, m, d, name) in BUILTIN_REST.iter() {
            c.rest.insert((*y, *m, *d), (*name).to_string());
        }
        for (y, m, d) in BUILTIN_WORK.iter() {
            c.work.insert((*y, *m, *d));
        }
        RwLock::new(c)
    })
}

/// 同步状态（设置页展示用）
pub const SYNC_IDLE: u8 = 0;
pub const SYNC_RUN: u8 = 1;
pub const SYNC_OK: u8 = 2;
pub const SYNC_FAIL: u8 = 3;

static SYNC_STATE: AtomicU8 = AtomicU8::new(SYNC_IDLE);
static SYNC_RUNNING: AtomicBool = AtomicBool::new(false);
static SYNC_NOTE: OnceLock<RwLock<String>> = OnceLock::new();

fn note() -> &'static RwLock<String> {
    SYNC_NOTE.get_or_init(|| RwLock::new(String::new()))
}

pub fn sync_state() -> u8 {
    SYNC_STATE.load(Ordering::Relaxed)
}

pub fn sync_note() -> String {
    note().read().map(|s| s.clone()).unwrap_or_default()
}

fn set_note(s: &str) {
    if let Ok(mut n) = note().write() {
        *n = s.to_string();
    }
}

// ───────────────────────── 缓存文件 ─────────────────────────

fn cache_path() -> std::path::PathBuf {
    crate::config::data_dir().join("holidays.json")
}

fn load_cache() {
    let Ok(txt) = std::fs::read_to_string(cache_path()) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return };
    let Some(list) = v.get("days").and_then(|d| d.as_array()) else { return };
    let mut days = Vec::new();
    for it in list {
        let Some(date) = it.get("date").and_then(|d| d.as_str()) else { continue };
        let Some(key) = parse_date(date) else { continue };
        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let off = it.get("off").and_then(|o| o.as_bool()).unwrap_or(false);
        days.push(Day { date: key, name, off });
    }
    let fetched = v.get("fetched").and_then(|f| f.as_str()).unwrap_or("").to_string();
    {
        let mut c = cal().write().unwrap();
        merge(&mut c, &days);
        if !fetched.is_empty() {
            c.fetched = Some(fetched);
        }
    }
}

fn save_cache(days: &[Day]) {
    let fetched = clock::now_stamp();
    let list: Vec<serde_json::Value> = days
        .iter()
        .map(|d| {
            serde_json::json!({
                "date": format!("{:04}-{:02}-{:02}", d.date.0, d.date.1, d.date.2),
                "name": d.name,
                "off": d.off,
            })
        })
        .collect();
    let v = serde_json::json!({ "fetched": fetched, "days": list });
    let _ = std::fs::create_dir_all(crate::config::data_dir());
    if let Ok(txt) = serde_json::to_string_pretty(&v) {
        let _ = std::fs::write(cache_path(), txt);
    }
}

fn merge(c: &mut Cal, days: &[Day]) {
    for d in days {
        if d.off {
            c.work.remove(&d.date);
            c.rest.insert(d.date, d.name.clone());
        } else {
            // isOffDay = false 的条目若是普通工作日（节日标记），不影响判定；
            // 只有落在周末的才算「调休上班」
            if is_weekend_key(d.date) {
                c.work.insert(d.date);
                c.rest.remove(&d.date);
            }
        }
    }
}

fn is_weekend_key(k: (i32, u32, u32)) -> bool {
    let dow = weekday_of(k);
    dow == 0 || dow == 6
}

/// 0=周日 .. 6=周六（蔡勒公式，用于缓存里没有 SYSTEMTIME 的场景）
fn weekday_of(k: (i32, u32, u32)) -> u32 {
    let (mut y, m, d) = (k.0, k.1 as i32, k.2 as i32);
    if m < 3 {
        y -= 1;
    }
    let (y, m) = if m < 3 { (y, m + 12) } else { (y, m) };
    let c = y / 100;
    let yy = y % 100;
    let w = (yy + yy / 4 + c / 4 - 2 * c + (26 * (m + 1)) / 10 + d - 1) % 7;
    (((w + 7) % 7) as u32 + 6) % 7 // 转成 0=周日
}

fn parse_date(s: &str) -> Option<(i32, u32, u32)> {
    let mut it = s.split('-');
    let y = it.next()?.parse::<i32>().ok()?;
    let m = it.next()?.parse::<u32>().ok()?;
    let d = it.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

// ───────────────────────── 联网同步 ─────────────────────────

/// 启动时调用：读缓存 + （可选）后台同步当年与明年
pub fn init(cfg: &Salary) {
    load_cache();
    if cfg.online_calendar {
        sync_async();
    }
}

/// 后台拉取官方日历。多次调用会被合并（同一时刻只跑一个）。
pub fn sync_async() {
    if SYNC_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    SYNC_STATE.store(SYNC_RUN, Ordering::Relaxed);
    set_note("正在同步官方节假日…");
    std::thread::spawn(|| {
        let year = clock::local_time().wYear as i32;
        let mut all: Vec<Day> = Vec::new();
        let mut hit = false;
        for y in [year, year + 1] {
            if let Some(days) = fetch_year(y) {
                hit = true;
                all.extend(days);
            }
        }
        if hit && !all.is_empty() {
            save_cache(&all);
            let mut c = cal().write().unwrap();
            merge(&mut c, &all);
            c.fetched = Some(clock::now_stamp());
            drop(c);
            SYNC_STATE.store(SYNC_OK, Ordering::Relaxed);
            set_note(&format!("已同步 {} 条官方安排", all.len()));
            // 岛上的状态徽标由每秒 tick 重绘，无需在此跨线程强制刷新
        } else {
            SYNC_STATE.store(SYNC_FAIL, Ordering::Relaxed);
            set_note("同步失败（已使用内置/缓存日历）");
        }
        SYNC_RUNNING.store(false, Ordering::SeqCst);
    });
}

fn fetch_year(year: i32) -> Option<Vec<Day>> {
    // 1) holiday-cn：直接来自国务院公告的放假/调休表（jsDelivr 两个节点）
    let path = format!("/gh/NateScarlet/holiday-cn@master/{year}.json");
    for host in ["cdn.jsdelivr.net", "fastly.jsdelivr.net"] {
        if let Some(txt) = crate::modules::net::https_get(host, &path) {
            if let Some(days) = parse_holiday_cn(&txt) {
                if !days.is_empty() {
                    return Some(days);
                }
            }
        }
    }
    // 2) 备用：节日/调休接口（同一份公告数据）
    if let Some(txt) = crate::modules::net::https_get("api.jiejiariapi.com", &format!("/v1/holidays/{year}")) {
        if let Some(days) = parse_map(&txt) {
            if !days.is_empty() {
                return Some(days);
            }
        }
    }
    None
}

/// holiday-cn: { "days": [{ "name": "元旦", "date": "2026-01-01", "isOffDay": true }] }
fn parse_holiday_cn(txt: &str) -> Option<Vec<Day>> {
    let v: serde_json::Value = serde_json::from_str(txt).ok()?;
    let list = v.get("days")?.as_array()?;
    let mut out = Vec::new();
    for it in list {
        let date = it.get("date")?.as_str()?;
        let Some(key) = parse_date(date) else { continue };
        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let off = it.get("isOffDay").and_then(|o| o.as_bool()).unwrap_or(false);
        out.push(Day { date: key, name, off });
    }
    Some(out)
}

/// { "2026-01-01": { "name": "元旦", "isOffDay": true } }
fn parse_map(txt: &str) -> Option<Vec<Day>> {
    let v: serde_json::Value = serde_json::from_str(txt).ok()?;
    let obj = v.as_object()?;
    let mut out = Vec::new();
    for (_, it) in obj.iter() {
        let Some(date) = it.get("date").and_then(|d| d.as_str()) else { continue };
        let Some(key) = parse_date(date) else { continue };
        let name = it.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
        let off = it.get("isOffDay").and_then(|o| o.as_bool()).unwrap_or(false);
        out.push(Day { date: key, name, off });
    }
    Some(out)
}

// ───────────────────────── 判定 ─────────────────────────

/// 周一=0 … 周日=6
fn weekday_index(st: &SYSTEMTIME) -> usize {
    ((st.wDayOfWeek as usize) + 6) % 7
}

/// 今天是否工作日
pub fn is_workday(st: &SYSTEMTIME, sal: &Salary) -> bool {
    let key = (st.wYear as i32, st.wMonth as u32, st.wDay as u32);
    let c = cal().read().unwrap();
    if c.rest.contains_key(&key) {
        return false;
    }
    if c.work.contains(&key) {
        return true;
    }
    match sal.custom_weekdays() {
        Some(w) => w[weekday_index(st)],
        None => st.wDayOfWeek != 0 && st.wDayOfWeek != 6,
    }
}

/// 今天的节日（公历固定 / 可计算 / 农历离线表）
pub fn festival(st: &SYSTEMTIME) -> Option<&'static str> {
    let (y, m, d, dow) = (st.wYear as i32, st.wMonth as u32, st.wDay as u32, st.wDayOfWeek);
    for (yy, mm, dd, name) in FEST_LUNAR.iter() {
        if *yy == y && *mm == m && *dd == d {
            return Some(name);
        }
    }
    // 可计算节日
    if m == 5 && dow == 0 && (8..=14).contains(&d) {
        return Some("母亲节");
    }
    if m == 6 && dow == 0 && (15..=21).contains(&d) {
        return Some("父亲节");
    }
    if m == 11 && dow == 4 && (22..=28).contains(&d) {
        return Some("感恩节");
    }
    for (mm, dd, name) in FEST_FIXED.iter() {
        if *mm == m && *dd == d {
            return Some(name);
        }
    }
    None
}

/// 放假表里查今天的假期名（如「国庆节」），没有则 None
pub fn holiday_name(st: &SYSTEMTIME) -> Option<String> {
    let key = (st.wYear as i32, st.wMonth as u32, st.wDay as u32);
    cal().read().ok()?.rest.get(&key).cloned()
}

/// 上班状态
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 非法定工作日
    Rest,
    /// 还没到上班时间
    BeforeWork,
    /// 上午工作中
    Morning,
    /// 午休中
    Lunch,
    /// 下午工作中
    Afternoon,
    /// 已下班
    Off,
}

impl Phase {
    /// 胶囊上的两字提示
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Rest => "休息",
            Phase::BeforeWork => "未上",
            Phase::Morning | Phase::Afternoon => "上班",
            Phase::Lunch => "午休",
            Phase::Off => "下班",
        }
    }

    /// 徽标颜色（0xRRGGBB）
    pub fn color(&self) -> u32 {
        match self {
            Phase::Morning | Phase::Afternoon => 0x3DDC84, // 绿：在岗
            Phase::Lunch => 0xFFB020,                      // 琥珀：午休
            Phase::BeforeWork => 0x6FA8FF,                 // 蓝：还没开工
            Phase::Off => 0x9AA0AE,                        // 灰：已收工
            Phase::Rest => 0x6B707E,                       // 暗灰：休息日
        }
    }
}

/// 根据当前时刻与配置判定状态
pub fn phase_now(st: &SYSTEMTIME, sal: &Salary) -> Phase {
    if !is_workday(st, sal) {
        return Phase::Rest;
    }
    let now = clock::secs_of_day_f(st) / 60.0; // 分钟
    if now < sal.work_start_min() {
        Phase::BeforeWork
    } else if now < sal.lunch_start_min() {
        Phase::Morning
    } else if now < sal.lunch_end_min() {
        Phase::Lunch
    } else if now < sal.work_end_min() {
        Phase::Afternoon
    } else {
        Phase::Off
    }
}

/// 设置页展示用的一句话状态
pub fn status_line(sal: &Salary) -> String {
    let st = clock::local_time();
    let phs = phase_now(&st, sal);
    let mut s = format!("今日：{} {}", clock::date_short(&st), phs.label());
    if let Some(h) = holiday_name(&st) {
        if h != phs.label() {
            s.push_str(&format!(" · {h}假期"));
        }
    }
    if let Some(f) = festival(&st) {
        s.push_str(&format!(" · {f}"));
    }
    let src = {
        let c = cal().read().unwrap();
        match &c.fetched {
            Some(t) => format!("官方日历 {}", t),
            None => "内置日历（未联网同步）".to_string(),
        }
    };
    let extra = match sync_state() {
        SYNC_RUN => "　·　同步中…".to_string(),
        SYNC_FAIL => format!("　·　{}", sync_note()),
        SYNC_OK => format!("　·　{}", sync_note()),
        _ => String::new(),
    };
    format!("{s}　｜　{src}{extra}")
}
