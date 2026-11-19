//! 盯盘助手 — A 股实时行情（腾讯公开行情接口）、自选 + 持仓 + 今日盈亏 + 涨跌提醒。
//!
//! - 自选/持仓来自 `%APPDATA%\WinIsland\watchlist.txt`，每行 `代码 名称 成本价 数量`。
//! - 后台线程按配置间隔拉取腾讯行情（GBK→UTF-8，经 encoding_rs），写入内存快照。
//! - 只在交易时段轮询（可关闭）；价格涨跌幅超过阈值时触发一次提醒（当天不重复）。
//! 本模块只读行情，不提交任何订单；不采集用户信息。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// 单只股票的行情快照。
#[derive(Clone, Debug)]
#[allow(dead_code)] // 部分字段为后续看板（今开/最高/最低）预留
pub struct Quote {
    pub code: String,
    pub name: String, // UTF-8
    pub price: f64,
    pub prev_close: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub chg: f64,
    pub chg_pct: f64,
}

/// 一条自选 / 持仓（来自 watchlist.txt）。
#[derive(Clone, Debug)]
pub struct Item {
    pub code: String,
    pub name: String, // 用户填的名称（可为空，自动取接口名称）
    pub cost: f64,
    pub qty: f64,
}

struct Shared {
    quotes: Mutex<Vec<Quote>>,
    dirty: AtomicBool,
    last_ts: Mutex<i64>,
}

static SHARED: OnceLock<Shared> = OnceLock::new();
fn shared() -> &'static Shared {
    SHARED.get_or_init(|| Shared {
        quotes: Mutex::new(Vec::new()),
        dirty: AtomicBool::new(false),
        last_ts: Mutex::new(0),
    })
}

/// 已触发提醒的 key（code|日期）+ 已对应的日期，跨天自然失效
static ALERT_KEY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
fn alerted() -> &'static Mutex<HashSet<String>> {
    ALERT_KEY.get_or_init(|| Mutex::new(HashSet::new()))
}

// ───────────────────────── 自选/持仓 读写 ─────────────────────────

/// 读取盯盘数据文件，按行解析成 Item 列表。解析失败 / 空返回空列表。
pub fn read_items() -> Vec<Item> {
    let p = crate::config::watchlist_path();
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in txt.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut toks = line.split_whitespace();
        let Some(raw) = toks.next() else { continue };
        // 校验为 6 位代码；Item.code 统一存纯 6 位（不带交易所前缀），
        // 展示与 quote_for 匹配都以它为准
        let Some(full) = normalize_code(raw) else { continue };
        let dig: String = raw.trim().chars().filter(|c| c.is_ascii_digit()).collect();
        let code = if !dig.is_empty() { dig } else { full };
        // 剩余 token：非数字 → 名称；数字依次 → 成本价、数量
        let mut name = String::new();
        let mut nums: Vec<f64> = Vec::new();
        for t in toks {
            if let Ok(v) = t.parse::<f64>() {
                nums.push(v);
            } else if name.is_empty() {
                name.push_str(t);
            }
        }
        let cost = nums.first().copied().unwrap_or(0.0);
        let qty = nums.get(1).copied().unwrap_or(0.0);
        out.push(Item { code, name, cost, qty });
    }
    out
}

/// 将自选列表写回 watchlist.txt（每行：代码 名称 成本价 数量）。
/// 名称由编辑窗口按代码自动补全后写入；持仓为空时只写代码 + 名称。
/// 传入的 code 可为 6 位或带前缀，写入时统一归一为 6 位。
pub fn save_items(items: &[Item]) {
    let mut s = String::new();
    for it in items {
        let Some(norm) = normalize_code(&it.code) else { continue };
        let code: String = norm.chars().filter(|c| c.is_ascii_digit()).collect();
        if code.is_empty() {
            continue;
        }
        let nm = it.name.trim();
        let has_pos = it.qty.abs() > 1e-9 || it.cost.abs() > 1e-9;
        if has_pos || !nm.is_empty() {
            s.push_str(&format!("{} {} {:.2} {:.0}\n", code, nm, it.cost, it.qty));
        } else {
            s.push_str(&format!("{}\n", code));
        }
    }
    let p = crate::config::watchlist_path();
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    let _ = std::fs::write(&p, s);
}

