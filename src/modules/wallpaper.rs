//! 自绘毛玻璃背景：抓当前桌面壁纸 → WIC 解码缩放 → 盒式模糊 → BGRA。
//!
//! 为什么不用系统 Mica/Acrylic：两者都是 DWM 按窗口矩形画的系统层效果，
//! 无法按 UI 的圆角裁剪，四角会露出矩形直角。这里改为自己抓壁纸、模糊，
//! 再把模糊结果作为「圆角形状」画进 DirectComposition surface，形状完美贴合。
//! 壁纸基本静态，位图按尺寸缓存，只在尺寸变化/手动刷新时重算。

use std::ffi::c_void;

use windows::core::Interface;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICBitmapSource, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETDESKWALLPAPER};

/// 模糊后的一帧 BGRA 像素（预乘无关，上传 D2D 时用 premultiplied）。
pub struct BlurredWallpaper {
    pub pixels: Vec<u8>,
    pub w: u32,
    pub h: u32,
}

/// 抓当前壁纸路径。多显示器/幻灯片时拿到的是系统当前实际壁纸文件。
fn current_wallpaper_path() -> Option<String> {
    let mut buf = [0u16; 2048];
    unsafe {
        // pvparam 指向缓冲区，uiparam 是缓冲区字符数
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut c_void),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 用 WIC 把图片解码、按「填充」方式缩放到 w×h，输出 BGRA(32bpp) 像素。
fn decode_scaled(path: &str, w: u32, h: u32) -> Result<Vec<u8>, windows::core::Error> {
    unsafe {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_ALL)?;
        let decoder = factory.CreateDecoderFromFilename(
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            GENERIC_READ,
            WICDecodeMetadataCacheOnLoad,
        )?;
        let frame = decoder.GetFrame(0)?;

        // 计算等比填充裁剪区，避免壁纸被拉伸变形（模糊后差异很小，但保持正确）
        let (src_w, src_h) = {
            let mut sw = 0u32;
            let mut sh = 0u32;
            frame.GetSize(&mut sw, &mut sh)?;
            (sw, sh)
        };
        let scale = (w as f32 / src_w as f32).max(h as f32 / src_h as f32);
        let crop_w = ((w as f32 / scale) as u32).min(src_w).max(1);
        let crop_h = ((h as f32 / scale) as u32).min(src_h).max(1);
        let crop_x = (src_w.saturating_sub(crop_w)) / 2;
        let crop_y = (src_h.saturating_sub(crop_h)) / 2;

        // 先裁剪到目标宽高比
        let cropped = {
            let bm = factory.CreateBitmapFromSourceRect(
                &frame,
                crop_x,
                crop_y,
                crop_w,
                crop_h,
            )?;
            bm
        };
        // 再缩放到目标尺寸
        let scaled = factory.CreateBitmapScaler()?;
        scaled.Initialize(
            &cropped,
            w,
            h,
            WICBitmapInterpolationModeFant,
        )?;

        // 统一转成 32bppBGRA（壁纸 jpg 通常是 24bppBGR）
        let conv = factory.CreateFormatConverter()?;
        conv.Initialize(
            &scaled,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let src: IWICBitmapSource = conv.cast()?;

        let stride = w * 4;
        let mut pixels = vec![255u8; (stride * h) as usize];
        src.CopyPixels(std::ptr::null(), stride, &mut pixels)?;
        Ok(pixels)
    }
}

/// 盒式模糊：水平 + 垂直两遍，迭代 3 次近似高斯。纯 Rust，无第三方依赖。
/// 边界用复制填充（clamp），滑动窗口 O(n) 每遍。
fn box_blur_rgba(pixels: &mut [u8], w: usize, h: usize, radius: usize) {
    if radius == 0 || w == 0 || h == 0 || pixels.len() < w * h * 4 {
        return;
    }
    let mut tmp = vec![0u8; pixels.len()];
    let stride = w * 4;

    for _ in 0..3 {
        // 水平：对每个 (y, c)，滑动窗口 x∈[x-r, x+r]
        for y in 0..h {
            let row = y * stride;
            for c in 0..4 {
                // 初始窗口 x=0：索引 [0..=radius] 全部 clamp 到 0
                let mut sum: u32 = (radius + 1) as u32 * pixels[row + c] as u32;
                for x in 0..w {
                    let add = ((x + radius) as i64).min((w - 1) as i64) as usize;
                    let remove = x as i64 - radius as i64 - 1;
                    sum += pixels[row + add * 4 + c] as u32;
                    if remove >= 0 {
                        sum -= pixels[row + remove as usize * 4 + c] as u32;
                    }
                    let lo = (x as i64 - radius as i64).max(0) as usize;
                    let count = (add - lo + 1) as u32;
                    tmp[row + x * 4 + c] = (sum / count) as u8;
                }
            }
        }
        // 垂直：对每个 (x, c)，滑动窗口 y∈[y-r, y+r]
        for x in 0..w {
            for c in 0..4 {
                let mut sum: u32 = (radius + 1) as u32 * tmp[x * 4 + c] as u32;
                for y in 0..h {
                    let add = ((y + radius) as i64).min((h - 1) as i64) as usize;
                    let remove = y as i64 - radius as i64 - 1;
                    sum += tmp[add * stride + x * 4 + c] as u32;
                    if remove >= 0 {
                        sum -= tmp[remove as usize * stride + x * 4 + c] as u32;
                    }
                    let lo = (y as i64 - radius as i64).max(0) as usize;
                    let count = (add - lo + 1) as u32;
                    pixels[y * stride + x * 4 + c] = (sum / count) as u8;
                }
            }
        }
    }
}

/// 抓取壁纸并生成模糊背景。失败返回 Err（调用方可以退回纯色半透明底）。
pub fn capture_blurred(w: u32, h: u32, radius: usize) -> Result<BlurredWallpaper, Box<dyn std::error::Error>> {
    let path = current_wallpaper_path().ok_or("no wallpaper path")?;
    let mut pixels = decode_scaled(&path, w, h)?;
    box_blur_rgba(&mut pixels, w as usize, h as usize, radius);
    Ok(BlurredWallpaper { pixels, w, h })
}
