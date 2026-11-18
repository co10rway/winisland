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

/// 时间粒度：把 unix 秒格式化为时间目录名。
/// gran="month" → "2026-11"，否则 "→ 2026"。
#[allow(dead_code)]
pub fn time_folder(ts_secs: i64, gran: &str) -> String {
    let (y, m) = local_ym(ts_secs);
    if gran == "month" {
        format!("{y:04}-{m:02}")
    } else {
        format!("{y}")
    }
}

/// 本机 UTC 时区偏移（秒）。桌面文件修改时间多为近期，用当前偏移近似即可。
fn local_offset_secs() -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::SystemInformation::GetLocalTime;
    use windows::Win32::System::Time::SystemTimeToFileTime;
    let now = now_unix();
    unsafe {
        let st = GetLocalTime();
        let mut ft = FILETIME::default();
        if SystemTimeToFileTime(&st, &mut ft).is_ok() {
            let h = ft.dwHighDateTime as i64;
            let l = ft.dwLowDateTime as i64;
            let secs = ((h << 32) | l) / 10_000_000 - 11644473600;
            return secs - now;
        }
    }
    0
}

/// unix 秒 → 本地(年, 月)。
fn local_ym(ts_secs: i64) -> (u16, u16) {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::FileTimeToSystemTime;
    let local = ts_secs + local_offset_secs();
    let ft100 = local * 10_000_000 + 116444736000000000;
    let ft = FILETIME {
        dwLowDateTime: (ft100 & 0xFFFF_FFFF) as u32,
        dwHighDateTime: ((ft100 >> 32) & 0xFFFF_FFFF) as u32,
    };
    unsafe {
        let mut st = SYSTEMTIME::default();
        if FileTimeToSystemTime(&ft, &mut st).is_ok() {
            return (st.wYear, st.wMonth);
        }
    }
    (0, 0)
}

/// 归档计划中的单个文件：源、目标（时间/类型 下的最终路径）、统计字段。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ArchItem {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub time: String,
    pub ty: String,
    pub size: u64,
}

/// 预览用的分组统计行：某个「时间 / 类型」组合下有多少文件、多大。
#[derive(Clone, Debug, Default)]
pub struct ArchTypeRow {
    pub time: String,
    pub ty: String,
    pub count: usize,
    pub bytes: u64,
}

/// 一次扫描的归档计划（只描述，不执行）。
#[derive(Clone)]
pub struct ArchivePlan {
    pub items: Vec<ArchItem>,
    pub rows: Vec<ArchTypeRow>,
    pub total: usize,
    pub total_bytes: u64,
}

/// 扫描桌面，生成归档计划（不移动任何文件）。
/// 只处理桌面根层级的文件（文件夹跳过）；排除 .lnk / desktop.ini / 位于归档目录内的文件。
/// 目标路径 = target_root / 时间 / 类型 / 文件名（时间→类型 结构）。
#[allow(dead_code)]
pub fn scan_arch_plan(target_root: &Path, gran: &str) -> ArchivePlan {
    let mut plan = ArchivePlan { items: vec![], rows: vec![], total: 0, total_bytes: 0 };
    let desktop = desktop_dir();
    let entries = match std::fs::read_dir(&desktop) {
        Ok(rd) => rd,
        Err(_) => return plan,
    };
    for e in entries.flatten() {
        let p = e.path();
        // 本次不处理文件夹：只归档桌面根层级的文件
        if !p.is_file() {
            continue;
        }
        let name = match p.file_name().map(|s| s.to_string_lossy().to_string()) {
            Some(n) => n,
            None => continue,
        };
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let ext = p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if ext.eq_ignore_ascii_case("lnk") {
            continue;
        }
        // 归档目录若恰好设在桌面内，跳过它内部的文件，避免归档自身
        if p.starts_with(target_root) {
            continue;
        }
        let ty = category_of(&ext).unwrap_or("其他").to_string();
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or_else(now_unix);
        let time = time_folder(mtime, gran);
        let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let dest = target_root.join(&time).join(&ty).join(&name);

        match plan.rows.iter_mut().find(|r| r.time == time && r.ty == ty) {
            Some(r) => {
                r.count += 1;
                r.bytes += size;
            }
            None => plan.rows.push(ArchTypeRow { time: time.clone(), ty: ty.clone(), count: 1, bytes: size }),
        }
        plan.items.push(ArchItem { src: p, dest, time, ty, size });
        plan.total += 1;
        plan.total_bytes += size;
    }
    plan
}