/// 6 位纯数字代码，按交易所补前缀返回可直接用于接口的完整代码（如 sh600000）。
/// 前缀规则：60/68 → sh；00/30 → sz；43/83/87/92 → bj；50 开头基金 → sh。
pub fn normalize_code(raw: &str) -> Option<String> {
    let dig: String = raw.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    if dig.len() != 6 {
        return None;
    }
    let pre = if dig.starts_with('6') || dig.starts_with('5') {
        "sh"
    } else if dig.starts_with('0') || dig.starts_with('2') || dig.starts_with('3') {
        "sz"
    } else if dig.starts_with('4') || dig.starts_with('8') || dig.starts_with('9') {
        "bj"
    } else {
        "sh"
    };
    Some(format!("{pre}{dig}"))
}

/// 从自选中提取用于请求的代码列表（补好前缀）
pub fn request_codes(items: &[Item]) -> Vec<String> {
    items.iter().filter_map(|i| normalize_code(&i.code)).collect()
}

// ───────────────────────── 行情拉取 ─────────────────────────

/// 拉取一批代码的实时行情（一次 HTTP 请求）。网络失败返回空。
pub fn fetch(codes: &[String]) -> Vec<Quote> {
    if codes.is_empty() {
        return Vec::new();
    }
    let q = codes.join(",");
    let path = format!("/q={q}");
    let Some(body) = crate::modules::net::https_get_bytes("qt.gtimg.cn", &path) else {
        return Vec::new();
    };
    // 腾讯接口返回 GBK，转 UTF-8
    let (txt, _, _) = encoding_rs::GBK.decode(&body);
    let txt = txt.into_owned();
    parse_body(&txt)
}

/// 解析腾讯行情正文（形如 v_sh600000="1~浦发银行~600000~7.78~..."; …）
fn parse_body(txt: &str) -> Vec<Quote> {
    let mut out = Vec::new();
    for seg in txt.split(';') {
        let seg = seg.trim();
        if !seg.starts_with("v_") {
            continue;
        }
        let Some(open) = seg.find('"') else { continue };
        let Some(close) = seg.rfind('"') else { continue };
        if close <= open {
            continue;
        }
        let inner = &seg[open + 1..close];
        let f: Vec<&str> = inner.split('~').collect();
        if f.len() < 35 {
            continue;
        }
        let price = f[3].trim().parse::<f64>().unwrap_or(0.0);
        let prev = f[4].trim().parse::<f64>().unwrap_or(0.0);
        if price <= 0.0 || prev <= 0.0 {
            continue;
        }
        out.push(Quote {
            code: f[2].trim().to_string(),
            name: f[1].trim().to_string(),
            price,
            prev_close: prev,
            open: f[5].trim().parse().unwrap_or(0.0),
            high: f[33].trim().parse().unwrap_or(0.0),
            low: f[34].trim().parse().unwrap_or(0.0),
            chg: price - prev,
            chg_pct: (price - prev) / prev * 100.0,
        });
    }
    out
}

// ───────────────────────── 状态读取 ─────────────────────────

/// 取最近一次成功行情快照
pub fn snapshot() -> Vec<Quote> {
    shared().quotes.lock().unwrap().clone()
}

/// 本模块是否有新数据待消费（主线程读取后置回 false）
pub fn take_dirty() -> bool {
    shared().dirty.swap(false, Ordering::Relaxed)
}

/// 按代码找行情
pub fn quote_for<'a>(quotes: &'a [Quote], code: &str) -> Option<&'a Quote> {
    quotes.iter().find(|q| q.code == code)
}

/// 名称持久缓存：代码 → 名称。编辑自选时异步补全，之后跨面板复用，
/// 不受后台轮询「整体替换快照」影响。
static NAME_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
fn name_cache() -> &'static Mutex<HashMap<String, String>> {
    NAME_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记住一个代码对应的名称（异步补全 / 行情拉取后调用）
