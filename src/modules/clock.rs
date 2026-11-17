//! 时间日期 — 使用 Win32 GetLocalTime，无额外依赖

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

pub fn local_time() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

pub fn hhmm(st: &SYSTEMTIME) -> String {
    format!("{:02}:{:02}", st.wHour, st.wMinute)
}

#[allow(dead_code)]
pub fn hhmmss(st: &SYSTEMTIME) -> String {
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

/// "11月13日 周五"
pub fn date_cn(st: &SYSTEMTIME) -> String {
    let wk = match st.wDayOfWeek {
        0 => "周日",
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        _ => "周六",
    };
    format!("{}月{}日 {}", st.wMonth, st.wDay, wk)
}

/// "11/15 周日" —— 胶囊内的紧凑日期（给状态徽标让出宽度）
pub fn date_short(st: &SYSTEMTIME) -> String {
    let wk = match st.wDayOfWeek {
        0 => "周日",
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        _ => "周六",
    };
    format!("{}/{} {}", st.wMonth, st.wDay, wk)
}

/// "2026-11-13"
pub fn today_str(st: &SYSTEMTIME) -> String {
    format!("{}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

/// "2026-11-13 14:05" —— 同步时间戳
pub fn now_stamp() -> String {
    let st = local_time();
    format!("{}-{:02}-{:02} {:02}:{:02}", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute)
}

/// 当天 0 点起的秒数
pub fn secs_of_day(st: &SYSTEMTIME) -> i64 {
    (st.wHour as i64) * 3600 + (st.wMinute as i64) * 60 + st.wSecond as i64
}

/// 当天 0 点起的秒数（带毫秒小数）——金额按此连续累计，
/// 而不是每秒跳一格，配合更多小数位才能让数字每帧都在变化
pub fn secs_of_day_f(st: &SYSTEMTIME) -> f64 {
    (st.wHour as f64) * 3600.0
        + (st.wMinute as f64) * 60.0
        + st.wSecond as f64
        + (st.wMilliseconds as f64) / 1000.0
}

/// "09:00" -> 540 分钟；解析失败返回 None
pub fn parse_hm(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some(h * 60 + m)
}
