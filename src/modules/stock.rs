//! 盯盘助手 — A 股实时行情（腾讯公开行情接口）、自选 + 持仓 + 今日盈亏 + 涨跌提醒。
//!
//! 自选/持仓来自 `%APPDATA%\WinIsland\watchlist.txt`，每行 `代码 名称 成本价 数量`。
//! 后台线程按配置间隔拉取腾讯行情（GBK→UTF-8，经 encoding_rs），写入内存快照。
//! 只在交易时段轮询（可关闭）；价格涨跌幅超过阈值时触发一次提醒（当天不重复）。
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
    pub name: String, // 接口名称（自动补全，可含空格）
    pub cost: f64,
    pub qty: f64,
    /// 该股独立涨跌提醒阈值（百分比绝对值）；0 = 这只不提醒
    pub alert: f64,
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

/// 按需查询的行情缓存：编辑窗口输入一个【尚未加入 watchlist】的新代码时，
/// 主轮询线程只会拉 watchlist 里的代码，快照里根本没有它 —— 这正是
/// 「输入代码后名称/现价一直不显示」的根因。这里补一份按代码单独拉取的缓存，
/// snapshot() 会把它和主快照合并，主面板 / 编辑窗口都能立即读到。
static EXTRA_QUOTES: OnceLock<Mutex<HashMap<String, Quote>>> = OnceLock::new();
fn extra_quotes() -> &'static Mutex<HashMap<String, Quote>> {
    EXTRA_QUOTES.get_or_init(|| Mutex::new(HashMap::new()))
}
/// 已发起过按需查询的 6 位代码，避免编辑时每次按键都起线程（输入不卡的前提）
static LOOKUP_TRIED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
fn lookup_tried() -> &'static Mutex<HashSet<String>> {
    LOOKUP_TRIED.get_or_init(|| Mutex::new(HashSet::new()))
}

// ───────────────────────── 自选/持仓 读写 ─────────────────────────

/// watchlist.txt 每行格式：`代码 成本价 数量 提醒阈值 名称…`
/// 数字字段固定在前面、名称放最后（可含空格），避免名称里的空格把成本/数量解析错位。
/// 同时兼容旧格式 `代码 名称 成本价 数量`：判据是第 2 个 token 能否解析为数字。
fn parse_line(line: &str) -> Option<Item> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let raw = *toks.first()?;
    let _full = normalize_code(raw)?; // 校验为 6 位代码
    let code: String = raw.trim().chars().filter(|c| c.is_ascii_digit()).collect();
    if code.len() != 6 {
        return None;
    }

    // 新格式：toks[1]=成本, [2]=数量, [3]=提醒, [4..]=名称（可含空格）
    let new_fmt = toks.len() >= 2 && toks[1].parse::<f64>().is_ok();
    let (cost, qty, alert, name) = if new_fmt {
        let cost = toks[1].parse::<f64>().unwrap_or(0.0);
        let qty = toks.get(2).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let alert = toks.get(3).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let name = if toks.len() > 4 { toks[4..].join(" ") } else { String::new() };
        (cost, qty, alert, name)
    } else {
        // 旧格式：代码 名称… 成本 数量 —— 名称可能含空格，取「所有非数字 token」拼接；
        // 末尾的数字 token 依次是成本、数量（数量可省略）。
        let mut nums: Vec<f64> = Vec::new();
        let mut name_parts: Vec<&str> = Vec::new();
        for t in toks.iter().skip(1) {
            if let Ok(v) = t.parse::<f64>() {
                nums.push(v);
            } else {
                name_parts.push(t);
            }
        }
        let cost = nums.first().copied().unwrap_or(0.0);
        let qty = nums.get(1).copied().unwrap_or(0.0);
        let name = name_parts.join(" ");
        (cost, qty, 0.0, name)
    };
    Some(Item { code, name, cost, qty, alert })
}

/// 读取盯盘数据文件，按行解析成 Item 列表。解析失败 / 空返回空列表。
pub fn read_items() -> Vec<Item> {
    let p = crate::config::watchlist_path();
    let Ok(txt) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    txt.lines().filter_map(|l| {
        let l = l.trim();
        if l.is_empty() || l.starts_with('#') { None } else { parse_line(l) }
    }).collect()
}

/// 将自选列表写回 watchlist.txt。
/// 每行 `代码 成本价 数量 提醒阈值 名称…`；成本/数量/提醒为 0 时写 0，名称可空。
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
        s.push_str(&format!(
            "{} {:.2} {:.0} {:.2} {}\n",
            code, it.cost, it.qty, it.alert, nm
        ));
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

/// 取最近一次成功行情快照（主轮询结果 + 编辑窗口按需查询的新代码）
pub fn snapshot() -> Vec<Quote> {
    let mut out = shared().quotes.lock().unwrap().clone();
    let extra = extra_quotes().lock().unwrap();
    for q in extra.values() {
        // 主快照里已有的代码优先（主轮询数据更新），不重复追加
        if !out.iter().any(|x| x.code == q.code) {
            out.push(q.clone());
        }
    }
    out
}

/// 按需拉一只尚未加入 watchlist 的股票行情（编辑窗口输入 6 位代码时调用）。
/// 完全异步：本函数只登记代码并起一个后台线程，不阻塞 UI；同一代码进程内只查一次，
/// 所以连续输入 / 退格都不会反复联网，保持"打字不卡"。
pub fn request_lookup(code: &str) {
    let Some(full) = normalize_code(code) else { return };
    let pure: String = full.chars().filter(|c| c.is_ascii_digit()).collect();
    if pure.len() != 6 {
        return;
    }
    // 快照里已经有的（watchlist 中已存在或之前查过）直接跳过
    if quote_for(&snapshot(), &pure).is_some() {
        return;
    }
    {
        let mut tried = lookup_tried().lock().unwrap();
        if !tried.insert(pure.clone()) {
            return;
        }
    }
    std::thread::spawn(move || {
        let qs = fetch(&[full]);
        if qs.is_empty() {
            // 拉取失败允许下次再试（不把失败永久记进 tried）
            lookup_tried().lock().unwrap().remove(&pure);
            return;
        }
        let mut extra = extra_quotes().lock().unwrap();
        for q in qs {
            extra.insert(q.code.clone(), q);
        }
        shared().dirty.store(true, Ordering::Relaxed);
    });
}