pub fn remember_name(code: &str, name: &str) {
    if !code.is_empty() && !name.is_empty() {
        name_cache()
            .lock()
            .unwrap()
            .insert(code.to_string(), name.to_string());
    }
}

/// 查已知名称：持久缓存 → 内存行情快照 → 空串（不联网，输入时即时调用）
pub fn name_known(code: &str) -> String {
    if let Some(n) = name_cache().lock().unwrap().get(code) {
        if !n.is_empty() {
            return n.clone();
        }
    }
    name_from_snapshot(code)
}

/// 只从内存行情快照查名称（不联网、不卡 UI）
pub fn name_from_snapshot(code: &str) -> String {
    let snap = snapshot();
    quote_for(&snap, code)
        .map(|q| q.name.clone())
        .unwrap_or_default()
}

/// 按代码自动补全证券名称：持久缓存 → 快照 → 同步拉一次行情
/// （只在编辑自选保存时按行调用，非交易时段也能补出名称）。失败返回空串。
pub fn resolve_name(code: &str) -> String {
    let known = name_known(code);
    if !known.is_empty() {
        return known;
    }
    if let Some(full) = normalize_code(code) {
        let qs = fetch(&[full]);
        if let Some(q) = qs.first() {
            remember_name(&q.code, &q.name);
            return q.name.clone();
        }
    }
    String::new()
}

/// 清空「当天已提醒」记录（设置里改阈值后调用，让新阈值立即生效）
pub fn reset_alerts() {
    alerted().lock().unwrap().clear();
}

/// 今日盈亏（加权）：Σ (现价 - 昨收) × 数量，仅统计持仓（qty>0）
pub fn today_pnl(quotes: &[Quote], items: &[Item]) -> f64 {
    let mut total = 0.0;
    for it in items.iter().filter(|i| i.qty > 0.0) {
        if let Some(q) = quote_for(quotes, &it.code) {
            total += (q.price - q.prev_close) * it.qty;
        }
    }
    total
}

/// 持仓浮动盈亏：Σ (现价 - 成本) × 数量
pub fn float_pnl(quotes: &[Quote], items: &[Item]) -> f64 {
    let mut total = 0.0;
    for it in items.iter().filter(|i| i.qty > 0.0 && i.cost > 0.0) {
        if let Some(q) = quote_for(quotes, &it.code) {
            total += (q.price - it.cost) * it.qty;
        }
    }
    total
}

/// 涨跌提醒：返回第一条涨幅超阈值且当天未提醒过的自选提示；无则 None。
/// 命中后当天不再重复提醒同一只。
pub fn next_alert(alert_pct: f64) -> Option<String> {
    if alert_pct <= 0.0 {
        return None;
    }
    let quotes = snapshot();
    let items = read_items();
    let today = crate::modules::clock::today_str(&crate::modules::clock::local_time());
    let mut seen = alerted().lock().unwrap();
    for it in &items {
        if let Some(q) = quote_for(&quotes, &it.code) {
            if q.chg_pct.abs() >= alert_pct {
                let key = format!("{}|{}", today, q.code);
                if !seen.contains(&key) {
                    seen.insert(key);
                    // 顺手清理非今天的旧 key，避免无限增长
                    if seen.len() > 500 {
                        let keep: Vec<String> = seen
                            .iter()
                            .filter(|k| k.starts_with(&today))
                            .cloned()
                            .collect();
                        *seen = keep.into_iter().collect();
                    }
                    let dir = if q.chg_pct >= 0.0 { "涨" } else { "跌" };
                    let nm = if it.name.is_empty() { &q.name } else { &it.name };
                    let code = &q.code;
                    return Some(format!(
                        "{nm} {code} {}{:+.2}% · 现价 {:.2}",
                        dir,
                        q.chg_pct,
                        q.price
                    ));
                }
            }
        }
    }
    None
}

// ───────────────────────── 后台轮询 ─────────────────────────

/// 是否处于 A 股交易时段（周一至五 9:15–11:30 / 13:00–15:00）
fn is_trading_time() -> bool {
    let st = crate::modules::clock::local_time();
    if st.wDayOfWeek == 0 || st.wDayOfWeek == 6 {
        return false;
    }
    let m = (st.wHour as i64) * 60 + st.wMinute as i64;
    (555..=690).contains(&m) || (780..=900).contains(&m) // 9:15-11:30 & 13:00-15:00
}

