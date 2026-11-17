//! 遮挡检测：面板区域是否被其他应用窗口压住
//!
//! 决定灵动岛要不要自动收成顶部横条。只读取窗口矩形、可见性、窗口类名，
//! **不截屏、不读取窗口标题/内容、不发任何网络请求**。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 重叠小于该像素就当作「贴边」，不算遮挡，避免窗口边缘抖动导致来回收起。
/// 调大后：窗口要真正压上来才收起，隔着一点距离不触发。
const MIN_OVERLAP: i32 = 16;

/// 桌面外壳窗口：即使铺满全屏也不算遮挡
const SHELL_CLASSES: [&str; 10] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "ForegroundStaging",
    "MultitaskingViewFrame",
    "TaskListThumbnailWnd",
    "XamlExplorerHostIslandWindow",
    "Shell_InputSwitchTopLevelWindow",
];

struct Scan {
    area: RECT,
    self_pid: u32,
    blocker: Option<HWND>,
}

/// 返回第一个压住该矩形的窗口（None = 没有遮挡）
pub fn blocker(x: i32, y: i32, w: i32, h: i32) -> Option<HWND> {
    let mut s = Scan {
        area: RECT { left: x, top: y, right: x + w, bottom: y + h },
        self_pid: std::process::id(),
        blocker: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut s as *mut Scan as isize));
    }
    s.blocker
}

/// 面板区域是否被遮挡
pub fn covered(x: i32, y: i32, w: i32, h: i32) -> bool {
    blocker(x, y, w, h).is_some()
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lp: LPARAM) -> BOOL {
    let s = &mut *(lp.0 as *mut Scan);
    if window_blocks(hwnd, &s.area, s.self_pid) {
        s.blocker = Some(hwnd);
        return BOOL(0); // 找到一个就够了，立刻停止枚举
    }
    BOOL(1)
}

unsafe fn window_blocks(hwnd: HWND, area: &RECT, self_pid: u32) -> bool {
    // 自己（含设置窗口）永远不算遮挡
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == self_pid {
        return false;
    }
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return false;
    }

    // 工具面板 / 不激活浮层 / 点击穿透层：不抢注意力，也不算遮挡
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let skip = WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TRANSPARENT.0;
    if ex & skip != 0 {
        return false;
    }

    // 被 DWM 隐藏的窗口（其它虚拟桌面、UWP 挂起窗口、开始菜单）不可见
    let mut cloaked = 0u32;
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok()
        && cloaked != 0
    {
        return false;
    }

    let cls = class_of(hwnd);
    if SHELL_CLASSES.iter().any(|c| *c == cls) {
        return false;
    }

    // 分层透明窗（alpha=0 的隐形窗口）：看不见就不算遮挡，
    // 避免某些应用的后台悬浮窗误判
    if ex & WS_EX_LAYERED.0 != 0 {
        let mut key = windows::Win32::Foundation::COLORREF(0);
        let mut alpha = 0u8;
        let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
        if GetLayeredWindowAttributes(hwnd, Some(&mut key), Some(&mut alpha), Some(&mut flags)).is_ok()
            && flags.0 & LWA_ALPHA.0 != 0
            && alpha == 0
        {
            return false;
        }
    }

    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_err() {
        return false;
    }
    let ox = r.right.min(area.right) - r.left.max(area.left);
    let oy = r.bottom.min(area.bottom) - r.top.max(area.top);
    ox >= MIN_OVERLAP && oy >= MIN_OVERLAP
}

/// 窗口类名（调试用：只取类名，不读标题）
pub fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n > 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    }
}

/// 调试用：阻塞窗口的类名与矩形
pub fn describe(hwnd: HWND) -> String {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut r);
    }
    format!(
        "{} @ ({},{})-({},{})",
        class_of(hwnd),
        r.left,
        r.top,
        r.right,
        r.bottom
    )
}
