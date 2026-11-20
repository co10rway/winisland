//! 邮箱验证码 — IMAPS 拉取（rustls 静态编译），仅本地解析邮件主题/正文前 2KB
//! 中的验证码数字；除登录必需外不上传任何数据。
//!
//! 协议层是自己写的极简 IMAP 客户端，但必须遵守两条硬规则，之前的实现都踩了：
//! 1. **响应必须要读到 tag 完成行**（`a001 OK ...`）才算结束。旧实现靠"读到的数据
//!    以 CRLF 结尾 + 250ms 内没数据"判断结束，而那次探测超时会被当成"还有数据"，
//!    于是每轮都会继续阻塞 20 秒再超时失败——这就是「配好了也收不到提醒」的主因。
//! 2. **去重要用 UID**。旧的 key 是「发件人|主题」，验证码邮件的主题往往就是
//!    「验证码」四个字，第二次之后全被当成同一条跳过。

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::store::MailCode;

/// 本进程是否还没完成首轮拉取。
///
/// 首轮的作用是「静默对齐水位」：把 `mail_last_uid` 推进到收件箱当前最新的 UID，
/// 但不弹任何验证码。这样启动前邮箱里积压的旧验证码不会被翻出来；而此后新到达
/// 的邮件 UID 一定更大，下一轮就能被识别并弹出。
///
/// 之所以不再用「INTERNALDATE 与本机启动时间比较」来判断新旧：那是两个不同时钟源
/// （邮件服务器时间 vs 本机系统时间），只要两者有偏差，启动后新到的邮件时间戳就
/// 可能 ≤ 本机启动时刻而被误判成旧邮件跳过，表现为「先开软件、后收邮件却收不到」。
/// UID 是服务器单调递增的，用它做水位完全不依赖时钟，更可靠。
static FIRST_POLL: AtomicBool = AtomicBool::new(true);

#[derive(Default, Clone)]
pub struct MailShared {
    pub latest: Option<MailCode>,
    /// 邮件线程每抓到一封「新验证码邮件」就 +1。主界面启动时对齐这个序号，
    /// 只提示序号增量，避免拿启动时 Store 快照和后台线程比较 UID/时间产生竞态。
    pub seq: u64,
    pub status: String, // "未配置" / "监听中" / "上次错误: ..."
}

pub type SharedMail = Arc<Mutex<MailShared>>;

/// 最近一次状态（供设置页显示，跨线程读取）
static STATUS: Mutex<String> = Mutex::new(String::new());

fn set_status(shared: &SharedMail, s: impl Into<String>) {
    let s = s.into();
    if let Ok(mut g) = STATUS.lock() {
        *g = s.clone();
    }
    if let Ok(mut g) = shared.lock() {
        g.status = s;
    }
}

