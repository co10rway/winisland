//! 外部日历订阅（ICS / webcal）—— 把钉钉、飞书等日历导出的 .ics 链接接入日程。
//!
//! 只读拉取用户自己配置的订阅地址，解析出「今天」的事件，与本地 data.json 日程合并展示；
//! 不发送任何数据，任何失败都降级为「没有外部日程」，绝不影响本地日程。
//!
//! 已支持的 ICS 子集：
//! - `DTSTART` 本地时间 / `TZID` 参数（按本机时区解释）/ UTC（`Z` 结尾，自动换算本机时区）
//! - 全天事件（`VALUE=DATE`）不支持：本项目日程粒度是 HH:MM
//! - `RRULE` 支持 DAILY / WEEKLY（含 INTERVAL、BYDAY、UNTIL）；MONTHLY / YEARLY / COUNT 暂不支持，
//!   不认识的规则按单次事件处理

use crate::store::ScheduleItem;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

pub const SYNC_IDLE: u8 = 0;
pub const SYNC_RUN: u8 = 1;
pub const SYNC_OK: u8 = 2;
pub const SYNC_FAIL: u8 = 3;

static STATE: AtomicU8 = AtomicU8::new(SYNC_IDLE);
static RUNNING: AtomicBool = AtomicBool::new(false);
static NOTE: OnceLock<RwLock<String>> = OnceLock::new();
static EVENTS: OnceLock<Mutex<Vec<ScheduleItem>>> = OnceLock::new();
static EVENTS_DAY: OnceLock<RwLock<String>> = OnceLock::new();
/// 最近一次成功/尝试同步的 unix 秒（自动刷新节流）
static LAST_SYNC: AtomicI64 = AtomicI64::new(0);
/// 后台线程完成后的重绘请求（redraw 用 thread_local，不能跨线程直接调）
static NEED_REDRAW: AtomicBool = AtomicBool::new(false);

/// RAII：离开作用域（含 panic）即复位 RUNNING 同步标志，
/// 防止同步线程异常退出后日历被永久判定为「正在同步」而停止刷新。
struct RunningReset;
impl Drop for RunningReset {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Relaxed);
    }
}

fn note() -> &'static RwLock<String> {
    NOTE.get_or_init(|| RwLock::new(String::new()))
}

fn events() -> &'static Mutex<Vec<ScheduleItem>> {
    EVENTS.get_or_init(|| Mutex::new(Vec::new()))
}

fn events_day() -> &'static RwLock<String> {
    EVENTS_DAY.get_or_init(|| RwLock::new(String::new()))
}

pub fn sync_state() -> u8 {
    STATE.load(Ordering::Relaxed)
}

pub fn sync_note() -> String {
    note().read().map(|s| s.clone()).unwrap_or_default()
}

/// 后台线程完成 → 由 UI 线程每秒取走，触发一次重绘
pub fn take_dirty() -> bool {
    NEED_REDRAW.swap(false, Ordering::Relaxed)
}