fn split_name(name: &str) -> (String, String) {
    let p = Path::new(name);
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    (stem, ext)
}

fn rollback_path(root: &Path, ts: i64) -> PathBuf {
    root.join(format!("归档记录-{}.txt", ts))
}

/// 把 原→新 路径映射写进撤销记录（根目录下 归档记录-<时间戳>.txt）。
fn write_rollback(root: &Path, moves: &[(String, String)]) -> std::io::Result<()> {
    let path = rollback_path(root, now_unix());
    let mut s = String::new();
    for (f, t) in moves {
        s.push_str(&format!("{f}\t{t}\n"));
    }
    std::fs::write(path, s)
}

/// 执行归档计划：逐项移动（只移动、永不删除、同名加序号），把 原→新 写入可撤销记录。
/// 返回成功移动的数量。被占用/目标创建失败的文件跳过，不影响其它项。
#[allow(dead_code)]
pub fn execute_arch_plan(target_root: &Path, plan: &ArchivePlan) -> usize {
    let mut moved = 0usize;
    let mut moves: Vec<(String, String)> = Vec::new();
    for it in &plan.items {
        if !it.src.exists() {
            continue;
        }
        let dir = it.dest.parent().unwrap_or(target_root);
        if std::fs::create_dir_all(dir).is_err() {
            continue;
        }
        let mut dest = it.dest.clone();
        // 同名冲突：追加 (序号)
        let name = it.dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let (stem, ext) = split_name(&name);
        let mut i = 1u32;
        while dest.exists() {
            let suffix = if ext.is_empty() {
                format!("({i})")
            } else {
                format!("({i}).{ext}")
            };
            dest = dir.join(format!("{stem} {suffix}"));
            i += 1;
            if i > 999 {
                break;
            }
        }
        if i > 999 {
            continue;
        }
        if std::fs::rename(&it.src, &dest).is_ok() {
            moved += 1;
            moves.push((it.src.to_string_lossy().to_string(), dest.to_string_lossy().to_string()));
        }
    }
    if !moves.is_empty() {
        let _ = write_rollback(target_root, &moves);
    }
    moved
}

/// 撤销最近一次归档：读最新的 归档记录-*.txt，把 dest → src 反向移回。
/// 返回移回数量；处理成功后删除该记录文件，避免重复撤销。
#[allow(dead_code)]
pub fn undo_last_archive(target_root: &Path) -> usize {
    let mut recs: Vec<(i64, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(target_root) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = n.strip_prefix("归档记录-").and_then(|s| s.strip_suffix(".txt")) {
                if let Ok(ts) = rest.parse::<i64>() {
                    recs.push((ts, e.path()));
                }
            }
        }
    }
    recs.sort_by(|a, b| b.0.cmp(&a.0));
    let (_, rec) = match recs.into_iter().next() {
        Some(x) => x,
        None => return 0,
    };
    let body = match std::fs::read_to_string(&rec) {
        Ok(b) => b,
        Err(_) => return 0,
    };
    let mut restored = 0usize;
    for line in body.lines() {
        let mut it = line.split('\t');
        let (Some(from), Some(to)) = (it.next(), it.next()) else { continue };
        let fp = PathBuf::from(from.trim());
        let tp = PathBuf::from(to.trim());
        if tp.exists() && !fp.exists() {
            if let Some(d) = fp.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if std::fs::rename(&tp, &fp).is_ok() {
                restored += 1;
            }
        }
    }
    let _ = std::fs::remove_file(&rec);
    restored
}

