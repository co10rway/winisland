//! 极简 HTTPS 客户端 —— 下载公开节假日日历、拉取日历订阅、访问用户自己的 CalDAV 服务器。
//!
//! 复用邮箱模块已引入的 rustls（证书静态编译进 exe），不新增任何依赖；
//! 只连用户配置的那一个地址，不发 cookie、不采集本机信息。

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

/// GET https://{host}{path}，只在 200 时返回响应体（UTF-8 无损转换）。
/// 超时 8 秒；任何错误都返回 None，调用方自行降级。
pub fn https_get(host: &str, path: &str) -> Option<String> {
    let (code, body) = https_req(host, path, "GET", &[], None)?;
    if code != 200 {
        return None;
    }
    Some(body)
}

/// 通用 HTTPS 请求：任意方法 + 自定义头 + 可选请求体，返回 (状态码, 响应体)。
/// 仅用于访问用户自己配置的地址（日历订阅 / CalDAV 服务器），不发送任何本机信息。
pub fn https_req(
    host: &str,
    path: &str,
    method: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
) -> Option<(u16, String)> {
    use rustls::pki_types::ServerName;

    let tcp = std::net::TcpStream::connect((host, 443u16)).ok()?;
    tcp.set_read_timeout(Some(Duration::from_secs(12))).ok();
    tcp.set_write_timeout(Some(Duration::from_secs(12))).ok();

    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let sn = ServerName::try_from(host.to_string()).ok()?;
    let mut tls = rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(cfg), sn).ok()?,
        tcp,
    );

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: WinIsland/{}\r\nAccept: */*\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
        env!("CARGO_PKG_VERSION")
    );
    for (k, v) in headers {
        req.push_str(k);
        req.push_str(": ");
        req.push_str(v);
        req.push_str("\r\n");
    }
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\n",
            b.as_bytes().len()
        ));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    tls.write_all(req.as_bytes()).ok()?;
    tls.flush().ok()?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 4_000_000 {
                    break; // 防御：日历 JSON 不该这么大
                }
            }
            Err(_) => break, // 服务器直接断开时也能拿到已收到的部分
        }
    }
    parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> Option<(u16, String)> {
    // 找响应头结束标记 \r\n\r\n
    let sep = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..sep]).to_string();
    let body = &buf[sep + 4..];

    // 状态行：HTTP/1.1 200 OK / 207 Multi-Status / 401 Unauthorized
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if code == 0 {
        return None;
    }
    let lower = head.to_ascii_lowercase();
    let body = if lower.contains("transfer-encoding: chunked") {
        dechunk(body)
    } else {
        body.to_vec()
    };
    Some((code, String::from_utf8_lossy(&body).to_string()))
}

/// 解析 chunked 编码（jsdelivr 常见）
fn dechunk(mut src: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(pos) = src.windows(2).position(|w| w == b"\r\n") else { break };
        let line = String::from_utf8_lossy(&src[..pos]).to_string();
        let size_str = line.split(';').next().unwrap_or("").trim().to_string();
        let size = match usize::from_str_radix(&size_str, 16) {
            Ok(v) => v,
            Err(_) => break,
        };
        src = &src[pos + 2..];
        if size == 0 {
            break;
        }
        if src.len() < size {
            out.extend_from_slice(src);
            break;
        }
        out.extend_from_slice(&src[..size]);
        src = &src[size..];
        // 跳过块尾 CRLF
        if src.starts_with(b"\r\n") {
            src = &src[2..];
        }
    }
    out
}
