//! 系统托盘图标（屏幕右下角）
//! 左键：显示/隐藏 + 展开切换；右键：菜单（显示/隐藏、开机自启、退出）
//! 图标为运行时绘制的深色胶囊 + 青色圆点，与主 UI 风格一致。

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, GetCursorPos, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, HICON, ICONINFO, MF_SEPARATOR, MF_STRING,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};

pub const TRAY_MSG: u32 = WM_APP + 1;

const ID_SHOW: i32 = 1;
const ID_SETTINGS: i32 = 2;
const ID_QUIT: i32 = 3;

pub fn add(hwnd: HWND) {
    unsafe {
        let icon = make_icon();
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = TRAY_MSG;
        nid.hIcon = icon;
        set_str(&mut nid.szTip, "WinIsland 灵动岛");
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            eprintln!("[winisland] 托盘图标添加失败");
        }
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
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

/// 运行时绘制 32x32 图标：深色胶囊 + 青色圆点（BGRA 预乘）
unsafe fn make_icon() -> HICON {
    const SZ: i32 = 32;
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: SZ,
        biHeight: -SZ, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut pbits: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut hbm_color: Option<HBITMAP> = None;
    if let Ok(hbm) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut pbits, None, 0) {
        if !pbits.is_null() {
            let buf = std::slice::from_raw_parts_mut(pbits as *mut u32, (SZ * SZ) as usize);
            for y in 0..SZ as usize {
                for x in 0..SZ as usize {
                    let (xf, yf) = (x as f32, y as f32);
                    // 胶囊主体：y ∈ [9, 23)，两端半圆半径 7
                    let in_capsule = {
                        let dx = if xf < 7.0 { 7.0 - xf } else if xf > 24.0 { xf - 24.0 } else { 0.0 };
                        let dy = (yf - 16.0).abs();
                        dx * dx + dy * dy <= 49.0
                    };
                    // 青色圆点：圆心 (9, 16) 半径 3.5
                    let ddx = xf - 9.0;
                    let ddy = yf - 16.0;
                    let in_dot = ddx * ddx + ddy * ddy <= 12.25;
                    let (b, g, r, a) = if in_dot {
                        (255u32, 199u32, 64u32, 255u32)
                    } else if in_capsule {
                        (30, 32, 35, 245)
                    } else {
                        (0, 0, 0, 0)
                    };
                    buf[y * SZ as usize + x] = b | (g << 8) | (r << 16) | (a << 24);
                }
            }
            hbm_color = Some(hbm);
        }
    }
    let Some(hbm_color) = hbm_color else { return HICON::default() };
    let hbm_mask = CreateBitmap(SZ, SZ, 1, 1, None);
    let ii = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let icon = CreateIconIndirect(&ii).unwrap_or_default();
    let _ = DeleteObject(HGDIOBJ(hbm_color.0));
    let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
    icon
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
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
        ID_QUIT => crate::app::quit(),
        _ => {}
    }
}