fn day_str(st: &SYSTEMTIME) -> String {
    format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn cache_path() -> std::path::PathBuf {
    crate::config::data_dir().join("schedule_ics.json")
}

/// 启动时载入缓存（仅当缓存日期就是今天）
pub fn init() {
    let today = day_str(&crate::modules::clock::local_time());
    let Ok(txt) = std::fs::read_to_string(cache_path()) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { return };
    if v.get("day").and_then(|d| d.as_str()) != Some(today.as_str()) {
        return;
    }
    let Some(arr) = v.get("items").and_then(|x| x.as_array()) else { return };
    let items: Vec<ScheduleItem> = arr
        .iter()
        .filter_map(|it| {
            let time = it.get("time")?.as_str()?.to_string();
            let title = it.get("title")?.as_str()?.to_string();
            Some(ScheduleItem { time, title, ..Default::default() })
        })
        .collect();
    *events().lock().unwrap_or_else(|e| e.into_inner()) = items;
    *events_day().write().unwrap_or_else(|e| e.into_inner()) = today;
}

/// 今天的订阅日程（跨天自动清空）
pub fn today_items() -> Vec<ScheduleItem> {
    let today = day_str(&crate::modules::clock::local_time());
    let mut ev = events().lock().unwrap_or_else(|e| e.into_inner());
    if events_day().read().unwrap_or_else(|e| e.into_inner()).as_str() != today {
        ev.clear();
        return Vec::new();
    }
    ev.clone()
}

/// 离屏预览（--demo）注入演示日程；正常运行不会调用
pub fn inject_demo(items: Vec<ScheduleItem>) {
    let today = day_str(&crate::modules::clock::local_time());
    *events().lock().unwrap_or_else(|e| e.into_inner()) = items;
    *events_day().write().unwrap_or_else(|e| e.into_inner()) = today;
}

/// 自动刷新节流：到间隔就同步所有已启用的来源（在 UI 线程调用，只做判断）
pub fn auto_tick(sub: &crate::config::ScheduleSub) {
    if RUNNING.load(Ordering::Relaxed) || !any_configured(sub) {
        return;
    }
    let wait = (sub.refresh_minutes.max(1) as i64) * 60;
    if unix_now() - LAST_SYNC.load(Ordering::Relaxed) < wait {
        return;
    }
    sync_all_async(sub);
}

fn any_configured(sub: &crate::config::ScheduleSub) -> bool {
    sub.feishu.configured() || sub.dingtalk.configured() || sub.wecom.configured()
}

/// 三个来源（固定顺序：飞书 / 钉钉 / 企业微信）
fn sources(sub: &crate::config::ScheduleSub) -> [(&'static str, crate::config::CalSource); 3] {
    [
        ("飞书", sub.feishu.clone()),
        ("钉钉", sub.dingtalk.clone()),
        ("企业微信", sub.wecom.clone()),
    ]
}

/// 后台同步所有已启用的来源，合并去重后写入缓存（重复调用直接忽略）
pub fn sync_all_async(sub: &crate::config::ScheduleSub) {
    if RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    let list: Vec<(&'static str, crate::config::CalSource)> =
        sources(sub).into_iter().filter(|(_, s)| s.configured()).collect();
    std::thread::spawn(move || {
        // RAII：无论线程正常结束还是 panic，离开作用域时都复位 RUNNING。
        // 原先在线程末尾手动 store(false)，一旦中间 panic 就会永久卡死在 RUNNING，
        // 导致日历从此不再同步。这里用作兜底。
        let _reset = RunningReset;
        STATE.store(SYNC_RUN, Ordering::Relaxed);
        set_note("正在同步外部日历…");
        let st = crate::modules::clock::local_time();
        let today = (st.wYear as i64, st.wMonth as i64, st.wDay as i64);
        let off = utc_offset_minutes();

        let mut items: Vec<ScheduleItem> = Vec::new();
        let mut ok = 0usize;
        let mut errs: Vec<String> = Vec::new();
        for (name, src) in &list {
            let res = if !src.ics_url.trim().is_empty() {
                fetch_ics(&src.ics_url, off, today)
            } else {
                fetch_dav(&src.dav_server, &src.dav_user, &src.dav_pass, off, today)
            };
            match res {
                Ok(v) => {
                    ok += 1;
                    items.extend(v);
                }
                Err(e) => errs.push(format!("{name}：{e}")),
            }
        }
        items.sort_by(|a, b| a.time.cmp(&b.time));
        items.dedup_by(|a, b| a.time == b.time && a.title == b.title);

        LAST_SYNC.store(unix_now(), Ordering::Relaxed);
        RUNNING.store(false, Ordering::Relaxed);
        if ok == 0 {
            STATE.store(SYNC_FAIL, Ordering::Relaxed);
            let msg = if errs.is_empty() { "还没有配置外部日历".to_string() } else { errs.join("；") };
            set_note(&msg);
        } else {
            STATE.store(SYNC_OK, Ordering::Relaxed);
            let mut n = format!("已同步 {} 条今日日程", items.len());
            if !errs.is_empty() {
                n.push('；');
                n.push_str(&errs.join("；"));
            }
            set_note(&n);
            save_cache(&st, &items);
            *events().lock().unwrap_or_else(|e| e.into_inner()) = items;
            *events_day().write().unwrap_or_else(|e| e.into_inner()) = day_str(&st);
        }
        NEED_REDRAW.store(true, Ordering::Relaxed);
    });
}

fn set_note(s: &str) {
    if let Ok(mut n) = note().write() {
        *n = s.to_string();
    }
}

fn fetch_ics(url: &str, off: i64, today: (i64, i64, i64)) -> Result<Vec<ScheduleItem>, String> {
    let (host, path) = split_url(url)?;
    let body = crate::modules::net::https_get(&host, &path)
        .ok_or_else(|| "订阅拉取失败（检查链接与网络）".to_string())?;
    if !body.contains("BEGIN:VCALENDAR") {
        return Err("链接返回的不是 ICS 日历".to_string());
    }
    Ok(parse_ics(&body, off, today))
}

/// webcal:// 视为 https://；不支持明文 http
fn split_url(url: &str) -> Result<(String, String), String> {
    let url = if let Some(rest) = url.strip_prefix("webcal://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    };
    if url.starts_with("http://") {
        return Err("订阅地址需要 https://（不支持明文 http）".to_string());
    }
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "订阅地址要以 https:// 或 webcal:// 开头".to_string())?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if host.is_empty() {
        return Err("订阅地址缺少主机名".to_string());
    }
    Ok((host.to_string(), path))
}

// ───────────────────────── 日历时间换算 ─────────────────────────

/// 公历 → 天数（自 1970-01-01）
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 本机时区相对 UTC 的分钟偏移（按当前时刻）
fn utc_offset_minutes() -> i64 {
    let l = unsafe { GetLocalTime() };
    let u = unsafe { GetSystemTime() };
    let lm = days_from_civil(l.wYear as i64, l.wMonth as i64, l.wDay as i64) * 1440
        + l.wHour as i64 * 60
        + l.wMinute as i64;
    let um = days_from_civil(u.wYear as i64, u.wMonth as i64, u.wDay as i64) * 1440
        + u.wHour as i64 * 60
        + u.wMinute as i64;
    lm - um
}

// ───────────────────────── ICS 解析 ─────────────────────────

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') | Some('N') => out.push(' '),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `20261115T093000Z` / `20261115T093000` → (y,m,d,hh,mm,is_utc)；全天事件返回 None
fn parse_dt(v: &str) -> Option<(i64, i64, i64, i64, i64, bool)> {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() < 15 || b[8] != b'T' {
        return None; // 含 VALUE=DATE 的全天事件
    }
    // 只用字节索引 + 数字校验（不再对 str 做 v[a..b] 切片）：
    // 不可信 ICS 输入若在日期段混入多字节字符，按字符边界切片会直接 panic。
    let rd = |s: &[u8]| -> Option<i64> {
        let mut n = 0i64;
        for &c in s {
            if !c.is_ascii_digit() {
                return None;
            }
            n = n * 10 + (c - b'0') as i64;
        }
        Some(n)
    };
    let y = rd(&b[0..4])?;
    let m = rd(&b[4..6])?;
    let d = rd(&b[6..8])?;
    let hh = rd(&b[9..11])?;
    let mm = rd(&b[11..13])?;
    Some((y, m, d, hh, mm, v.ends_with('Z')))
}

/// RRULE 是否让该事件落在今天
fn occurs(days: i64, rrule: &Option<String>, today_days: i64, dow_today: i64) -> bool {
    let Some(rr) = rrule else { return days == today_days };
    let mut freq = "";
    let mut interval = 1i64;
    let mut until_days: Option<i64> = None;
    let mut byday: Vec<i64> = Vec::new();
    for part in rr.split(';') {
        let Some((k, v)) = part.split_once('=') else { continue };
        match k {
            "FREQ" => freq = v,
            "INTERVAL" => interval = v.parse().unwrap_or(1).max(1),
            "UNTIL" => {
                if let Some((y, m, d, _, _, _)) = parse_dt(v) {
                    until_days = Some(days_from_civil(y, m, d));
                }
            }
            "BYDAY" => {
                byday = v
                    .split(',')
                    .filter_map(|s| match s.trim() {
                        "MO" => Some(0),
                        "TU" => Some(1),
                        "WE" => Some(2),
                        "TH" => Some(3),
                        "FR" => Some(4),
                        "SA" => Some(5),
                        "SU" => Some(6),
                        _ => None,
                    })
                    .collect();
            }
            _ => {}
        }
    }
    if let Some(u) = until_days {
        if today_days > u {
            return false;
        }
    }
    let diff = today_days - days;
    if diff < 0 {
        return false;
    }
    match freq {
        "DAILY" => diff % interval == 0,
        "WEEKLY" => {
            let by = if byday.is_empty() { vec![(days + 3).rem_euclid(7)] } else { byday };
            // 以「周一开始的那一周」为锚点，按整周差算间隔，避免按天数分桶错位
            let start_dow = (days + 3).rem_euclid(7);
            let start_week = days - start_dow;
            let today_week = today_days - dow_today;
            let weeks = (today_week - start_week) / 7;
            weeks >= 0 && weeks % interval == 0 && by.contains(&dow_today)
        }
        // MONTHLY / YEARLY / COUNT 暂不支持
        _ => false,
    }
}

fn parse_ics(body: &str, off_min: i64, today: (i64, i64, i64)) -> Vec<ScheduleItem> {
    // 展开折行（RFC5545：以空格/制表符开头的行属于上一行）
    let mut lines: Vec<String> = Vec::new();
    for raw in body.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                last.push_str(raw[1..].trim_end());
            }
        } else {
            lines.push(raw.trim_end().to_string());
        }
    }

    let today_days = days_from_civil(today.0, today.1, today.2);
    let dow_today = (today_days + 3).rem_euclid(7); // 0=周一
    let mut out: Vec<ScheduleItem> = Vec::new();
    let mut in_ev = false;
    let mut sum = String::new();
    let mut dt: Option<(i64, i64, i64, i64, i64, bool)> = None;
    let mut rrule: Option<String> = None;
    let mut cancelled = false;

    for line in &lines {
        match line.as_str() {
            "BEGIN:VEVENT" => {
                in_ev = true;
                sum.clear();
                dt = None;
                rrule = None;
                cancelled = false;
                continue;
            }
            "END:VEVENT" => {
                if in_ev
                    && !cancelled && !sum.is_empty() {
                        if let Some((y, m, d, hh, mm, is_utc)) = dt {
                            let mut days = days_from_civil(y, m, d);
                            let mut mins = hh * 60 + mm;
                            if is_utc {
                                // UTC → 本机时区：加时区偏移
                                mins += off_min;
                            }
                            // UTC 换算可能跨天
                            while mins < 0 {
                                mins += 1440;
                                days -= 1;
                            }
                            while mins >= 1440 {
                                mins -= 1440;
                                days += 1;
                            }
                            if occurs(days, &rrule, today_days, dow_today) {
                                out.push(ScheduleItem {
                                    time: format!("{:02}:{:02}", mins / 60, mins % 60),
                                    title: sum.clone(),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                in_ev = false;
                continue;
            }
            _ => {}
        }
        if !in_ev {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let ku = k.to_ascii_uppercase();
            if ku.starts_with("SUMMARY") {
                sum = unescape(v);
            } else if ku.starts_with("DTSTART") {
                dt = parse_dt(v);
            } else if ku.starts_with("RRULE") {
                rrule = Some(v.to_ascii_uppercase());
            } else if ku == "STATUS" && v.eq_ignore_ascii_case("CANCELLED") {
                cancelled = true;
            }
        }
        if out.len() >= 100 {
            break;
        }
    }

    out.sort_by(|a, b| a.time.cmp(&b.time));
    out.dedup_by(|a, b| a.time == b.time && a.title == b.title);
    out
}

// ───────────────────────── 缓存 ─────────────────────────

fn save_cache(st: &SYSTEMTIME, items: &[ScheduleItem]) {
    let v = serde_json::json!({ "day": day_str(st), "items": items });
    let _ = std::fs::write(cache_path(), v.to_string());
}

// ───────────────────── CalDAV（飞书 / 钉钉 / 企业微信）─────────────────────
//
// 三家都不给通用 ICS 导出（钉钉只有团队日历的分享链接），官方都提供 CalDAV：
//   飞书    caldav.feishu.cn        桌面端 设置 → 日历 → CalDAV 同步 → 生成密码（只读）
//   钉钉    calendar.dingtalk.com   日历设置 → 同步到其它日历 → 获取CalDAV账号
//   企业微信 caldav.wecom.work      手机端 工作台 → 日程 → … → 同步至其他日历
//     （页面写的 wecom.work 是手机 Exchange 用的，CalDAV 要用 caldav 子域，dav_host 自动映射）
// 注意：企业微信那个页面每打开一次就重新生成密码，旧密码可能立刻失效。
//
// 流程：PROPFIND 查 principal → 查 calendar-home-set → Depth:1 列出日历集合
//       → 对每个集合 REPORT 今天的时间范围 → 复用 parse_ics 解析 VEVENT。

const PF_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;
const PF_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><C:calendar-home-set xmlns:C="urn:ietf:params:xml:ns:caldav"/></D:prop></D:propfind>"#;
const PF_LIST: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:displayname/></D:prop></D:propfind>"#;

fn fetch_dav(
    server: &str,
    user: &str,
    pass: &str,
    off: i64,
    today: (i64, i64, i64),
) -> Result<Vec<ScheduleItem>, String> {
    let (user, pass) = (user.trim(), pass.trim());
    if user.is_empty() || pass.is_empty() {
        return Err("未填 CalDAV 账号或密码".to_string());
    }
    let (host, prefix) = dav_host(server)?;
    let auth = format!("Basic {}", b64(format!("{user}:{pass}").as_bytes()));

    // 服务器字段里带的路径（如 https://calendar.dingtalk.com/dav/）要先补到构造出的路径前；
    // 服务器自己回的 href 已是完整路径，原样使用。
    let abs = |p: &str| -> String {
        if prefix.is_empty() || p.starts_with(&prefix) {
            p.to_string()
        } else if p == "/" {
            format!("{prefix}/")
        } else {
            format!("{prefix}{p}")
        }
    };

    let days = days_from_civil(today.0, today.1, today.2);
    let start = utc_stamp(days, 0, off);
    let end = utc_stamp(days + 1, 0, off);

    // 1) 标准发现：根路径 PROPFIND 拿 current-user-principal → calendar-home-set。
    //    钉钉的 DAV 根是 /dav/{user}/，对 / 发 PROPFIND 直接 405，所以下面必须能兜底。
    let mut homes: Vec<String> = Vec::new();
    let root = abs("/");
    if let Some(p) = dav_propfind(&host, &auth, &root, "0", PF_PRINCIPAL)
        .ok()
        .and_then(|t| xml_all(&t, "current-user-principal").into_iter().next())
        .and_then(|inner| xml_all(&inner, "href").into_iter().next())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
    {
        if let Some(h) = dav_propfind(&host, &auth, &p, "0", PF_HOME)
            .ok()
            .and_then(|t| xml_all(&t, "calendar-home-set").into_iter().next())
            .and_then(|inner| xml_all(&inner, "href").into_iter().next())
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
        {
            homes.push(h);
        }
    }
    // 2) 约定路径兜底：钉钉 /dav/{user}/、飞书/企微 /calendar/{user}/ 与 /calendar/
    //    （企微的 calendar-home-set 回 /calendar/，真正的日历列在它的子项里）、旧版 /principals/
    for cand in [
        abs(&format!("/dav/{user}/")),
        abs(&format!("/calendar/{}/", user.replace('@', "%40"))),
        "/calendar/".to_string(),
        abs(&format!("/principals/{user}/")),
        root.clone(),
    ] {
        if !homes.contains(&cand) {
            homes.push(cand);
        }
    }

    // 3) 逐个候选 home 做 Depth:1，列出日历集合（resourcetype 里带 calendar 的才是日历）
    let mut cals: Vec<String> = Vec::new();
    let mut list_err: Option<String> = None;
    for home in &homes {
        let list = match dav_propfind(&host, &auth, home, "1", PF_LIST) {
            Ok(t) => t,
            Err(e) => {
                list_err.get_or_insert(e);
                continue;
            }
        };
        cals.clear();
        for resp in xml_all(&list, "response") {
            let rt = xml_all(&resp, "resourcetype").join("").to_ascii_lowercase();
            // 企微的 principal 集合（/calendar/{user}/）的 resourcetype 里也带 calendar，
            // 但它本身不是日历，REPORT 它只会白出错误——真正的日历是 /calendar/{id}/
            if !rt.contains("calendar") || rt.contains("principal") {
                continue;
            }
            if let Some(h) = xml_all(&resp, "href").into_iter().next() {
                let h = h.trim().to_string();
                if h.starts_with('/') && !cals.contains(&h) {
                    cals.push(h);
                }
            }
        }
        if !cals.is_empty() {
            break;
        }
    }
    if cals.is_empty() {
        return Err(match list_err {
            Some(e) => e,
            None => "服务器上没找到可读日历（检查账号权限）".to_string(),
        });
    }
    cals.truncate(8);

    // 4) 逐个日历取今天的事件。两种服务器行为都要兼容：
    //    标准实现（钉钉 / 企业微信）：calendar-query 直接在 propstat 里回 calendar-data；
    //    飞书：query 只回命中的 <href>（且不以 .ics 结尾），内容必须用 calendar-multiget 取。
    let mut ics = String::new();
    let mut last_err: Option<String> = None;
    for cal in &cals {
        let xml = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="{start}" end="{end}"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
        );
        let text = match dav_request(&host, &auth, cal, "1", "REPORT", &xml) {
            Ok(t) => t,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        // 只认真正带 VCALENDAR 的内容：飞书对 calendar-data 回 404（可能给空标签），
        // 空内容必须落到下面的 multiget 分支，否则会把有事件的日历当成没事件。
        let direct: Vec<String> = xml_all(&text, "calendar-data")
            .into_iter()
            .map(|c| xml_unescape(&c))
            .filter(|c| c.contains("BEGIN:VCALENDAR"))
            .collect();
        if !direct.is_empty() {
            for cd in direct {
                ics.push_str(&cd);
                ics.push_str("\r\n");
            }
            if ics.len() > 2_000_000 {
                break;
            }
            continue;
        }
        let hrefs: Vec<String> = xml_all(&text, "href")
            .into_iter()
            .map(|h| h.trim().to_string())
            .filter(|h| h.starts_with('/') && h != cal)
            .collect();
        if hrefs.is_empty() {
            continue;
        }
        // multiget 分批（每批 50 个），内容全部拼起来一次解析
        for chunk in hrefs.chunks(50) {
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><C:calendar-data/></D:prop>{} </C:calendar-multiget>"#,
                chunk.iter().map(|h| format!("<D:href>{h}</D:href>")).collect::<String>()
            );
            match dav_request(&host, &auth, cal, "0", "REPORT", &body) {
                Ok(t) => {
                    for cd in xml_all(&t, "calendar-data") {
                        ics.push_str(&xml_unescape(&cd));
                        ics.push_str("\r\n");
                        if ics.len() > 2_000_000 {
                            break;
                        }
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }
        if ics.len() > 2_000_000 {
            break;
        }
    }
    let items = parse_ics(&ics, off, today);
    if items.is_empty() {
        // 账号通了、但今天真的没有会 —— 这和失败要区分开
        if let Some(e) = last_err {
            return Err(e);
        }
    }
    Ok(items)
}

/// 拆服务器字段 → (host, prefix)。prefix 形如 "" 或 "/dav"（不带结尾斜杠）。
/// 允许填 "calendar.dingtalk.com"、"calendar.dingtalk.com/dav"、
/// "https://calendar.dingtalk.com/dav/" 三种写法。
fn dav_host(server: &str) -> Result<(String, String), String> {
    let s = server.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err("未填 CalDAV 服务器".to_string());
    }
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let mut it = s.splitn(2, '/');
    let mut host = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("").trim_matches('/').to_string();
    if host.is_empty() || !host.contains('.') {
        return Err("服务器地址看起来不对（示例：calendar.dingtalk.com）".to_string());
    }
    // 企业微信页面上写的 wecom.work 是手机 Exchange 用的网关，对 CalDAV 请求
    // 一律回 403；CalDAV 必须走 caldav 子域（官方 Mac 教程写的就是它）。
    // 用户直接填页面上的 wecom.work 也能用，这里自动补上。
    if host == "wecom.work" {
        host = "caldav.wecom.work".to_string();
    }
    let prefix = if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    };
    Ok((host, prefix))
}

fn dav_propfind(host: &str, auth: &str, path: &str, depth: &str, body: &str) -> Result<String, String> {
    dav_request(host, auth, path, depth, "PROPFIND", body)
}

fn dav_request(host: &str, auth: &str, path: &str, depth: &str, method: &str, body: &str) -> Result<String, String> {
    let hdrs = [
        ("Authorization", auth.to_string()),
        ("Depth", depth.to_string()),
    ];
    match crate::modules::net::https_req(host, path, method, &hdrs, Some(body)) {
        Some((200 | 207, t)) => Ok(t),
        Some((401, _)) => Err("账号或密码不对（企业微信的密码要去「同步至其他日历」页面重新取一次）".to_string()),
        Some((403, _)) => Err("服务器拒绝访问（检查服务器地址与密码：企微密码每次打开获取页面都会重新生成）".to_string()),
        Some((404, _)) => Err("日历路径不存在（检查服务器地址，或账号格式 you@corp.wecom.work）".to_string()),
        Some((code, _)) => Err(format!("CalDAV 服务器返回 {code}")),
        None => Err("连不上 CalDAV 服务器（检查服务器地址与网络）".to_string()),
    }
}

/// 收集所有 `<[ns:]local ...>…</[ns:]local>` 的内部文本（不处理同名嵌套）
fn xml_all(src: &str, local: &str) -> Vec<String> {
    let low = src.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < low.len() {
        let Some(rel) = low[i..].find('<') else { break };
        let at = i + rel;
        let rest_at = at + 1;
        if low[rest_at..].starts_with('/') {
            i = rest_at;
            continue;
        }
        let Some(gt) = low[rest_at..].find('>') else { break };
        let tag = &low[rest_at..rest_at + gt];
        let content_start = rest_at + gt + 1;
        let self_close = tag.trim_end().ends_with('/');
        let name = tag.split_whitespace().next().unwrap_or("");
        let lname = name.rsplit(':').next().unwrap_or(name);
        if !self_close && lname.eq_ignore_ascii_case(local) {
            let close_pat = format!("</{}", name);
            if let Some(c) = low[content_start..].find(&close_pat) {
                out.push(src[content_start..content_start + c].to_string());
                i = content_start + c;
                continue;
            }
        }
        i = content_start;
    }
    out
}

/// XML 反转义：数字字符引用（十进制/十六进制、位数不定，如 &#13; &#xD; &#xA;）
/// + 常用命名实体。飞书的 calendar-data 用最少位数的 &#xD;&#xA; 分行，必须通用解析。
fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("&#") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let (radix, digits) = match after.as_bytes().first() {
            Some(b'x') | Some(b'X') => (16u32, &after[1..]),
            _ => (10, after),
        };
        let end = digits
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(digits.len());
        if end > 0 {
            let cp = u32::from_str_radix(&digits[..end], radix).unwrap_or(0xFFFD);
            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
            rest = &digits[end..];
            if rest.starts_with(';') {
                rest = &rest[1..];
            }
        } else {
            out.push_str("&#");
            rest = after;
        }
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Base64 编码（Basic 认证头）
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b0 = c[0] as u32;
        let b1 = *c.get(1).unwrap_or(&0) as u32;
        let b2 = *c.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 天数 → 公历（Howard Hinnant 算法）
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 本地「第 days 天的第 minutes 分钟」→ CalDAV 的 UTC 时间戳（YYYYMMDDTHHMMSSZ）
fn utc_stamp(days: i64, minutes: i64, off_min: i64) -> String {
    let total = days * 1440 + minutes - off_min;
    let d = total.div_euclid(1440);
    let m = total.rem_euclid(1440);
    let (y, mo, da) = civil_from_days(d);
    format!("{:04}{:02}{:02}T{:02}{:02}00Z", y, mo, da, m / 60, m % 60)
}


#[cfg(test)]
mod tests {
    use super::*;

    // 本机时区 UTC+8，今天 = 2026-11-15（周日）
    const OFF: i64 = 480;
    const TODAY: (i64, i64, i64) = (2026, 11, 15);

    fn ics(events: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\n{events}\r\nEND:VEVENT\r\nEND:VCALENDAR")
    }

    #[test]
    fn utc_event_converts_to_local_today() {
        // 01:30Z = 09:30 北京时间
        let out = parse_ics(&ics("DTSTART:20261115T013000Z\r\nSUMMARY:周会"), OFF, TODAY);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, "09:30");
        assert_eq!(out[0].title, "周会");
    }

    #[test]
    fn tzid_event_keeps_local_time() {
        let out = parse_ics(&ics("DTSTART;TZID=Asia/Shanghai:20261115T143000\r\nSUMMARY:评审"), OFF, TODAY);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, "14:30");
    }

    #[test]
    fn other_day_and_allday_skipped() {
        let out = parse_ics(
            &ics("DTSTART:20261116T090000\r\nSUMMARY:明天\r\n\r\nDTSTART;VALUE=DATE:20261115\r\nSUMMARY:全天"),
            OFF,
            TODAY,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn daily_rrule_hits_today() {
        let out = parse_ics(&ics("DTSTART:20261111T090000\r\nRRULE:FREQ=DAILY;INTERVAL=2\r\nSUMMARY:隔天站会"), OFF, TODAY);
        // 11-11 起隔天：11,13,15 → 今天（15）应命中
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, "09:00");
    }

    #[test]
    fn weekly_byday_hits_today() {
        // 起始 11-02（周一），每周一/周三/周五
        let out = parse_ics(
            &ics("DTSTART:20261102T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR\r\nSUMMARY:锻炼"),
            OFF,
            TODAY,
        );
        // 11-15 是周日，不在 BYDAY 里
        assert!(out.is_empty());
        let out2 = parse_ics(
            &ics("DTSTART:20261109T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,FR;UNTIL=20261231T235959Z\r\nSUMMARY:锻炼"),
            OFF,
            TODAY,
        );
        assert!(out2.is_empty()); // 周日也不命中
        let out3 = parse_ics(
            &ics("DTSTART:20261109T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,WE,SU\r\nSUMMARY:锻炼"),
            OFF,
            TODAY,
        );
        assert_eq!(out3.len(), 1);
    }

    #[test]
    fn folded_summary_and_escapes() {
        let out = parse_ics(
            &ics("DTSTART:20261115T090000\r\nSUMMARY:第一行第二\n 行\\,带逗号"),
            OFF,
            TODAY,
        );
        assert_eq!(out[0].title, "第一行第二行,带逗号");
    }

    #[test]
    fn cancelled_and_webcal_url() {
        let out = parse_ics(&ics("DTSTART:20261115T090000\r\nSUMMARY:取消的会\r\nSTATUS:CANCELLED"), OFF, TODAY);
        assert!(out.is_empty());
        let (host, path) = split_url("webcal://cal.example.com/x.ics?token=abc").unwrap();
        assert_eq!(host, "cal.example.com");
        assert_eq!(path, "/x.ics?token=abc");
        assert!(split_url("http://a.com/x.ics").is_err());
    }

    #[test]
    fn utc_crosses_midnight_to_next_day() {
        // 17:30Z = 次日 01:30 → 不是今天
        let out = parse_ics(&ics("DTSTART:20261115T173000Z\r\nSUMMARY:跨天"), OFF, TODAY);
        assert!(out.is_empty());
    }

    #[test]
    fn dav_helpers() {
        assert_eq!(b64(b"a:b"), "YTpi");
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(b""), "");
        // 本地 2026-11-15 00:00（UTC+8）→ UTC 2026-11-14 16:00
        let d = days_from_civil(2026, 11, 15);
        assert_eq!(utc_stamp(d, 0, 480), "20261114T160000Z");
        assert_eq!(utc_stamp(d + 1, 0, 480), "20261115T160000Z");

        // 典型 multistatus：只挑出带 calendar 的集合
        let xml = concat!(
            r#"<D:multistatus xmlns:D="DAV:">"#,
            r#"<D:response><D:href>/calendar/me/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>"#,
            r#"<D:response><D:href>/calendar/me/main/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar xmlns:C="urn:ietf:params:xml:ns:caldav"/></D:resourcetype></D:prop></D:propstat></D:response>"#,
            r#"</D:multistatus>"#
        );
        let resps = xml_all(xml, "response");
        assert_eq!(resps.len(), 2);
        let cals: Vec<String> = resps
            .iter()
            .filter(|r| xml_all(r, "resourcetype").join("").to_ascii_lowercase().contains("calendar"))
            .filter_map(|r| xml_all(r, "href").into_iter().next())
            .map(|h| h.trim().to_string())
            .collect();
        assert_eq!(cals, vec!["/calendar/me/main/".to_string()]);

        // CalDAV 返回的 calendar-data 是 XML 转义过、用 &#13;&#10; 分行的
        assert_eq!(
            xml_unescape("BEGIN:VEVENT&#13;&#10;SUMMARY:a&amp;b"),
            "BEGIN:VEVENT\r\nSUMMARY:a&b"
        );
        // 飞书用最少位数的十六进制 &#xD;&#xA; 分行（回归：0.4.9 因不认这种写法整段 ICS 解析为 0 条）
        assert_eq!(
            xml_unescape("A&#xD;&#xA;B&#x0D;&#x0A;C&#13;&#10;D"),
            "A\r\nB\r\nC\r\nD"
        );
        assert_eq!(
            dav_host("https://caldav.wecom.work/").unwrap(),
            ("caldav.wecom.work".to_string(), String::new())
        );
        assert_eq!(
            dav_host("caldav.wecom.work").unwrap(),
            ("caldav.wecom.work".to_string(), String::new())
        );
        // 企微页面写的 wecom.work 是手机 Exchange 网关（CalDAV 一律 403），
        // 必须自动映射到 caldav 子域（回归：0.4.17 用户填 wecom.work 后同步报 403）
        assert_eq!(
            dav_host("wecom.work").unwrap(),
            ("caldav.wecom.work".to_string(), String::new())
        );
        // 钉钉的 DAV 根带路径，必须保留下来（丢路径就会对 / 发 PROPFIND 报 405）
        assert_eq!(
            dav_host("https://calendar.dingtalk.com/dav/").unwrap(),
            ("calendar.dingtalk.com".to_string(), "/dav".to_string())
        );
        assert_eq!(
            dav_host("calendar.dingtalk.com/dav").unwrap(),
            ("calendar.dingtalk.com".to_string(), "/dav".to_string())
        );
        assert!(dav_host("  ").is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::config::CalSource;

    /// 真实联调：需要 WI_DAV_HOST/WI_DAV_USER/WI_DAV_PASS 环境变量，默认 ignore
    #[test]
    #[ignore]
    fn feishu_live_dav() {
        let host = std::env::var("WI_DAV_HOST").unwrap_or_default();
        let user = std::env::var("WI_DAV_USER").unwrap_or_default();
        let pass = std::env::var("WI_DAV_PASS").unwrap_or_default();
        assert!(!pass.is_empty(), "缺少 WI_DAV_* 环境变量");
        let src = CalSource { on: true, ics_url: String::new(), dav_server: host, dav_user: user, dav_pass: pass };
        let st = crate::modules::clock::local_time();
        let today = (st.wYear as i64, st.wMonth as i64, st.wDay as i64);
        let items = fetch_dav(&src.dav_server, &src.dav_user, &src.dav_pass, 480, today)
            .unwrap_or_else(|e| panic!("fetch_dav 失败: {e}"));
        println!("拉到 {} 条今日日程", items.len());
        for it in &items {
            println!("  {} {}", it.time, it.title);
        }
        assert!(!items.is_empty(), "至少应有 1 条");
    }
}