/// 设置页展示用：邮箱监听当前状态
pub fn last_status() -> String {
    STATUS.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn spawn(shared: SharedMail) {
    std::thread::spawn(move || loop {
        let cfg_all = crate::config::Config::load();
        let cfg = cfg_all.mail;
        let enabled = cfg_all.modules.mailcode;
        if !enabled {
            set_status(&shared, "已关闭");
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        if cfg.imap_host.trim().is_empty() || cfg.user.trim().is_empty() || cfg.password.is_empty() {
            set_status(&shared, "未配置 · 请填写 IMAP 服务器 / 邮箱地址 / 授权码");
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        match poll_once(&cfg, &shared) {
            Ok(diag) => {
                let wait = cfg.poll_seconds.clamp(10, 600) as u64;
                // 状态里带上拉取诊断，用户能直接看到监听是否在跑、拉到几封、卡在哪
                set_status(&shared, format!("监听中 · 每 {wait} 秒 · {diag}"));
                std::thread::sleep(Duration::from_secs(wait));
            }
            Err(e) => {
                set_status(&shared, format!("连接失败：{e}（30 秒后重试）"));
                std::thread::sleep(Duration::from_secs(30));
            }
        }
    });
}

// ── 一次完整拉取 ──

/// 连接 + 登录，返回可用的 IMAP 连接
fn connect(cfg: &crate::config::Mail) -> Result<ImapConn<TlsStream>, String> {
    use rustls::pki_types::ServerName;

    let server_name =
        ServerName::try_from(cfg.imap_host.trim().to_string()).map_err(|_| "服务器名无效".to_string())?;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = std::net::TcpStream::connect((cfg.imap_host.trim(), cfg.imap_port))
        .map_err(|e| format!("无法连接 {}:{} —— {e}", cfg.imap_host.trim(), cfg.imap_port))?;
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(20))).ok();
    let tls = rustls::ClientConnection::new(Arc::new(tls_config), server_name)
        .map_err(|e| format!("TLS 初始化失败：{e}"))?;
    let mut conn = ImapConn {
        stream: rustls::StreamOwned::new(tls, tcp),
        buf: Vec::with_capacity(8192),
        tag: 0,
    };

    // 问候
    let greet = conn.read_line()?;
    if !greet.to_uppercase().contains("OK") {
        return Err(format!("服务器问候异常：{greet}"));
    }
    // 登录
    let user = cfg.user.trim();
    let pass = cfg.password.trim();
    let r = conn.cmd(&format!(
        "LOGIN \"{}\" \"{}\"",
        imap_escape(user),
        imap_escape(pass)
    ))?;
    if !is_ok(&r) {
        return Err(format!(
            "登录被拒绝（QQ/163 等需用「授权码」而非登录密码）：{}",
            first_tag_line(&r)
        ));
    }
    // 网易（163/126）在登录后会要求客户端声明身份，否则 SELECT 返回 Unsafe Login。
    // 不支持的服务器回 BAD，忽略即可。
    let _ = conn.cmd("ID (\"name\" \"WinIsland\" \"version\" \"0.14\" \"vendor\" \"winisland\")");
    Ok(conn)
}

/// 设置页「测试连接」：只握手 + 登录 + 打开收件箱，不消费任何验证码
pub fn test_connection_async() {
    if let Ok(mut g) = STATUS.lock() {
        *g = "正在测试连接…".into();
    }
    std::thread::spawn(|| {
        let cfg = crate::config::Config::load();
        let m = cfg.mail;
        if m.imap_host.trim().is_empty() || m.user.trim().is_empty() {
            set_only_status("请先填写 IMAP 服务器与邮箱地址".to_string());
            return;
        }
        if m.password.is_empty() {
            set_only_status("请先填写密码 / 授权码".to_string());
            return;
        }
        let r = connect(&m).and_then(|mut conn| {
            let folder = if m.folder.trim().is_empty() { "INBOX" } else { m.folder.trim() };
            conn.select(folder)
        });
        match r {
            Ok(n) => set_only_status(format!("连接成功 · 「{}」共 {} 封邮件", if m.folder.trim().is_empty() { "INBOX" } else { m.folder.trim() }, n)),
            Err(e) => set_only_status(format!("连接失败：{e}")),
        }
    });
}

fn set_only_status(s: String) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s;
    }
}

fn poll_once(cfg: &crate::config::Mail, shared: &SharedMail) -> Result<String, String> {
    let mut conn = connect(cfg)?;
    let folder = if cfg.folder.trim().is_empty() { "INBOX" } else { cfg.folder.trim() };
    let exists = conn.select(folder)?;

    let mut store = crate::store::Store::load();
    let last_uid = store.mail_last_uid;
    let msgs = conn.fetch_since(last_uid, exists)?;

    // 首轮静默对齐：只在第一次成功拉取时把水位推进到当前最新 UID，不弹任何验证码。
    // 这样启动前积压的旧验证码不会在启动时被翻出来；之后 UID 更大的才是新邮件。
    // 用 compare_exchange 保证即使多线程也只有一轮是「首轮」。
    let silent_first = FIRST_POLL.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok();

    // 只认 UID 更大的（服务端在 UID 越界时会回最后一条，靠这里过滤）
    let mut newest: Option<(u32, MailCode)> = None;
    let mut max_uid = last_uid;
    let mut fetched = 0usize;
    let mut skipped_old = 0usize;
    for m in &msgs {
        fetched += 1;
        max_uid = max_uid.max(m.uid);
        if m.uid <= last_uid {
            skipped_old += 1;
            continue;
        }
        // 首轮只推进水位、不弹验证码（启动前积压的一律不提醒）
        if silent_first {
            continue;
        }
        if let Some(code) = extract_code(&m.subject, &m.body) {
            let ok = newest.as_ref().map(|(u, _)| m.uid > *u).unwrap_or(true);
            if ok {
                newest = Some((
                    m.uid,
                    MailCode {
                        code,
                        from: clean_from(&m.from),
                        ts: crate::modules::files::now_unix(),
                        uid: m.uid,
                    },
                ));
            }
        }
    }

    store.mail_last_uid = max_uid;
    let newest_uid = newest.as_ref().map(|(u, _)| *u);
    if let Some((_, mc)) = newest {
        store.mail_code = Some(mc.clone());
        if let Ok(mut s) = shared.lock() {
            s.latest = Some(mc);
            s.seq = s.seq.saturating_add(1);
        }
    } else if let Ok(mut s) = shared.lock() {
        // 没有新验证码时保留已有展示内容，不覆盖
        if s.latest.is_none() {
            s.latest = store.mail_code.clone();
        }
    }
    store.save();

    // 诊断串：拉到几封 / 跳过几封旧 / 水位 UID / 本轮是否首轮对齐 / 是否取到验证码
    let diag = format!(
        "拉到 {fetched} 封 · 旧 {skipped_old} · 水位 {max_uid}{}{}",
        if silent_first { " · 首轮对齐" } else { "" },
        if let Some(u) = newest_uid {
            format!(" · 验证码 UID {u}")
        } else {
            String::new()
        },
    );
    Ok(diag)
}

