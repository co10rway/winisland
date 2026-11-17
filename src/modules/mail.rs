//! 邮箱验证码 — IMAPS 拉取（rustls 静态编译），仅本地解析邮件主题/正文前 2KB
//! 中的验证码数字；除登录必需外不上传任何数据。

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::store::MailCode;

#[derive(Default, Clone)]
pub struct MailShared {
    pub latest: Option<MailCode>,
    pub status: String, // "未配置" / "监听中" / "上次错误: ..."
}

pub type SharedMail = Arc<Mutex<MailShared>>;

pub fn spawn(shared: SharedMail) {
    std::thread::spawn(move || loop {
        let cfg = crate::config::Config::load().mail;
        let enabled = crate::config::Config::load().modules.mailcode;
        if !enabled || cfg.imap_host.is_empty() || cfg.user.is_empty() {
            if let Ok(mut s) = shared.lock() {
                s.status = if enabled {
                    "未配置 · 请在 config.json 填写 IMAP 信息".into()
                } else {
                    "已关闭".into()
                };
            }
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        match poll_once(&cfg, &shared) {
            Ok(()) => {
                if let Ok(mut s) = shared.lock() {
                    s.status = "监听中".into();
                }
                std::thread::sleep(Duration::from_secs(cfg.poll_seconds.max(20) as u64));
            }
            Err(e) => {
                if let Ok(mut s) = shared.lock() {
                    s.status = format!("连接失败: {e}（60 秒后重试）");
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    });
}

fn poll_once(cfg: &crate::config::Mail, shared: &SharedMail) -> Result<(), String> {
    use rustls::pki_types::ServerName;

    let server_name = ServerName::try_from(cfg.imap_host.clone())
        .map_err(|_| "服务器名无效".to_string())?;
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = std::net::TcpStream::connect((cfg.imap_host.as_str(), cfg.imap_port))
        .map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let mut tls = rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|e| e.to_string())?,
        tcp,
    );

    let mut conn = ImapConn { stream: &mut tls, tag: 0 };

    conn.read_resp()?; // greeting
    conn.cmd(&format!("LOGIN \"{}\" \"{}\"", imap_escape(&cfg.user), imap_escape(&cfg.password)))?;
    let exists = conn.select(&cfg.folder)?;
    let from = exists.saturating_sub(4).max(1);
    let (headers, bodies) = conn.fetch_range(from, exists)?;

    let store = crate::store::Store::load();
    let mut seen = store.seen_mail_ids.clone();
    let mut latest = store.mail_code.clone();

    for i in 0..headers.len().min(bodies.len()) {
        let (from, subject) = &headers[i];
        let text = &bodies[i];
        let key = format!("{}|{}", from, subject);
        if seen.iter().any(|s| *s == key) {
            continue;
        }
        if let Some(code) = extract_code(subject, text) {
            seen.push(key.clone());
            if seen.len() > 50 {
                seen.drain(..seen.len() - 50);
            }
            latest = Some(MailCode {
                code,
                from: clean_from(from),
                ts: crate::modules::files::now_unix(),
            });
        }
    }

    if let Ok(mut s) = shared.lock() {
        s.latest = latest.clone();
    }
    let mut st = crate::store::Store::load();
    st.seen_mail_ids = seen;
    st.mail_code = latest;
    st.save();
    Ok(())
}

struct ImapConn<'a> {
    stream: &'a mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
    tag: u32,
}

impl<'a> ImapConn<'a> {
    fn read_resp(&mut self) -> Result<String, String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = self.stream.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // 简化：读到以 CRLF 结尾且包含 tag 完成行或一定量后停
            if buf.ends_with(b"\r\n") {
                let s = String::from_utf8_lossy(&buf).to_string();
                // 连续多行响应（如 FETCH）：尝试在 300ms 静默后认为读完
                if pending_more(self.stream) {
                    continue;
                }
                return Ok(s);
            }
            if buf.len() > 4 * 1024 * 1024 {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    fn cmd(&mut self, command: &str) -> Result<String, String> {
        self.tag += 1;
        let tag = format!("a{:03}", self.tag);
        let line = format!("{} {}\r\n", tag, command);
        self.stream
            .write_all(line.as_bytes())
            .map_err(|e| e.to_string())?;
        self.stream.flush().ok();
        self.read_resp()
    }

    fn select(&mut self, folder: &str) -> Result<u32, String> {
        let resp = self.cmd(&format!("SELECT \"{}\"", folder))?;
        for line in resp.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("* ") {
                if let Some(n_str) = rest.strip_suffix(" EXISTS") {
                    if let Ok(n) = n_str.trim().parse() {
                        return Ok(n);
                    }
                }
            }
        }
        Ok(0)
    }

    fn fetch_range(&mut self, from: u32, to: u32) -> Result<(Vec<(String, String)>, Vec<String>), String> {
        let resp = self.cmd(&format!(
            "FETCH {}:{} (BODY.PEEK[HEADER.FIELDS (FROM SUBJECT)] BODY.PEEK[TEXT]<0.2048>)",
            from, to
        ))?;
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut bodies: Vec<String> = Vec::new();
        let mut cur_from = String::new();
        let mut cur_subj = String::new();
        let mut in_header = false;
        let mut body_acc = String::new();
        for line in resp.lines() {
            if line.starts_with("* ") && line.contains("FETCH (") {
                if !cur_subj.is_empty() || !cur_from.is_empty() {
                    headers.push((std::mem::take(&mut cur_from), std::mem::take(&mut cur_subj)));
                    bodies.push(std::mem::take(&mut body_acc));
                }
                in_header = true;
                continue;
            }
            if in_header {
                let low = line.to_lowercase();
                if low.starts_with("from:") {
                    cur_from = line[5..].trim().to_string();
                } else if low.starts_with("subject:") {
                    cur_subj = decode_subj(line[8..].trim());
                } else if low.starts_with("body[") || low.starts_with(") ") {
                    in_header = false;
                }
            } else if !line.is_empty() && !line.starts_with("a0") {
                if body_acc.len() < 4096 {
                    body_acc.push_str(line);
                    body_acc.push('\n');
                }
            }
        }
        if !cur_subj.is_empty() || !cur_from.is_empty() {
            headers.push((cur_from, cur_subj));
            bodies.push(body_acc);
        }
        Ok((headers, bodies))
    }
}

/// 250ms 静默检测：TLS 流是否还有更多数据可读
fn pending_more(stream: &mut rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>) -> bool {
    stream.sock.set_read_timeout(Some(Duration::from_millis(250))).ok();
    let mut probe = [0u8; 1];
    let more = matches!(stream.read(&mut probe), Ok(0)) == false;
    stream.sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
    more
}

fn imap_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn clean_from(s: &str) -> String {
    // "GitHub <noreply@github.com>" -> "noreply@github.com"
    if let Some(a) = s.rfind('<') {
        if let Some(b) = s.rfind('>') {
            if b > a {
                return s[a + 1..b].to_string();
            }
        }
    }
    s.trim().to_string()
}

fn decode_subj(s: &str) -> String {
    // 只处理 =?utf-8?B?...?= 与 =?UTF-8?Q?...?= 的常见形态
    let mut out = s.to_string();
    let low = s.to_lowercase();
    if let (Some(bpos), Some(eidx)) = (low.find("?b?"), s.rfind("?=")) {
        if let Some(start) = s.find("=?") {
            if let Ok(bytes) = base64_decode(&s[bpos + 3..eidx]) {
                if let Ok(txt) = String::from_utf8(bytes) {
                    out = txt;
                }
            }
            let _ = start;
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
        if ch == b'=' || ch == b'\r' || ch == b'\n' || ch == b' ' {
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
    Ok(out)
}

/// 从主题/正文中提取验证码（本地启发式，绝不外传内容）
pub fn extract_code(subject: &str, body: &str) -> Option<String> {
    for text in [subject, body] {
        if text.is_empty() {
            continue;
        }
        // "验证码 123456" / "验证码: 123456" / "code is 123456"
        for kw in ["验证码", "码是", "code is", "verification code", "code:"] {
            let low = text.to_lowercase();
            if let Some(pos) = low.find(kw) {
                let rest = &text[pos + kw.len()..];
                if let Some(code) = first_digit_run(rest, 4, 8) {
                    return Some(code);
                }
            }
        }
        // "123456 is your ..."（GitHub 风格）
        if let Some(code) = first_digit_run(&text[..text.len().min(20)], 4, 8) {
            if text[code.len()..].trim_start().to_lowercase().starts_with("is your") {
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
