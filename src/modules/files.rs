//! 文件中转站 + 桌面类型归档（std::fs 完成，全部在本机磁盘进行）

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

pub fn push_file(files: &mut Vec<FileItem>, path: &str, cap: usize) {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    // 去重
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

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ArchiveGroup {
    pub label: String,
    pub count: usize,
}

const CATEGORIES: &[(&str, &[&str])] = &[
    ("文档", &["pdf", "doc", "docx", "txt", "md", "ppt", "pptx", "xls", "xlsx", "csv", "epub"]),
    ("图片", &["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "heic"]),
    ("视频", &["mp4", "mkv", "mov", "avi", "flv", "wmv"]),
    ("音频", &["mp3", "wav", "flac", "m4a", "aac", "ogg"]),
    ("压缩包", &["zip", "rar", "7z", "tar", "gz", "bz2"]),
    ("程序", &["exe", "msi", "bat", "cmd", "lnk", "appx"]),
    ("代码", &["rs", "py", "js", "ts", "html", "css", "json", "java", "c", "cpp", "h", "go", "ipynb", "xml", "yml", "yaml", "toml"]),
];

fn category_of(ext: &str) -> Option<&'static str> {
    let ext = ext.to_lowercase();
    CATEGORIES
        .iter()
        .find(|(_, exts)| exts.iter().any(|e| *e == ext))
        .map(|(label, _)| *label)
}

/// 扫描桌面根目录（不含子目录、快捷方式跳过）
#[allow(dead_code)]
pub fn scan_desktop() -> Vec<ArchiveGroup> {
    let mut groups: Vec<ArchiveGroup> = Vec::new();
    let desktop = desktop_dir();
    let mut other = 0usize;
    if let Ok(rd) = std::fs::read_dir(&desktop) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if ext.eq_ignore_ascii_case("lnk") {
                continue;
            }
            match category_of(&ext) {
                Some(label) => match groups.iter_mut().find(|g| g.label == label) {
                    Some(g) => g.count += 1,
                    None => groups.push(ArchiveGroup { label: label.to_string(), count: 1 }),
                },
                None => other += 1,
            }
        }
    }
    if other > 0 {
        groups.push(ArchiveGroup { label: "其他".into(), count: other });
    }
    groups
}

/// 执行归档：把桌面文件按类型移动到 target 目录下的分类子目录。
/// 返回成功移动的数量。
pub fn run_archive(target_root: &Path) -> usize {
    let desktop = desktop_dir();
    let mut moved = 0usize;
    let entries = match std::fs::read_dir(&desktop) {
        Ok(rd) => rd,
        Err(_) => return 0,
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if ext.eq_ignore_ascii_case("lnk") {
            continue;
        }
        let cat = category_of(&ext).unwrap_or("其他");
        let dir = target_root.join(cat);
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let name = match p.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        let mut dest = dir.join(&name);
        // 同名冲突：追加序号
        let mut i = 1;
        while dest.exists() {
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());
            let ext2 = if ext.is_empty() { String::new() } else { format!(".{}", ext) };
            dest = dir.join(format!("{} ({}).{}", stem, i, ext2.trim_start_matches('.')));
            i += 1;
            if i > 999 {
                break;
            }
        }
        if std::fs::rename(&p, &dest).is_ok() {
            moved += 1;
        }
    }
    moved
}