/// 启动后台行情轮询线程。间隔 / 自选每次循环从配置与数据文件重读，改后可立即生效。
pub fn spawn() {
    std::thread::spawn(|| loop {
        let cfg = crate::config::Config::load();
        let items = read_items();
        if cfg.modules.stock && !items.is_empty() {
            let codes = request_codes(&items);
            let want = cfg.stock.off_hours || is_trading_time();
            if !codes.is_empty() && want {
                // 交易时段里价格实时变，间隔秒数短；行情接口免费，拉取内容仅自选代码
                let q = fetch(&codes);
                if !q.is_empty() {
                    *shared().quotes.lock().unwrap() = q;
                    *shared().last_ts.lock().unwrap() = crate::modules::files::now_unix();
                    shared().dirty.store(true, Ordering::Relaxed);
                }
            }
        }
        let secs = cfg.stock.refresh_secs.clamp(5, 600) as u64;
        std::thread::sleep(Duration::from_secs(secs));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rules() {
        assert_eq!(normalize_code("600000").as_deref(), Some("sh600000"));
        assert_eq!(normalize_code("688001").as_deref(), Some("sh688001"));
        assert_eq!(normalize_code("000001").as_deref(), Some("sz000001"));
        assert_eq!(normalize_code("300750").as_deref(), Some("sz300750"));
        assert_eq!(normalize_code("430047").as_deref(), Some("bj430047"));
        assert_eq!(normalize_code("sh600000").as_deref(), Some("sh600000"));
        assert_eq!(normalize_code("12345"), None);
        assert_eq!(normalize_code("abc"), None);
    }

    #[test]
    fn parse_tencent_body() {
        // 模拟腾讯返回值，字段 1=名 2=代码 3=现价 4=昨收，33=最高 34=最低
        let line = r#"v_sh600000="1~浦发银行~600000~7.78~7.70~7.75~1~2~3~4~5~6~7~8~9~10~11~12~13~14~15~16~17~18~19~20~21~22~23~24~25~26~27~28~29~20261119093000~0.08~1.04~7.90~7.60";"#;
        let q = parse_body(line);
        assert_eq!(q.len(), 1);
        let q = &q[0];
        assert_eq!(q.code, "600000");
        assert_eq!(q.name, "浦发银行");
        assert!((q.price - 7.78).abs() < 1e-6);
        assert!((q.prev_close - 7.70).abs() < 1e-6);
        assert!((q.chg - 0.08).abs() < 1e-6);
        assert!((q.chg_pct - 1.04).abs() < 1e-6);
    }

    #[test]
    fn read_items_parses() {
        // 名称带空格式的多种写法
        let saved = crate::config::watchlist_path();
        let dir = saved.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            &saved,
            "# 测试\n600000 浦发银行 7.50 1000\n000001 5.00 500\n300750\n",
        )
        .unwrap();
        let items = read_items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].code, "600000");
        assert!((items[0].cost - 7.5).abs() < 1e-6);
        assert!((items[0].qty - 1000.0).abs() < 1e-6);
        assert_eq!(items[1].cost, 5.0);
        assert_eq!(items[1].qty, 500.0);
        assert_eq!(items[2].cost, 0.0);
        assert_eq!(items[2].qty, 0.0);
        std::fs::remove_file(&saved).ok();
    }

    #[test]
    fn pnl_is_weighted() {
        let q = vec![Quote {
            code: "600000".into(),
            name: "浦发".into(),
            price: 8.0,
            prev_close: 7.0,
            open: 0.0,
            high: 0.0,
            low: 0.0,
            chg: 1.0,
            chg_pct: 14.28,
        }];
        let items = vec![Item { code: "600000".into(), name: String::new(), cost: 6.0, qty: 100.0 }];
        assert!((today_pnl(&q, &items) - 100.0).abs() < 1e-6);
        assert!((float_pnl(&q, &items) - 200.0).abs() < 1e-6);
    }
}