fn is_ok(reply: &str) -> bool {
    reply
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| {
            let u = l.to_uppercase();
            u.contains(" OK")
        })
        .unwrap_or(false)
}

fn first_tag_line(reply: &str) -> String {
    reply
        .lines()
        .rev()
        .find(|l| l.starts_with("a0") || l.contains(" NO ") || l.contains(" BAD "))
        .unwrap_or("")
        .trim()
        .chars()
        .take(160)
        .collect()
}

// ── IMAP 连接 ──

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>;

struct ImapConn<S: Read + Write> {
    stream: S,
    /// 已从 socket 读入但还没消费的字节
    buf: Vec<u8>,
    tag: u32,
}

struct MailMsg {
    uid: u32,
    /// 邮件到达时间（IMAP INTERNALDATE，unix 秒）；0 表示解析失败。
    /// 仅作参考保留：新旧判断已改用 UID 水位（见 FIRST_POLL），不再依赖此时间，
    /// 以免两端时钟偏差导致新邮件被误判为旧邮件。
    #[allow(dead_code)]
    date: i64,
    from: String,
    subject: String,
    body: String,
}

impl<S: Read + Write> ImapConn<S> {
    fn fill(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 8192];
        let n = self.stream.read(&mut chunk).map_err(|e| format!("读取超时/断开：{e}"))?;
        if n == 0 {
            return Err("服务器关闭了连接".into());
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// 读一行（不含 CRLF）
    fn read_line(&mut self) -> Result<String, String> {
        loop {
            if let Some(pos) = find_crlf(&self.buf) {
                let line = String::from_utf8_lossy(&self.buf[..pos]).to_string();
                self.buf.drain(..pos + 2);
                return Ok(line);
            }
            if self.buf.len() > 8 * 1024 * 1024 {
                return Err("响应过大".into());
            }
            self.fill()?;
        }
    }

    fn read_n(&mut self, n: usize) -> Result<Vec<u8>, String> {
        while self.buf.len() < n {
            self.fill()?;
        }
        let out = self.buf[..n].to_vec();
        self.buf.drain(..n);
        Ok(out)
    }

    fn next_tag(&mut self) -> String {
        self.tag += 1;
        format!("a{:04}", self.tag)
    }

    /// 发一条命令，读到该 tag 的完成行（含中间的字面量）为止
    fn cmd(&mut self, command: &str) -> Result<String, String> {
        let tag = self.next_tag();
        let line = format!("{} {}\r\n", tag, command);
        self.stream
            .write_all(line.as_bytes())
            .map_err(|e| format!("发送失败：{e}"))?;
        self.stream.flush().ok();
        self.read_until_tag(&tag)
    }

    fn read_until_tag(&mut self, tag: &str) -> Result<String, String> {
        let mut out = String::new();
        loop {
            let line = self.read_line()?;
            // 字面量：行尾 `{123}`，后面紧跟 123 字节原样数据
            if let Some(n) = trailing_literal(&line) {
                out.push_str(&line);
                out.push('\n');
                let data = self.read_n(n)?;
                out.push_str(&String::from_utf8_lossy(&data));
                continue;
            }
            out.push_str(&line);
            out.push('\n');
            if line.starts_with(&format!("{tag} ")) {
                return Ok(out);
            }
            if line.to_uppercase().starts_with("* BYE") {
                return Err("服务器主动断开（BYE）".into());
            }
        }
    }

    fn select(&mut self, folder: &str) -> Result<u32, String> {
        let resp = self.cmd(&format!("SELECT \"{}\"", imap_escape(folder)))?;
        if !is_ok(&resp) {
            return Err(format!("打不开邮箱文件夹「{folder}」：{}", first_tag_line(&resp)));
        }
        for line in resp.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("* ") {
                if let Some(n_str) = rest.strip_suffix(" EXISTS") {
                    if let Ok(n) = n_str.trim().parse() {
                        return Ok(n);
                    }
                }
            }
        }
        Ok(0)
    }