/// 本模块是否有新数据待消费（主线程读取后置回 false）
pub fn take_dirty() -> bool {
    shared().dirty.swap(false, Ordering::Relaxed)
}

/// 按代码找行情
pub fn quote_for<'a>(quotes: &'a [Quote], code: &str) -> Option<&'a Quote> {
    quotes.iter().find(|q| q.code == code)
}

/// 只从内存行情快照查名称（不联网、不卡 UI）
pub fn name_from_snapshot(code: &str) -> String {
    let snap = snapshot();
    quote_for(&snap, code)
        .map(|q| q.name.clone())
        .unwrap_or_default()
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

/// 涨跌提醒：按每支股票自己设置的阈值判断，返回第一条涨幅超阈值且当天未提醒过的自选提示；
/// 无则 None。某支 alert<=0 表示这支不提醒。命中后当天不再重复提醒同一只。
pub fn next_alert() -> Option<String> {
    let quotes = snapshot();
    let items = read_items();
    let today = crate::modules::clock::today_str(&crate::modules::clock::local_time());
    let mut seen = alerted().lock().unwrap();
    for it in &items {
        // 每支股票独立阈值：0 = 这支不提醒
        if it.alert <= 0.0 {
            continue;
        }
        if let Some(q) = quote_for(&quotes, &it.code) {
            if q.chg_pct.abs() >= it.alert {
                let key = format!("{}|{}|{:.2}", today, q.code, it.alert);
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

/// 首次启动是否已完成过一次成功的行情拉取。
/// 默认 false：第一轮无条件拉一次，保证「刚打开面板就有数据」，
/// 不再依赖点编辑触发的按需查询。拉取成功后置 true，此后才由
/// `off_hours` / 交易时段决定是否继续刷新；若首次拉取失败则保持 false，
/// 非交易时段也会按刷新间隔重试，直到成功为止（网络兜底）。
static BOOTSTRAPPED: AtomicBool = AtomicBool::new(false);

/// 启动后台行情轮询线程。间隔 / 自选每次循环从配置与数据文件重读，改后可立即生效。
pub fn spawn() {
    std::thread::spawn(|| loop {
        let cfg = crate::config::Config::load();
        let items = read_items();
        if cfg.modules.stock && !items.is_empty() {
            let codes = request_codes(&items);
            // 首次循环强制拉一次（即使非交易时段 / off_hours=false），
            // 让主面板首次打开即有行情；之后才尊重刷新开关。
            let want = !BOOTSTRAPPED.load(Ordering::Relaxed) || cfg.stock.off_hours || is_trading_time();
            if !codes.is_empty() && want {
                // 交易时段里价格实时变，间隔秒数短；行情接口免费，拉取内容仅自选代码
                let q = fetch(&codes);
                if !q.is_empty() {
                    *shared().quotes.lock().unwrap() = q;
                    *shared().last_ts.lock().unwrap() = crate::modules::files::now_unix();
                    shared().dirty.store(true, Ordering::Relaxed);
                    BOOTSTRAPPED.store(true, Ordering::Relaxed);
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
        // 涨跌幅是现算的：腾讯原文里给的是四舍五入到两位的 1.04，这里对现算值做断言
        assert!((q.chg_pct - (0.08 / 7.70 * 100.0)).abs() < 1e-6);
    }

    #[test]
    fn read_items_parses() {
        // 新格式：代码 成本 数量 提醒 名称（名称可含空格）；旧格式：代码 名称 成本 数量
        let saved = crate::config::watchlist_path();
        let dir = saved.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            &saved,
            "# 测试\n600000 7.50 1000 3.00 浦发银行\n000001 5.00 500 0.00\n300750 20.00 100 2.50 宁德时代 X\n600519 贵州茅台 1600.00 10\n",
        )
        .unwrap();
        let items = read_items();
        assert_eq!(items.len(), 4);
        // 新格式 + 名称
        assert_eq!(items[0].code, "600000");
        assert!((items[0].cost - 7.5).abs() < 1e-6);
        assert!((items[0].qty - 1000.0).abs() < 1e-6);
        assert!((items[0].alert - 3.0).abs() < 1e-6);
        assert_eq!(items[0].name, "浦发银行");
        // 新格式 + 无名称
        assert!((items[1].cost - 5.0).abs() < 1e-6);
        assert!((items[1].qty - 500.0).abs() < 1e-6);
        // 新格式 + 名称含空格
        assert_eq!(items[2].name, "宁德时代 X");
        assert!((items[2].alert - 2.5).abs() < 1e-6);
        // 旧格式兼容
        assert_eq!(items[3].name, "贵州茅台");
        assert!((items[3].cost - 1600.0).abs() < 1e-6);
        assert!((items[3].qty - 10.0).abs() < 1e-6);
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
        let items = vec![Item {
            code: "600000".into(),
            name: String::new(),
            cost: 6.0,
            qty: 100.0,
            alert: 0.0,
        }];
        assert!((today_pnl(&q, &items) - 100.0).abs() < 1e-6);
        assert!((float_pnl(&q, &items) - 200.0).abs() < 1e-6);
    }
}
