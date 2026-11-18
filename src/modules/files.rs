//! 文件中转站（std::fs 完成，全部在本机磁盘进行）

use crate::store::FileItem;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use windows::core::w;

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 文件中转站容量上限【默认值】（可在设置里调整）。
/// 只存路径引用不占内存，此上限仅避免列表过长，非内存约束。
pub const DEFAULT_FILEHUB_CAP: usize = 20;

/// 加入中转站的结果。
#[derive(PartialEq, Eq, Debug)]
pub enum PushResult {
    /// 新文件已加入
    Added,
    /// 已存在的路径被刷新到顶部（不算新增，满也允许）
    Refreshed,
    /// 中转站已满且是新文件 —— 拒绝，不挤掉已有条目
    Full,
}

/// 中转站专用：满额时拒绝新文件（不淘汰旧条目），但允许刷新已存在的路径。
pub fn try_push_file(files: &mut Vec<FileItem>, path: &str, cap: usize) -> PushResult {
    let existed = files.iter().any(|f| f.path == path);
    if !existed && files.len() >= cap {
        return PushResult::Full;
    }
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    files.retain(|f| f.path != path);
    files.insert(
        0,
        FileItem {
            path: path.to_string(),
            name,
            size,
            ts: now_unix(),
        },
    );
    if files.len() > cap {
        files.truncate(cap);
    }
    if existed { PushResult::Refreshed } else { PushResult::Added }
}

#[allow(dead_code)]
pub fn fmt_size(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1048576.0)
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

/// 桌面路径
#[allow(dead_code)]
pub fn desktop_dir() -> PathBuf {
    // 尊重用户的实际桌面（ OneDrive 重定向场景下读取 Shell Folders 注册表）
    #[cfg(windows)]
    {
        use windows::Win32::System::Registry::*;
        unsafe {
            let mut hkey = Default::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders"),
                None,
                KEY_READ,
                &mut hkey,
            )
            .is_ok()
            {
                let mut buf = [0u16; 512];
                let mut len = (buf.len() * 2) as u32;
                let mut ty = REG_VALUE_TYPE::default();
                if RegQueryValueExW(hkey, w!("Desktop"), None, Some(&mut ty), Some(buf.as_mut_ptr() as _), Some(&mut len)).is_ok()
                {
                    let _ = RegCloseKey(hkey);
                    let s = String::from_utf16_lossy(&buf[..(len as usize / 2).min(512)]);
                    let s = s.trim_end_matches('\0');
                    if !s.is_empty() {
                        return PathBuf::from(s);
                    }
                } else {
                    let _ = RegCloseKey(hkey);
                }
            }
        }
    }
    dirs_fallback()
}

fn dirs_fallback() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(|u| PathBuf::from(u).join("Desktop"))
        .unwrap_or_else(|_| PathBuf::from("."))
}