    /// 拉取新邮件（首轮只取最近 10 封，避免一次拉满）。
    /// 返回每条邮件的 UID、头部原文、正文片段。
    ///
    /// 注意首轮必须用**序号**（`FETCH 41:50`）而不是 UID（`UID FETCH 41:50`）：
    /// 后者取的是 UID 41~50 那几封，对一个有几千封信的邮箱来说是最老的那批，
    /// 反而不是最近的。
    fn fetch_since(&mut self, last_uid: u32, exists: u32) -> Result<Vec<MailMsg>, String> {
        if exists == 0 {
            return Ok(Vec::new());
        }
        let (word, range) = if last_uid == 0 {
            (
                "FETCH",
                format!("{}:{}", exists.saturating_sub(9).max(1), exists),
            )
        } else {
            ("UID FETCH", format!("{}:*", last_uid + 1))
        };
        let tag = self.next_tag();
        let req = format!(
            "{} {} {} (UID INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)] BODY.PEEK[TEXT]<0.4096>)\r\n",
            tag, word, range
        );
        self.stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("发送失败：{e}"))?;
        self.stream.flush().ok();

        // 一条 FETCH 响应里按顺序有 2 个字面量：头部、正文
        let mut out: Vec<MailMsg> = Vec::new();
        let mut cur = MailMsg {
            uid: 0,
            date: 0,
            from: String::new(),
            subject: String::new(),
            body: String::new(),
        };
        let mut has_cur = false;
        let mut lit_idx = 0usize;
        let mut head_raw = Vec::new();
        let mut body_raw = Vec::new();

        loop {
            let line = self.read_line()?;
            if line.starts_with(&format!("{tag} ")) {
                if !is_ok(&line) {
                    return Err(format!("拉取失败：{}", line.trim()));
                }
                break;
            }
            // 先认「新的一条」再处理字面量：首条响应行本身就以 `{N}` 结尾，
            // 如果先判字面量就会在读字面量后 continue，永远建不出 cur（旧 bug）
            if line.contains("FETCH (") || line.contains("FETCH(") {
                if has_cur {
                    out.push(finish_msg(&cur, &head_raw, &body_raw));
                }
                cur = MailMsg {
                    uid: parse_uid(&line),
                    date: parse_internal_date(&line),
                    from: String::new(),
                    subject: String::new(),
                    body: String::new(),
                };
                has_cur = true;
                lit_idx = 0;
                head_raw = Vec::new();
                body_raw = Vec::new();
            }
            if let Some(n) = trailing_literal(&line) {
                let data = self.read_n(n)?;
                if lit_idx == 0 {
                    head_raw = data;
                } else if lit_idx == 1 {
                    body_raw = data;
                }
                lit_idx += 1;
            }
        }
        if has_cur {
            out.push(finish_msg(&cur, &head_raw, &body_raw));
        }
        Ok(out)
    }
}

