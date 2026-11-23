//! 系统托盘图标（屏幕右下角）
//! 左键：显示/隐藏 + 展开切换；右键：菜单（模式切换、显示/隐藏、设置、退出）
//! 图标取自嵌入 exe 的应用图标资源（assets/icon.ico → RT_ICON ID=1），
//! 与资源管理器 / 任务栏 / ALT-TAB 显示的是同一枚，不再运行时手绘。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, HICON, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};

pub const TRAY_MSG: u32 = WM_APP + 1;

const ID_SHOW: i32 = 1;
const ID_SETTINGS: i32 = 2;
const ID_QUIT: i32 = 3;
const ID_MODE_WORK: i32 = 4;
const ID_MODE_STOCK: i32 = 5;

/// 应用图标（嵌入资源 ICON ID=1）。托盘与各窗口类共用，保证全局一致。
pub fn app_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        // MAKEINTRESOURCEW(1)：ID 1 即 build.rs 写入的 ICON 资源
        LoadIconW(Some(hinst.into()), PCWSTR(1usize as *const u16)).unwrap_or_default()
    }
}

pub fn add(hwnd: HWND) {
    unsafe {
        let icon = app_icon();
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: TRAY_MSG,
            hIcon: icon,
            ..Default::default()
        };
        set_str(&mut nid.szTip, "WinIsland 灵动岛");
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            eprintln!("[winisland] 托盘图标添加失败");
        }
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

pub fn on_msg(hwnd: HWND, lparam: LPARAM) {
    let msg = (lparam.0 & 0xFFFF) as u32;
    match msg {
        WM_LBUTTONUP => crate::app::tray_click(),
        WM_RBUTTONUP => unsafe { show_menu(hwnd) },
        _ => {}
    }
}

fn set_str(dst: &mut [u16; 128], s: &str) {
    let wide: Vec<u16> = s.encode_utf16().take(127).collect();
    dst[..wide.len()].copy_from_slice(&wide);
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    // 模式二选一：当前模式打勾，便于一眼看出胶囊现在处于哪种模式
    let stock_mode = crate::config::Config::load().stock_mode();
    let mark = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, mark(!stock_mode), ID_MODE_WORK as usize, w!("工作模式"));
    let _ = AppendMenuW(menu, mark(stock_mode), ID_MODE_STOCK as usize, w!("盯盘模式"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, ID_SHOW as usize, w!("显示 / 隐藏 灵动岛"));
    let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS as usize, w!("设置…"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT as usize, w!("退出 WinIsland"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 {
        ID_SHOW => crate::app::toggle_hidden(),
        ID_SETTINGS => crate::settings::open(),
        ID_MODE_WORK => crate::app::set_mode("work"),
        ID_MODE_STOCK => crate::app::set_mode("stock"),
        ID_QUIT => crate::app::quit(),
        _ => {}
    }
}
