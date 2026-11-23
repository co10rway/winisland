//! 真毛玻璃背板（Mica / Acrylic）：DirectComposition 呈现路径。
//!
//! 思路：毛玻璃模式下，主窗口不再用 `WS_EX_LAYERED` 分层自绘，
//! 而是 (1) 用 `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE)` 让 DWM
//! 画真正的系统背板（Mica=MINWINDOW / Acrylic=TRANSIENTWINDOW），
//! (2) 用 DirectComposition 把一个 D2D surface 叠加到窗口上绘制内容。
//! 这样半透明内容 + 圆角都能保留 alpha，DWM 背板从透明区域透出来。
//!
//! 本模块只在 Win11 22H2+ 上有效；调用失败时上层会回退到分层自绘。

use std::ffi::c_void;

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_9_3,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice2, IDCompositionDesktopDevice, IDCompositionSurface, IDCompositionTarget,
    IDCompositionVisual2,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWM_SYSTEMBACKDROP_TYPE,
    DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_NONE, DWMWCP_DONOTROUND,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

/// 背板类型。解析自 config 的 `backdrop` 字段。
/// 背板类型（两态）。v0.17.2 起 Mica/Acrylic 合并为单一「毛玻璃」：
/// 两者在悬浮小部件上肉眼无差别，且旧枚举名对用户无意义。
/// 旧配置里的 "mica"/"acrylic" 由 config::load 归一到 "glass"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackdropKind {
    /// 分层自绘（无毛玻璃）
    None,
    /// 毛玻璃（自绘模糊壁纸，Acrylic 配方：通透磨砂）
    Glass,
}

impl BackdropKind {
    #[allow(dead_code)]
    pub fn parse(s: &str) -> BackdropKind {
        match s {
            "glass" | "mica" | "acrylic" => BackdropKind::Glass,
            _ => BackdropKind::None,
        }
    }

    #[allow(dead_code)]
    pub fn id(&self) -> &'static str {
        match self {
            BackdropKind::None => "none",
            BackdropKind::Glass => "glass",
        }
    }

    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            BackdropKind::None => "无",
            BackdropKind::Glass => "毛玻璃",
        }
    }

    /// 是否走 DirectComposition 毛玻璃路径
    #[allow(dead_code)]
    pub fn glassy(&self) -> bool {
        !matches!(self, BackdropKind::None)
    }
}

/// DirectComposition 全链路：D3D11 设备 → DComp 设备 → target → visual → surface。
/// surface 需要随窗口尺寸变化重建（本结构持有唯一 surface）。
pub struct Glassy {
    device: IDCompositionDesktopDevice,
    _visual: IDCompositionVisual2,
    surface: Option<IDCompositionSurface>,
    pub sw: u32,
    pub sh: u32,
    /// surface 每次重建 +1。Canvas 据此判断绑定的 bitmap 是否需要重建。
    pub generation: u64,
}

impl Glassy {
    /// 初始化毛玻璃链路。失败返回 Err（调用方静默回退到分层自绘）。
    pub fn enable(hwnd: HWND, w: i32, h: i32) -> Result<Glassy, windows::core::Error> {
        unsafe {
            // 1) D3D11 设备（BGRA 以支持 D2D surface 互操作）
            let levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
                D3D_FEATURE_LEVEL_9_3,
            ];
            let mut dev: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let null_mod: windows::Win32::Foundation::HMODULE = std::mem::zeroed();
            // 优先硬件，失败退回 WARP（远程桌面 / 无独显环境）
            let mut drv = D3D_DRIVER_TYPE_HARDWARE;
            let mut r = D3D11CreateDevice(
                None,
                drv,
                null_mod,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            );
            if r.is_err() {
                drv = D3D_DRIVER_TYPE_WARP;
                dev = None;
                ctx = None;
                r = D3D11CreateDevice(
                    None,
                    drv,
                    null_mod,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                );
            }
    r?;
    let dev = dev.ok_or_else(|| windows::core::Error::empty())?;
    let dxgi: IDXGIDevice = dev.cast()?;

            // 2) 关闭 DWM 系统背板。系统背板只对「激活的顶层窗口」显示壁纸采样；
            // 灵动岛是 NOACTIVATE 悬浮窗，DWM 只会画一块中性灰 fallback。这块灰
            // 会从「窗口矩形」与「UI 内容圆角」之间的缝隙露出，在四角形成灰边。
            // 真正毛玻璃由 wallpaper.rs 自绘模糊壁纸提供，不靠系统背板。
            let sb: DWM_SYSTEMBACKDROP_TYPE = DWMSBT_NONE;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &sb as *const DWM_SYSTEMBACKDROP_TYPE as *const c_void,
                std::mem::size_of::<DWM_SYSTEMBACKDROP_TYPE>() as u32,
            )?;

            // 2b) 去掉系统浅色边框（白框根因）：一旦去掉 WS_EX_LAYERED，
            // DWM 会把窗口当普通顶层窗口画标准边框。必须显式声明深色模式、
            // 无边框、圆角，否则四边出现白色/浅灰细框，且背板被浅色边框割裂。
            let dark: u32 = 1; // TRUE
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );
            let no_border: u32 = DWMWA_COLOR_NONE;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &no_border as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            );
            // 不让 DWM 再做一层圆角裁剪：它会裁成约 8px，而 UI 内容圆角是 24px，
            // 两者缝隙正是灰色背板漏出的地方。改由 UI 自绘圆角决定外形，
            // 圆角之外保持透明、直接透出桌面壁纸。
            let corner: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );

            // 毛玻璃背景改为自绘（wallpaper.rs）：抓壁纸→模糊→按圆角画进 surface。
            // 系统 backdrop / accent 都按窗口矩形绘制、无法贴合 UI 圆角，故不再使用。

            // 3) DirectComposition：device → target → visual → surface
            let device: IDCompositionDesktopDevice = DCompositionCreateDevice2(&dxgi)?;
            // topmost=true：让 DComp visual 成为窗口最上层内容，
            // 系统背板（Mica/Acrylic）才能从 surface 的透明/半透明区透出。
            let target: IDCompositionTarget = device.CreateTargetForHwnd(hwnd, true)?;
            let visual: IDCompositionVisual2 = device.CreateVisual()?;
            target.SetRoot(&visual)?;
            let surface: IDCompositionSurface =
                device.CreateSurface(w as u32, h as u32, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            visual.SetContent(&surface)?;
            device.Commit()?;

            Ok(Glassy {
                device,
                _visual: visual,
                surface: Some(surface),
                sw: w as u32,
                sh: h as u32,
                generation: 1,
            })
        }
    }

    pub fn device(&self) -> &IDCompositionDesktopDevice {
        &self.device
    }

    pub fn surface(&self) -> &IDCompositionSurface {
        self.surface.as_ref().expect("glassy surface")
    }

    /// 尺寸变化时重建 surface（DComp surface 不支持原地 resize）。
    pub fn ensure_surface(&mut self, w: i32, h: i32) -> Result<(), windows::core::Error> {
        if self.sw == w as u32 && self.sh == h as u32 && self.surface.is_some() {
            return Ok(());
        }
        unsafe {
            let surface: IDCompositionSurface =
                self.device.CreateSurface(w as u32, h as u32, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED)?;
            self._visual.SetContent(&surface)?;
            self.device.Commit()?;
            self.surface = Some(surface);
            self.sw = w as u32;
            self.sh = h as u32;
            self.generation = self.generation.wrapping_add(1);
        }
        Ok(())
    }
}
