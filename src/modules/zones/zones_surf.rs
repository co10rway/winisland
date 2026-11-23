//! 桌面分区：离屏 32bpp 位图表面与像素级绘图原语。
//!
//! 从一个纯自包含、无共享静态状态的子系统拆出；父模块通过
//! `use zones_surf::{Surf, put_px, blend_px, corner_cover}` 复用。
//! 32bpp 预乘 ARGB 位图再提交，物理上不可能有残留；窗口移动时 DWM 搬运的
//! 是完整图层而不是屏幕像素，所以拖动不会重影。

use core::ffi::c_void;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HBITMAP, HGDIOBJ,
};

/// 一张离屏 32bpp（预乘 BGRA、top-down）位图及其 DC
pub struct Surf {
    pub dib: HBITMAP,
    pub mdc: HDC,
    pub old: HGDIOBJ,
    pub buf: *mut u32,
    pub w: i32,
    pub h: i32,
}

impl Surf {
    pub fn new(w: i32, h: i32) -> Option<Surf> {
        unsafe {
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // 负高 = top-down，与 GDI 坐标一致
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            if dib.0.is_null() || bits.is_null() {
                return None;
            }
            let mdc = CreateCompatibleDC(None);
            if mdc.0.is_null() {
                let _ = DeleteObject(dib.into());
                return None;
            }
            let old = SelectObject(mdc, dib.into());
            Some(Surf {
                dib,
                mdc,
                old,
                buf: bits as *mut u32,
                w,
                h,
            })
        }
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.buf, (self.w * self.h) as usize) }
    }
}

impl Drop for Surf {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mdc, self.old);
            let _ = DeleteDC(self.mdc);
            let _ = DeleteObject(self.dib.into());
        }
    }
}

/// 写一个预乘 ARGB 像素（r,g,b 未预乘）
#[inline]
pub fn put_px(buf: &mut [u32], i: usize, c: (u32, u32, u32), a: u32) {
    buf[i] = (a << 24)
        | ((c.0 * a / 255) << 16)
        | ((c.1 * a / 255) << 8)
        | (c.2 * a / 255);
}

/// 把一个未预乘颜色按 alpha 叠加到已有预乘像素上（用于滚动条等画在面板之上的装饰）
#[inline]
pub fn blend_px(buf: &mut [u32], i: usize, c: (u32, u32, u32), a: u32) {
    let cur = buf[i];
    let da = (cur >> 24) & 0xff;
    let na = a + da * (255 - a) / 255;
    let mix = |src: u32, dst: u32| -> u32 { (src * a / 255 + dst * (255 - a) / 255).min(255) };
    let r = mix(c.0, (cur >> 16) & 0xff);
    let g = mix(c.1, (cur >> 8) & 0xff);
    let b = mix(c.2, cur & 0xff);
    buf[i] = (na << 24) | (r << 16) | (g << 8) | b;
}

/// 圆角覆盖率（0-4）。返回 4 表示完全在圆角内。只对四角像素调用。
pub fn corner_cover(x: i32, y: i32, w: i32, h: i32, r: i32) -> u32 {
    let (cx, cy) = if x < r && y < r {
        (r as f32, r as f32)
    } else if x >= w - r && y < r {
        ((w - r) as f32, r as f32)
    } else if x < r && y >= h - r {
        (r as f32, (h - r) as f32)
    } else if x >= w - r && y >= h - r {
        ((w - r) as f32, (h - r) as f32)
    } else {
        return 4;
    };
    let rf = r as f32;
    let mut hit = 0u32;
    for sy in 0..2 {
        for sx in 0..2 {
            let px = x as f32 + 0.25 + sx as f32 * 0.5;
            let py = y as f32 + 0.25 + sy as f32 * 0.5;
            let (dx, dy) = (px - cx, py - cy);
            if dx * dx + dy * dy <= rf * rf {
                hit += 1;
            }
        }
    }
    hit
}
