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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::store::MailCode;

/// 首轮拉取是否还没完成。首轮只做一件事：确定「服务器视角的启动时刻」基准。
static FIRST_POLL: AtomicBool = AtomicBool::new(true);

/// 「启动时刻」基准，取**邮件服务器时钟**（INTERNALDATE）。
///
/// 设计很简单：程序启动后第一轮拉取，取收件箱里最新一封邮件的 INTERNALDATE 作为
/// 基准 T0；此后只要有邮件的 INTERNALDATE > T0，就认为它是「启动后新到的」，
/// 提取其中的验证码弹出来。
///
/// 为什么基准取自服务器邮件而不是本机时间：INTERNALDATE 是服务器记录的到达时间，
/// 拿它和本机启动时间比会有两端时钟偏差（旧版踩过坑，新邮件被误判成旧的收不到）。
/// 基准和比较值都用 INTERNALDATE，同一个时钟源，偏差问题从根上不存在。
///
/// 另存基准邮件的 UID 作次级判断：INTERNALDATE 精度只到秒，同一秒内到达的新邮件
/// date 会等于基准，靠 UID 更大来识别，避免漏掉。
static BASE_TS: AtomicI64 = AtomicI64::new(0);
static BASE_UID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[derive(Default, Clone)]
pub struct MailShared {
    pub latest: Option<MailCode>,
    /// 邮件线程每抓到一封「新验证码邮件」就 +1。主界面只提示序号增量。
    pub seq: u64,
    /// 主界面已处理的序号（主界面写入，供状态栏诊断）
    pub ui_seq: u64,
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
        // 必须兜住 panic：poll_once 里任何一处切片/解析崩溃都会让线程直接退出，
        // 之后再也不轮询（表现为「诊断停在某个时间、收到邮件却不通知」）。
        // 用 catch_unwind 把每轮包起来，出错只跳过这一轮，30 秒后照常重试。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            poll_once(&cfg, &shared)
        }));
        match result {
            Ok(Ok(diag)) => {
                let wait = cfg.poll_seconds.clamp(10, 600) as u64;
                // 状态里带上拉取诊断，用户能直接看到监听是否在跑、拉到几封、卡在哪
                set_status(&shared, format!("监听中 · 每 {wait} 秒 · {diag}"));
                std::thread::sleep(Duration::from_secs(wait));
            }
            Ok(Err(e)) => {
                set_status(&shared, format!("连接失败：{e}（30 秒后重试）"));
                std::thread::sleep(Duration::from_secs(30));
            }
            Err(_) => {
                set_status(&shared, "内部解析异常，本轮跳过（30 秒后重试）");
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
            Ok((n, _)) => set_only_status(format!("连接成功 · 「{}」共 {} 封邮件", if m.folder.trim().is_empty() { "INBOX" } else { m.folder.trim() }, n)),
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
    let (exists, _uidnext) = conn.select(folder)?;

    // 按序号拉最近 30 封（绝不用 UID 范围查询——QQ 邮箱上会返回 0 封）
    let msgs = conn.fetch_since(0, exists)?;

    // 首轮：取收件箱里「最新一封」邮件的 (INTERNALDATE, UID) 作为启动基准，不弹验证码。
    // "最新"按 (date, uid) 字典序取最大。之后只有 (date, uid) 更大的才算启动后新到。
    let first = FIRST_POLL
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    if first {
        let mut best: Option<(i64, u32)> = None;
        for m in &msgs {
            if m.date <= 0 {
                continue;
            }
            let better = best
                .map(|(d, u)| m.date > d || (m.date == d && m.uid > u))
                .unwrap_or(true);
            if better {
                best = Some((m.date, m.uid));
            }
        }
        if let Some((d, u)) = best {
            BASE_TS.store(d, Ordering::SeqCst);
            BASE_UID.store(u, Ordering::SeqCst);
        }
    }
    let base_ts = BASE_TS.load(Ordering::SeqCst);
    let base_uid = BASE_UID.load(Ordering::SeqCst);

    // 找出「启动后新到」的验证码邮件：(date, uid) 晚于/大于基准。
    let mut newest: Option<(i64, u32, MailCode)> = None;
    let fetched = msgs.len();
    let mut skipped_old = 0usize;
    let mut new_cnt = 0usize;
    let mut code_hits = 0usize;
    let mut latest_new_subject: Option<String> = None;
    let mut latest_candidates: Option<Vec<String>> = None;
    for m in &msgs {
        if first {
            skipped_old += 1;
            continue;
        }
        // date 解析失败的邮件跳过（无法判断是否新到）
        if m.date <= 0 {
            skipped_old += 1;
            continue;
        }
        // 与基准比较：date 更大，或同秒但 UID 更大，才算新邮件
        let is_after_base = m.date > base_ts || (m.date == base_ts && m.uid > base_uid);
        if !is_after_base {
            skipped_old += 1;
            continue;
        }
        new_cnt += 1;
        latest_new_subject = Some(truncate_subject(&m.subject, 12));
        if let Some(code) = extract_code(&m.subject, &m.body) {
            code_hits += 1;
            latest_candidates = None;
            let is_newer = newest
                .as_ref()
                .map(|(d, u, _)| m.date > *d || (m.date == *d && m.uid > *u))
                .unwrap_or(true);
            if is_newer {
                newest = Some((
                    m.date,
                    m.uid,
                    MailCode {
                        code,
                        from: clean_from(&m.from),
                        ts: crate::modules::files::now_unix(),
                        uid: m.uid,
                    },
                ));
            }
        } else if latest_candidates.is_none() {
            // 没解析出验证码：记下这封正文里的数字候选，供状态栏诊断
            latest_candidates = Some(digit_candidates(&m.body));
        }
    }

    let newest_uid = newest.as_ref().map(|(_, u, _)| *u);
    if let Some((_, _, mc)) = newest {
        if let Ok(mut s) = shared.lock() {
            s.latest = Some(mc);
            s.seq = s.seq.saturating_add(1);
        }
    } else if let Ok(mut s) = shared.lock() {
        // 没有新验证码时保留已有展示内容，不覆盖
        if s.latest.is_none() {
            // 用 store 里可能存在的历史值兜底（不含则空）
            s.latest = crate::store::Store::load().mail_code.clone();
        }
    }

    // 诊断串：拉到几封 / 跳过几封旧 / 基准时间 / seq / 最近一封主题和到达时间
    let (seq, ui_seq) = shared
        .lock()
        .map(|s| (s.seq, s.ui_seq))
        .unwrap_or((0, 0));
    let recent = msgs
        .last()
        .map(|m| format!(" · 最近「{}」{}", truncate_subject(&m.subject, 10), m.date))
        .unwrap_or_default();
    // 新 {new_cnt} = 通过「晚于基准」判断的邮件数；命中 {code_hits} = 其中解析出验证码的封数。
    // 三者组合可一眼定位：新=0→时间判断拦住；新>0命中=0→正文/关键词解析失败。
    let new_info = if first {
        String::new()
    } else {
        let subj = latest_new_subject
            .map(|s| format!(" · 新邮件「{s}」"))
            .unwrap_or_default();
        // 命中 0 时附上正文里的数字候选，一眼看出是不是提取规则没覆盖到
        let cand = if code_hits == 0 {
            latest_candidates
                .filter(|v| !v.is_empty())
                .map(|v| format!(" · 候选[{}]", v.join(",")))
                .unwrap_or_default()
        } else {
            String::new()
        };
        format!(" · 新 {new_cnt} 命中 {code_hits}{subj}{cand}")
    };
    let diag = format!(
        "拉到 {fetched} 封 · 旧 {skipped_old} · 基准 {base_ts} · seq {seq}/{ui_seq}{}{}{}{}",
        if first { " · 首轮对齐" } else { "" },
        if let Some(u) = newest_uid {
            format!(" · 验证码 UID {u}")
        } else {
            String::new()
        },
        new_info,
        recent,
    );
    Ok(diag)
}

/// 收集文本里所有 4~8 位数字串（去重，最多 8 个），用于状态栏诊断：
/// 若「命中 0」但候选里有 6 位数字，就能确认是提取规则问题而非邮件没正文。
fn digit_candidates(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n = i - start;
            if (4..=8).contains(&n) {
                let v: String = chars[start..i].iter().collect();
                if !out.contains(&v) {
                    out.push(v);
                }
            }
        } else {
            i += 1;
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

/// 截断主题用于状态栏诊断（按字符，兼容中文）
fn truncate_subject(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
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

    /// 返回 (邮件总数 exists, 下一个将分配的 UID UIDNEXT)。
    /// UIDNEXT 是判断「启动前 vs 启动后」的可靠依据：SELECT 完成时，
    /// 所有 UID < UIDNEXT 的邮件都已存在（旧邮件），之后到达的邮件 UID >= UIDNEXT。
    fn select(&mut self, folder: &str) -> Result<(u32, u32), String> {
        let resp = self.cmd(&format!("SELECT \"{}\"", imap_escape(folder)))?;
        if !is_ok(&resp) {
            return Err(format!("打不开邮箱文件夹「{folder}」：{}", first_tag_line(&resp)));
        }
        let mut exists = 0u32;
        let mut uidnext = 0u32;
        for line in resp.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("* ") {
                if let Some(n_str) = rest.strip_suffix(" EXISTS") {
                    if let Ok(n) = n_str.trim().parse() {
                        exists = n;
                    }
                }
                // * OK [UIDNEXT 12345] Predicted next UID
                if let Some(p) = rest.find("UIDNEXT ") {
                    let digits: String = rest[p + 8..]
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(n) = digits.parse() {
                        uidnext = n;
                    }
                }
            }
        }
        Ok((exists, uidnext))
    }

    /// 拉取最近若干封邮件（按序号，不用 UID 范围）。
    ///
    /// 关键教训：绝不能用 `UID FETCH {last_uid+1}:*` 来「只取新邮件」——
    /// QQ 邮箱上这经常返回 0 封（状态栏「拉到 0 封」），因为 UID 与序号不是线性对应，
    /// 且水位一旦偏高就永远拉不到新邮件。改为每轮都按**序号**拉最近 30 封，
    /// 保证只要新邮件在收件箱里就一定被抓到，新旧判断交给调用方用 UID + INTERNALDATE 做。
    fn fetch_since(&mut self, _last_uid: u32, exists: u32) -> Result<Vec<MailMsg>, String> {
        if exists == 0 {
            return Ok(Vec::new());
        }
        let word = "FETCH";
        let range = format!("{}:{}", exists.saturating_sub(29).max(1), exists);
        let tag = self.next_tag();
        // 正文抓 16KB：QQ 邮箱验证码邮件多为 HTML，base64 编码后验证码常被推到 4KB 之后，
        // 只取 4096 字节会把验证码整段截掉（AKY 邮件拉到了却解析不出码的根因之一）。
        let req = format!(
            "{} {} {} (UID INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)] BODY.PEEK[TEXT]<0.16384>)\r\n",
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
/// 这里把「解码后的文本」和「原文」都拼在一起返回，交给 extract_code 逐个尝试。
/// 关键：一封 multipart 邮件可能有**多个** base64 段（纯文本段 + HTML 段），
/// 验证码常常只在其中一段里；旧实现只解第一个 "base64" 之后的块，
/// 于是「拉到了邮件却解析不出验证码」。这里遍历所有段。
fn decode_body(raw: &str) -> String {
    let lower = raw.to_lowercase();
    // 收集所有「原始文本来源」：base64 解码段、quoted-printable、原文。
    // 关键修复：对**每一个**来源都要做 HTML 剥离——旧实现只对未解码的 raw 剥离，
    // 而 base64 解码后的 HTML（验证码数字常被拆进 <span> 里）从未被剥离，
    // 于是「拉到邮件、时间判断也通过，就是解析不出验证码」。
    let mut sources: Vec<String> = Vec::new();

    // 1) 遍历所有 base64 传输编码段
    let mut search = 0usize;
    while let Some(rel) = lower[search..].find("base64") {
        let pos = search + rel;
        let line_end = raw[pos..].find('\n').map(|i| pos + i + 1).unwrap_or(raw.len());
        let chunk: String = raw[line_end..]
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
                sources.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        search = line_end;
        if search >= raw.len() {
            break;
        }
    }

    // 2) quoted-printable
    if lower.contains("quoted-printable") {
        sources.push(String::from_utf8_lossy(&q_decode(raw)).into_owned());
    }

    // 3) 原文
    sources.push(raw.to_string());

    // 对每个来源生成三种视图，全部拼给 extract_code 逐个尝试：
    //   a) 原文
    //   b) 紧凑剥离 HTML（标签直接删掉）：<span>4</span><span>7</span> -> 47
    //      验证码常被拆进多个标签，紧凑版才能拼回连续数字。
    //   c) 空格剥离 HTML：<td>1</td><td>2</td> -> "1 2"（避免瞎拼出假数字）
    let mut out = String::new();
    for src in &sources {
        out.push_str(src);
        out.push('\n');
        out.push_str(&strip_html(src, ""));
        out.push('\n');
        out.push_str(&strip_html(src, " "));
        out.push('\n');
    }
    out
}

/// 去掉 HTML 标签，标签位置替换为 `sep`。
/// sep="" 得到紧凑版（数字被标签分开时能拼回），sep=" " 得到保守版。
fn strip_html(s: &str, sep: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push_str(sep);
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
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

// 验证码提取：借鉴业界成熟做法（Thunderbird Vericode / mailhook /
// @onedaydevelopers/otp-detector / lumbox），核心是「候选 + 邻近关键词评分 + 置信不足不弹」。
// 绝不用「全文找第一个数字」——那会把订单号 929292、CSS 颜色 #123456、年份当成验证码。

/// 正向词：出现在数字附近会加分（验证码语义）
const POS_KEYWORDS: &[&str] = &[
    "验证码", "校验码", "动态码", "确认码", "激活码", "认证码", "安全码", "动态密码",
    "一次性密码", "一次性验证码", "口令", "验证码为", "输入的验证", "请输入", "填写",
    "输入以下", "验证", "校验", "认证",
    "verification code", "one-time code", "one time code", "security code",
    "login code", "sign-in code", "verification", "verify", "passcode", "one-time",
    "otp", "is your", "enter this code", "use this code", "code", "pin",
];

/// 负向词：紧邻数字出现会减分（订单/电话/地址等干扰数字）
const NEG_KEYWORDS: &[&str] = &[
    "订单", "订单号", "单号", "发票", "收据", "工单", "参考号", "流水号", "追踪号",
    "电话", "手机", "手机号", "联系电话", "地址", "收货", "金额", "付款", "支付",
    "余额", "账单", "会员号", "账户号", "order", "invoice", "ticket", "case", "ref",
    "reference", "tracking", "phone", "tel", "address", "support", "amount", "price",
    "payment", "usd", "cny", "customer",
];

/// 文本归一化：去零宽字符、全角数字转半角
fn normalize_text(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}') {
            continue;
        }
        if ('\u{ff10}'..='\u{ff19}').contains(&c) {
            out.push(char::from_u32(c as u32 - 0xff10 + '0' as u32).unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Clone)]
struct CodeCandidate {
    code: String,
    start: usize, // 字符下标
    end: usize,   // 字符下标（不含）
}

/// 生成所有 4~8 位数字候选（按字符处理，天然规避 UTF-8 切片 panic）；
/// 支持 "482 913" / "482-913" 这种 3-3 分隔写法拼回一个 6 位码。
fn code_candidates(chars: &[char]) -> Vec<CodeCandidate> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let n = i - start;
            let mut code: String = chars[start..i].iter().collect();
            let mut end = i;
            if n == 3 {
                let mut j = i;
                while j < chars.len()
                    && (chars[j] == ' ' || chars[j] == '-' || chars[j] == '\u{00a0}')
                {
                    j += 1;
                }
                let ds = j;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                if j > ds && j - ds == 3 {
                    code.extend(chars[ds..j].iter());
                    end = j;
                    i = j;
                }
            }
            if (4..=8).contains(&code.len()) {
                v.push(CodeCandidate { code, start, end });
            }
        } else {
            i += 1;
        }
    }
    v
}

/// 给候选打分：距正向词越近分越高；紧邻负向词减分；
/// CSS 颜色 / URL / 年份直接重罚。分数 < 3 视为不可信，不弹。
fn score_candidate(chars: &[char], c: &CodeCandidate) -> i32 {
    let mut s = 0i32;

    let mut best_pos_dist: Option<usize> = None;
    for k in POS_KEYWORDS {
        let lo = c.start.saturating_sub(60);
        let hi = (c.end + 30).min(chars.len());
        let window: String = chars[lo..hi].iter().collect();
        let lw = window.to_lowercase();
        let kl = k.to_lowercase();
        if let Some(p) = lw.find(&kl) {
            let kw_start = lo + lw[..p].chars().count();
            let kw_end = kw_start + kl.chars().count();
            let dist = if kw_end <= c.start {
                c.start - kw_end
            } else if kw_start >= c.end {
                kw_start - c.end
            } else {
                0
            };
            best_pos_dist = Some(best_pos_dist.map_or(dist, |bd| dist.min(bd)));
        }
    }
    let very_close = matches!(best_pos_dist, Some(d) if d <= 6);
    match best_pos_dist {
        Some(d) if d <= 12 => s += 6,
        Some(d) if d <= 30 => s += 4,
        Some(d) if d <= 60 => s += 2,
        _ => {}
    }

    // 负向词只在候选前方 12 字符内生效；紧贴正向词时惩罚减半
    let nlo = c.start.saturating_sub(12);
    let nwin: String = chars[nlo..c.start].iter().collect();
    let nw = nwin.to_lowercase();
    let neg_penalty = if very_close { 2 } else { 4 };
    for k in NEG_KEYWORDS {
        if nw.contains(&k.to_lowercase()) {
            s -= neg_penalty;
        }
    }

    if c.start > 0 && chars[c.start - 1] == '#' {
        s -= 10; // CSS 颜色 #123456
    }
    let lo = c.start.saturating_sub(40);
    let hi = (c.end + 40).min(chars.len());
    let win: String = chars[lo..hi].iter().collect();
    let w = win.to_lowercase();
    if w.contains("http") || w.contains("www.") || w.contains(".com/") || w.contains(".cn/") {
        s -= 6; // URL 里的数字
    }
    if c.code.len() == 4 {
        if let Ok(y) = c.code.parse::<i32>() {
            if (1900..=2099).contains(&y) {
                s -= 4; // 年份
            }
        }
    }
    if c.code.len() == 6 {
        s += 1; // 6 位最常见
    }
    s
}

/// 从主题/正文中提取验证码（本地启发式，绝不外传内容）。
/// 主题权重更高（更简洁、干扰少）；所有候选取最高分，低于阈值则放弃——宁可不弹也不弹错。
pub fn extract_code(subject: &str, body: &str) -> Option<String> {
    let subject = normalize_text(subject);
    let body = normalize_text(body);
    let mut best: Option<(i32, i32, String)> = None; // (分数, 主题加权, 验证码)
    for (text, subj_bonus) in [(subject.as_str(), 1i32), (body.as_str(), 0i32)] {
        let chars: Vec<char> = text.chars().collect();
        for c in code_candidates(&chars) {
            let mut sc = score_candidate(&chars, &c);
            if subj_bonus > 0 {
                sc += subj_bonus;
            }
            let better = match &best {
                None => true,
                Some((bs, bb, _)) => sc > *bs || (sc == *bs && subj_bonus > *bb),
            };
            if better {
                best = Some((sc, subj_bonus, c.code));
            }
        }
    }
    const THRESHOLD: i32 = 3;
    match best {
        Some((s, _, code)) if s >= THRESHOLD => Some(code),
        _ => None,
    }
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