fn finish_msg(cur: &MailMsg, head: &[u8], body: &[u8]) -> MailMsg {
    let h = String::from_utf8_lossy(head).to_string();
    let b = String::from_utf8_lossy(body).to_string();
    MailMsg {
        uid: cur.uid,
        date: cur.date,
        from: header_field(&h, "from"),
        subject: decode_header(&header_field(&h, "subject")),
        body: decode_body(&b),
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// 行尾的 IMAP 字面量长度标记 `{123}`
fn trailing_literal(line: &str) -> Option<usize> {
    let t = line.trim_end();
    if !t.ends_with('}') {
        return None;
    }
    let open = t.rfind('{')?;
    let inner = &t[open + 1..t.len() - 1];
    // `{123}` 或 `{123+}`（非同步字面量）
    let inner = inner.strip_suffix('+').unwrap_or(inner);
    inner.parse::<usize>().ok()
}

fn parse_uid(line: &str) -> u32 {
    let up = line.to_uppercase();
    if let Some(p) = up.find("UID ") {
        let rest = &line[p + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse() {
            return n;
        }
    }
    0
}

/// 解析 IMAP FETCH 响应行上的 INTERNALDATE（服务器记录的到达时间）：
/// `"20-Nov-2026 13:05:00 +0800"` → unix 秒。解析失败返回 0。
/// 用服务器到达时间而非邮件头 Date（后者可被客户端伪造/时区混乱）。
fn parse_internal_date(line: &str) -> i64 {
    let Some(a) = line.find("INTERNALDATE") else { return 0 };
    let rest = &line[a + "INTERNALDATE".len()..];
    let Some(q1) = rest.find('"') else { return 0 };
    let after_q = &rest[q1 + 1..];
    let Some(q2) = after_q.find('"') else { return 0 };
    let s = &after_q[..q2];
    // IMAP INTERNALDATE 标准格式（RFC 3501）："20-Nov-2026 13:05:00 +0800"
    // 注意日期是一个整体 token，用连字符分隔（DD-Mon-YYYY），不是空格分隔的三个字段。
    // 之前的版本误按空格拆分，"20-Nov-2026".parse::<i64>() 必然失败 → 恒返回 0，
    // 导致所有验证码邮件都被当成「无有效到达时间」而跳过，收不到任何验证码。
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 3 {
        return 0;
    }
    // parts[0] = "20-Nov-2026"（也可能出现无前导零的 "1-Nov-2026"）
    let dmy: Vec<&str> = parts[0].split('-').collect();
    if dmy.len() != 3 {
        return 0;
    }
    let day: i64 = match dmy[0].parse() { Ok(v) => v, Err(_) => return 0 };
    let mon = match dmy[1].to_uppercase().as_str() {
        "JAN" => 1, "FEB" => 2, "MAR" => 3, "APR" => 4, "MAY" => 5, "JUN" => 6,
        "JUL" => 7, "AUG" => 8, "SEP" => 9, "OCT" => 10, "NOV" => 11, "DEC" => 12,
        _ => return 0,
    };
    let year: i64 = match dmy[2].parse() { Ok(v) => v, Err(_) => return 0 };
    // parts[1] = "13:05:00"
    let hms: Vec<&str> = parts[1].split(':').collect();
    if hms.len() != 3 {
        return 0;
    }
    let (hh, mm, ss): (i64, i64, i64) = match (hms[0].parse(), hms[1].parse(), hms[2].parse()) {
        (Ok(h), Ok(m), Ok(s)) => (h, m, s),
        _ => return 0,
    };
    // parts[2] = "+0800" 时区 ±HHMM
    let tz_secs: i64 = if parts.len() >= 3 {
        let t = parts[2];
        let sign = match t.as_bytes().first() { Some(b'+') => 1, Some(b'-') => -1, _ => 0 };
        let digits = &t[1..];
        if digits.len() >= 4 {
            let th: i64 = digits[..2].parse().unwrap_or(0);
            let tm: i64 = digits[2..4].parse().unwrap_or(0);
            sign * (th * 3600 + tm * 60)
        } else { 0 }
    } else { 0 };

    // 民用日期 → 距 epoch 的天数（Howard Hinnant 算法，无第三方依赖）
    let y = if mon <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if mon > 2 { mon - 3 } else { mon + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;

    days * 86400 + hh * 3600 + mm * 60 + ss - tz_secs
}

/// 从原始头部文本里取某个字段（支持折行续行）
fn header_field(text: &str, name: &str) -> String {
    let mut out: Option<String> = None;
    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(o) = out.as_mut() {
                o.push_str(line.trim());
            }
            continue;
        }
        if out.is_some() {
            break;
        }
        if let Some((n, v)) = line.split_once(':') {
            if n.trim().eq_ignore_ascii_case(name) {
                out = Some(v.trim().to_string());
            }
        }
    }
    out.unwrap_or_default()
}

fn imap_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn clean_from(s: &str) -> String {
    // "GitHub <noreply@github.com>" -> "noreply@github.com"
    if let (Some(a), Some(b)) = (s.rfind('<'), s.rfind('>')) {
        if b > a {
            return s[a + 1..b].to_string();
        }
    }
    s.trim().to_string()
}

// ── 解码 ──

/// RFC 2047 编码字解码：`=?UTF-8?B?...?=` / `=?gb2312?Q?...?=`，可多段拼接
///
/// 注意不能靠 `find("?=")` 找编码字结尾：Q 编码里 `?=` 会先出现在 `Q?=` 处，
/// 导致整个编码字被误判、原样输出（旧实现的 bug）。
fn decode_header(s: &str) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < s.len() {
        if s[i..].starts_with("=?") {
            let rest = &s[i + 2..];
            // charset?enc?text?=
            let mut parts = rest.splitn(3, '?');
            if let (Some(charset), Some(enc), Some(tail)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let Some(rel_end) = tail.find("?=") {
                    let encoded = &tail[..rel_end];
                    if let Some(txt) = decode_rfc2047_word(charset, enc, encoded) {
                        out.push_str(&txt);
                        i += 2 + charset.len() + 1 + enc.len() + 1 + rel_end + 2;
                        continue;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn decode_rfc2047_word(charset: &str, enc: &str, text: &str) -> Option<String> {
    let charset = charset.to_lowercase();
    let raw = match enc.to_lowercase().as_str() {
        "b" => base64_decode(text).ok()?,
        "q" => q_decode(text),
        _ => return None,
    };
    Some(decode_charset(&raw, &charset))
}

/// 尽力解码字符集：UTF-8 直接转；GBK 系没有内置码表，用 latin1 兜底
/// （验证码是 ASCII 数字，兜底后依然能认出，不会漏提醒）
fn decode_charset(raw: &[u8], charset: &str) -> String {
    if charset.contains("utf") || charset.contains("ascii") {
        if let Ok(s) = std::str::from_utf8(raw) {
            return s.to_string();
        }
        return String::from_utf8_lossy(raw).to_string();
    }
    // GB2312/GBK/BIG5/ISO-8859-* 等：ASCII 部分保持原样即可
    raw.iter()
        .map(|&b| if b < 0x80 { b as char } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn q_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// 正文可能被 base64 / quoted-printable 编码（中文邮件非常常见），
/// 这里把「解码后的文本」和「原文」都拼在一起返回，交给 extract_code 逐个尝试
fn decode_body(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let mut out = String::new();
    // 1) base64 段：从 "base64" 那一行之后开始取，跳过空行，直到 MIME 边界 / 下一个头部
    if let Some(pos) = lower.find("base64") {
        let start = raw[pos..].find('\n').map(|i| pos + i + 1).unwrap_or(raw.len());
        let chunk: String = raw[start..]
            .lines()
            .skip_while(|l| l.trim().is_empty())
            .take_while(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("--") && !t.contains(':')
            })
            .collect::<Vec<_>>()
            .join("");
        if chunk.len() >= 16 {
            if let Ok(bytes) = base64_decode(&chunk) {
                out.push_str(&String::from_utf8_lossy(&bytes));
                out.push('\n');
            }
        }
    }
    // 2) quoted-printable
    if lower.contains("quoted-printable") {
        out.push_str(&String::from_utf8_lossy(&q_decode(raw)));
        out.push('\n');
    }
    out.push_str(raw);
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    const TBL: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for ch in s.bytes() {
        if ch == b'=' || ch == b'\r' || ch == b'\n' || ch == b' ' || ch == b'\t' {
            continue;
        }
        let v = TBL.iter().position(|&c| c == ch).ok_or(())? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.is_empty() {
        return Err(());
    }
    Ok(out)
}

// ── 验证码提取 ──

/// 从主题/正文中提取验证码（本地启发式，绝不外传内容）
pub fn extract_code(subject: &str, body: &str) -> Option<String> {
    for text in [subject, body] {
        if text.is_empty() {
            continue;
        }
        // 统一在**小写副本**上做查找和切片，避免 to_lowercase 改变字节长度后
        // 用旧下标切原文导致 panic（个别 Unicode 字符会变长）
        let low = text.to_lowercase();
        for kw in [
            "验证码",
            "校验码",
            "动态码",
            "确认码",
            "激活码",
            "code is",
            "verification code",
            "your code",
            "code:",
            "code：",
        ] {
            if let Some(pos) = low.find(kw) {
                let rest = &low[pos + kw.len()..];
                if let Some(code) = first_digit_run(rest, 4, 8) {
                    return Some(code);
                }
            }
        }
        // "123456 is your ..."（GitHub 风格）
        let head: String = low.chars().take(24).collect();
        if let Some(code) = first_digit_run(&head, 4, 8) {
            let tail = head[code.len()..].trim_start();
            if tail.starts_with("is your") || tail.starts_with("是您") {
                return Some(code);
            }
        }
    }
    None
}

fn first_digit_run(s: &str, min: usize, max: usize) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n = i - start;
            if (min..=max).contains(&n) {
                return Some(chars[start..i].iter().collect());
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_alphabet_must_be_standard() {
        // 回归：字母表曾把 0123456789 写成 0123456789，所有 base64 解码全错
        assert_eq!(base64_decode("MTIzNDU2").unwrap(), b"123456");
    }

    #[test]
    fn decodes_rfc2047_subject() {
        let s = "=?utf-8?B?6aqM6K+B56CBIDEyMzQ1Ng==?=";
        assert!(decode_header(s).contains("123456"));
    }

    #[test]
    fn extracts_code_after_chinese_keyword() {
        assert_eq!(
            extract_code("验证码", "您的验证码为 456123，5 分钟内有效"),
            Some("456123".into())
        );
        // 只有关键词没有数字时不能瞎猜
        assert_eq!(extract_code("【某某】验证码", "请勿泄露"), None);
    }

    #[test]
    fn extracts_github_style_code() {
        assert_eq!(
            extract_code("123456 is your GitHub launch code", ""),
            Some("123456".into())
        );
    }

    #[test]
    fn ignores_short_numbers() {
        assert_eq!(extract_code("你的订单已发货", "订单号 12，预计 3 天到达"), None);
    }

    #[test]
    fn parses_internal_date() {
        // 标准 IMAP 格式：日期 token 是 "20-Nov-2026"（连字符），不是空格分隔
        let line = "* 45 FETCH (UID 123 INTERNALDATE \"20-Nov-2026 13:05:00 +0800\" BODY[HEADER] {10}";
        let ts = super::parse_internal_date(line);
        assert_eq!(ts, 1795151100, "应正确解析 DD-Mon-YYYY +0800");
        // 同一时刻的 UTC 表示，unix 时间戳必须相等
        let utc = super::parse_internal_date(
            "* 45 FETCH (UID 123 INTERNALDATE \"20-Nov-2026 05:05:00 +0000\" BODY[HEADER] {10}",
        );
        assert_eq!(ts, utc, "不同时区表示同一时刻应得到相同 unix 时间戳");
        // 负时区 -0500 与等价 UTC 时刻
        let neg = super::parse_internal_date(
            "* 1 FETCH (UID 3 INTERNALDATE \"15-Jun-2026 23:30:00 -0500\" BODY[] {5}",
        );
        let utc2 = super::parse_internal_date(
            "* 1 FETCH (UID 3 INTERNALDATE \"16-Jun-2026 04:30:00 +0000\" BODY[] {5}",
        );
        assert_eq!(neg, utc2, "负时区应正确换算");
        // epoch 边界
        assert_eq!(
            super::parse_internal_date("* 1 FETCH (UID 1 INTERNALDATE \"1-Jan-1970 00:00:00 +0000\" BODY[] {5}"),
            0
        );
        // 闰年
        assert_eq!(
            super::parse_internal_date("* 1 FETCH (UID 2 INTERNALDATE \"29-Feb-2000 00:00:00 +0000\" BODY[] {5}"),
            951782400
        );
        // 无 INTERNALDATE → 0
        assert_eq!(super::parse_internal_date("* 1 FETCH (UID 9 BODY[] {5}"), 0);
    }
}
