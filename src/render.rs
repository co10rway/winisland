//! 渲染引擎：Direct2D(DCRT) + DirectWrite，绘制到分层窗口（UpdateLayeredWindow）
//! 全部为系统组件，无第三方运行时依赖。

use std::collections::HashMap;
use windows::Win32::Foundation::{RECT, HWND, POINT, SIZE};
use windows::Win32::Graphics::Direct2D::Common as D2DC;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common as DxgiC;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};
use windows::core::{w, Interface, PCWSTR};

/// 全局 UI 缩放（DPI/96）
pub static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
pub fn s(v: f32) -> f32 {
    v * SCALE.get().copied().unwrap_or(1.0)
}
pub fn si(v: f32) -> i32 {
    (v * SCALE.get().copied().unwrap_or(1.0)) as i32
}

#[inline]
fn col(hex: u32, alpha: f32) -> D2DC::D2D1_COLOR_F {
    D2DC::D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: alpha,
    }
}

type FmtKey = (u32, u32, i32, i32, bool); // weight, size*100, halign, valign, nowrap

pub struct Canvas {
    pub dcrt: ID2D1DCRenderTarget,
    /// 当前绘制目标：默认是 dcrt 的基接口；毛玻璃路径下每帧切换为
    /// BeginDraw 取到的 ID2D1DeviceContext。所有图元通过它绘制。
    rt: ID2D1RenderTarget,
    /// dcrt 的基接口缓存（normal 路径每帧复位 `rt` 用，避免重复 QueryInterface）
    dcrt_base: ID2D1RenderTarget,
    dwrite: IDWriteFactory,
    fmts: HashMap<FmtKey, IDWriteTextFormat>,
    /// 复用的纯色画刷：每帧上百个图元共用同一个 COM 对象，只改颜色，
    /// 避免每帧创建/销毁几十个画刷（高刷下这是主要开销之一）
    brush: std::cell::RefCell<Option<ID2D1SolidColorBrush>>,
    memdc: HDC,
    hbmp: HBITMAP,
    /// 位图容量（分配过的最大尺寸）
    cap_w: i32,
    cap_h: i32,
    /// 当前绑定到渲染目标的尺寸
    w: i32,
    h: i32,
    /// 当前是否走 DirectComposition 毛玻璃路径。
    /// UI 层据此把最外层背景/小黑条底色改为半透明，让 DWM 系统背板透出来。
    pub glassy: bool,
    /// 毛玻璃下主底色相对 alpha（背板透出强度）。normal 路径恒为 1.0。
    pub glass_bg_alpha: f32,
    /// 自绘毛玻璃：缓存的模糊壁纸 BGRA 像素（按最大窗口尺寸生成一次）。
    blur_pixels: Vec<u8>,
    blur_pw: u32,
    blur_ph: u32,
    /// 绑定到 device context 的位图/画刷，surface 重建（generation 变化）后作废。
    blur_bmp: Option<ID2D1Bitmap>,
    blur_brush: Option<ID2D1BitmapBrush>,
    blur_bmp_gen: u64,
    /// 是否成功抓到壁纸（false 时 UI 退回带色调的兜底底）
    pub has_blur: bool,
}

impl Canvas {
    pub fn new() -> Result<Canvas, Box<dyn std::error::Error>> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2DC::D2D1_PIXEL_FORMAT {
                    format: DxgiC::DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2DC::D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let dcrt = factory.CreateDCRenderTarget(&props)?;
            let dcrt_base: ID2D1RenderTarget = dcrt.cast()
                .map_err(|e| format!("dcrt->render target cast: {e}"))?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let screen = GetWindowDC(None);
            let memdc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            Ok(Canvas {
                dcrt,
                rt: dcrt_base.clone(),
                dcrt_base,
                dwrite,
                fmts: HashMap::new(),
                brush: std::cell::RefCell::new(None),
                memdc,
                hbmp: HBITMAP::default(),
                cap_w: 0,
                cap_h: 0,
                w: 0,
                h: 0,
                glassy: false,
                glass_bg_alpha: 1.0,
                blur_pixels: Vec::new(),
                blur_pw: 0,
                blur_ph: 0,
                blur_bmp: None,
                blur_brush: None,
                blur_bmp_gen: 0,
                has_blur: false,
            })
        }
    }

    /// 取一个已设置好颜色的纯色画刷（复用同一对象）。返回 clone 只是 AddRef，
    /// 比 CreateSolidColorBrush 便宜一个数量级。
    fn solid(&self, c: u32, alpha: f32) -> Option<ID2D1SolidColorBrush> {
        let color = col(c, alpha);
        let mut slot = self.brush.borrow_mut();
        if slot.is_none() {
            match unsafe { self.rt.CreateSolidColorBrush(&color, None) } {
                Ok(b) => *slot = Some(b),
                Err(_) => return None,
            }
        }
        let b = slot.as_ref()?;
        if color.a >= 0.0 {
            unsafe {
                let _ = b.SetColor(&color);
            }
        }
        Some(b.clone())
    }

    /// 只在尺寸变化时重建位图；尺寸只增不减（展开动画里每帧都会变高，
    /// 若每帧重建 DIB section 会产生大量 GDI 分配/释放）
    fn ensure_size(&mut self, w: i32, h: i32) {
        unsafe {
            if self.hbmp.is_invalid() || w > self.cap_w || h > self.cap_h {
                // 按 1.25 倍增长，减少重建次数
                let cw = ((w as f32 * 1.25) as i32 + 63) & !63;
                let ch = ((h as f32 * 1.25) as i32 + 63) & !63;
                let bi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: cw,
                        biHeight: -ch,
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
                let hbmp = match CreateDIBSection(Some(self.memdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(b) => b,
                    Err(_) => return,
                };
                if !self.hbmp.is_invalid() {
                    let _ = DeleteObject(self.hbmp.into());
                }
                let _old = SelectObject(self.memdc, hbmp.into());
                self.hbmp = hbmp;
                self.cap_w = cw;
                self.cap_h = ch;
                self.w = 0; // 强制重新 BindDC
                self.h = 0;
            }
            if self.w != w || self.h != h {
                self.dcrt
                    .BindDC(self.memdc, &RECT { left: 0, top: 0, right: w, bottom: h })
                    .expect("BindDC");
                self.w = w;
                self.h = h;
            }
        }
    }

    /// 渲染一帧并提交到分层窗口
    pub fn present(&mut self, hwnd: HWND, w: i32, h: i32, draw: impl FnOnce(&mut Canvas, f32, f32)) {
        self.ensure_size(w, h);
        self.glassy = false; // normal 路径：底色不透明，走 ULW
        self.glass_bg_alpha = 1.0;
        self.has_blur = false;
        self.rt = self.dcrt_base.clone(); // 复位绘制目标为 DCRT
        // 画刷绑定创建它的 render target：从 glassy 回退到 DCRT 后必须作废旧画刷
        *self.brush.borrow_mut() = None;
        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&D2DC::D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            draw(self, w as f32, h as f32);
            if self.rt.EndDraw(None, None).is_err() {
                // 渲染失败：静默跳过本帧（v0.1 策略）
            }
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let size = SIZE { cx: w, cy: h };
            let pt0 = POINT { x: 0, y: 0 };
            let _ = UpdateLayeredWindow(
                hwnd,
                None,
                None,
                Some(&size),
                Some(self.memdc),
                Some(&pt0),
                windows::Win32::Foundation::COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    /// 准备自绘毛玻璃背景：抓壁纸→模糊→上传成 D2D 位图画刷。
    /// 像素按窗口最大尺寸缓存一次，surface 重建（generation 变化）才重建位图。
    fn ensure_blur(&mut self, g: &crate::modules::glassy::Glassy, w: u32, h: u32) {
        // 半径 12：保留可见的壁纸纹理，呈现通透磨砂感（原 Acrylic 配方）。
        let radius: usize = 12;
        // 1) 像素缓存：尺寸不够时重新抓取（放大 1.2 倍减少重抓次数）
        if self.blur_pixels.is_empty() || w > self.blur_pw || h > self.blur_ph {
            let cw = (w * 6 / 5).max(w + 16);
            let ch = (h * 6 / 5).max(h + 16);
            match crate::modules::wallpaper::capture_blurred(cw, ch, radius) {
                Ok(b) => {
                    self.blur_pixels = b.pixels;
                    self.blur_pw = b.w;
                    self.blur_ph = b.h;
                    self.has_blur = true;
                    self.blur_bmp_gen = 0; // 强制重建位图
                }
                Err(_) => {
                    self.has_blur = false;
                    return;
                }
            }
        }
        // 2) 位图绑定 device context：generation 变了或还没建就重建
        if self.blur_bmp_gen == g.generation && self.blur_bmp.is_some() {
            return;
        }
        unsafe {
            let props = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2DC::D2D1_PIXEL_FORMAT {
                    format: DxgiC::DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2DC::D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            let bmp = self.rt.CreateBitmap(
                D2DC::D2D_SIZE_U { width: self.blur_pw, height: self.blur_ph },
                Some(self.blur_pixels.as_ptr() as *const std::ffi::c_void),
                self.blur_pw as u32 * 4,
                &props,
            );
            match bmp {
                Ok(bmp) => {
                    let bp = D2D1_BITMAP_BRUSH_PROPERTIES {
                        extendModeX: D2D1_EXTEND_MODE_CLAMP,
                        extendModeY: D2D1_EXTEND_MODE_CLAMP,
                        interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    };
                    match self.rt.CreateBitmapBrush(&bmp, Some(&bp), None) {
                        Ok(brush) => {
                            self.blur_brush = Some(brush);
                            self.blur_bmp = Some(bmp);
                            self.blur_bmp_gen = g.generation;
                        }
                        Err(_) => self.has_blur = false,
                    }
                }
                Err(_) => self.has_blur = false,
            }
        }
    }

    /// 用模糊壁纸以圆角形状填充背景（四角完美贴合，不留矩形直角）。
    /// alpha 用于把壁纸压暗到合适的玻璃亮度。
    pub fn rrect_blur(&self, x: f32, y: f32, w: f32, h: f32, r: f32, alpha: f32) {
        let Some(brush) = &self.blur_brush else { return };
        unsafe {
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            // 用画刷透明度控制壁纸亮度
            let _ = brush.SetOpacity(alpha);
            self.rt.FillRoundedRectangle(&rr, brush);
            let _ = brush.SetOpacity(1.0);
        }
    }

    /// 毛玻璃路径：把当前帧绘制到 DirectComposition surface（GPU，alpha 保留）。
    /// 背景用自绘的模糊壁纸（圆角），不再依赖系统矩形背板。
    pub fn present_glassy(
        &mut self,
        g: &mut crate::modules::glassy::Glassy,
        w: i32,
        h: i32,
        draw: impl FnOnce(&mut Canvas, f32, f32),
    ) -> Result<(), Box<dyn std::error::Error>> {
        g.ensure_surface(w, h)?;
        let rect = RECT { left: 0, top: 0, right: w, bottom: h };
        let mut offset = POINT { x: 0, y: 0 };
        let surface = g.surface();
        let dc: ID2D1DeviceContext = unsafe { surface.BeginDraw(Some(&rect), &mut offset)? };
        self.rt = dc.cast()?;
        self.glassy = true;
        self.glass_bg_alpha = 0.62;
        *self.brush.borrow_mut() = None; // 新设备上下文：画刷缓存作废
        self.ensure_blur(g, w as u32, h as u32);
        unsafe {
            dc.BeginDraw();
            dc.Clear(Some(&D2DC::D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            draw(self, w as f32, h as f32);
            dc.EndDraw(None, None)?;
            surface.EndDraw()?;
            g.device().Commit()?;
        }
        Ok(())
    }

    fn fmt(&mut self, size: f32, weight: DWRITE_FONT_WEIGHT, halign: i32, valign: i32, nowrap: bool) -> IDWriteTextFormat {
        let key: FmtKey = (weight.0 as u32, (size * 100.0) as u32, halign, valign, nowrap);
        if !self.fmts.contains_key(&key) {
            unsafe {
                let f = self
                    .dwrite
                    .CreateTextFormat(
                        w!("Microsoft YaHei UI"),
                        None,
                        weight,
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        size,
                        w!(""),
                    )
                    .expect("CreateTextFormat");
                let _ = f.SetTextAlignment(match halign {
                    1 => DWRITE_TEXT_ALIGNMENT_CENTER,
                    2 => DWRITE_TEXT_ALIGNMENT_TRAILING,
                    _ => DWRITE_TEXT_ALIGNMENT_LEADING,
                });
                let _ = f.SetParagraphAlignment(match valign {
                    1 => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                    _ => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
                });
                if nowrap {
                    // 单行控件里禁止折行：宁可裁剪，也不要出现半截第二行
                    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                }
                self.fmts.insert(key, f);
            }
        }
        self.fmts.get(&key).unwrap().clone()
    }

    /// 矩形填充
    #[allow(dead_code)]
    pub fn rect(&self, x: f32, y: f32, w: f32, h: f32, c: u32, alpha: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            self.rt.FillRectangle(
                &D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                &brush,
            );
        }
    }

    /// 圆角矩形填充
    pub fn rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: u32, alpha: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            self.rt.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// 圆角矩形描边
    pub fn rrect_stroke(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: u32, alpha: f32, sw: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            self.rt.DrawRoundedRectangle(&rr, &brush, sw, None);
        }
    }

    /// 竖条（波形）
    #[allow(dead_code)]
    pub fn vbar(&self, cx: f32, cy: f32, w: f32, h: f32, c: u32, alpha: f32) {
        self.rrect(cx - w / 2.0, cy - h / 2.0, w, h, w / 2.0, c, alpha);
    }

    /// 圆点
    pub fn circle(&self, cx: f32, cy: f32, r: f32, c: u32, alpha: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            let e = D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: cx, Y: cy },
                radiusX: r,
                radiusY: r,
            };
            self.rt.FillEllipse(&e, &brush);
        }
    }

    /// 文本（盒子内左对齐、垂直居中）
    pub fn text(&mut self, x: f32, y: f32, maxw: f32, maxh: f32, txt: &str, size: f32, weight: DWRITE_FONT_WEIGHT, c: u32, alpha: f32) {
        self.text_ex(x, y, maxw, maxh, txt, size, weight, c, alpha, 0, 1);
    }

    /// 文本（支持水平/垂直对齐；maxh<=0 时自动用行高）
    pub fn text_ex(&mut self, x: f32, y: f32, maxw: f32, maxh: f32, txt: &str, size: f32, weight: DWRITE_FONT_WEIGHT, c: u32, alpha: f32, halign: i32, valign: i32) {
        self.text_full(x, y, maxw, maxh, txt, size, weight, c, alpha, halign, valign, false);
    }

    /// 单行文本（不折行，超出裁剪）
    pub fn text1(&mut self, x: f32, y: f32, maxw: f32, maxh: f32, txt: &str, size: f32, weight: DWRITE_FONT_WEIGHT, c: u32, alpha: f32) {
        self.text_full(x, y, maxw, maxh, txt, size, weight, c, alpha, 0, 1, true);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_full(&mut self, x: f32, y: f32, maxw: f32, maxh: f32, txt: &str, size: f32, weight: DWRITE_FONT_WEIGHT, c: u32, alpha: f32, halign: i32, valign: i32, nowrap: bool) {
        let wide: Vec<u16> = txt.encode_utf16().collect();
        let fmt = self.fmt(s(size), weight, halign, valign, nowrap);
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            let h = if maxh > 0.0 { maxh } else { s(size) * 1.6 };
            let rect = D2DC::D2D_RECT_F {
                left: x,
                top: y,
                right: x + maxw.max(1.0),
                bottom: y + h,
            };
            self.rt.DrawText(
                &wide,
                &fmt,
                &rect,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// 居中文本快捷方式
    pub fn text_c(&mut self, x: f32, y: f32, w: f32, h: f32, txt: &str, size: f32, weight: DWRITE_FONT_WEIGHT, c: u32, alpha: f32) {
        self.text_ex(x, y, w, h, txt, size, weight, c, alpha, 1, 1);
    }

    /// 圆角矩形竖向/横向渐变填充（c1 起始 → c2 结束）
    pub fn rrect_grad(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c1: u32, c2: u32, horizontal: bool, alpha: f32) {
        unsafe {
            let stops = [
                D2DC::D2D1_GRADIENT_STOP { position: 0.0, color: col(c1, alpha) },
                D2DC::D2D1_GRADIENT_STOP { position: 1.0, color: col(c2, alpha) },
            ];
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: windows_numerics::Vector2 { X: x, Y: y },
                endPoint: windows_numerics::Vector2 {
                    X: if horizontal { x + w } else { x },
                    Y: if horizontal { y } else { y + h },
                },
            };
            let coll = match self
                .rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let brush = match self.rt.CreateLinearGradientBrush(&props, None, &coll) {
                Ok(b) => b,
                Err(_) => return,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            self.rt.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// 圆角矩形三段横向渐变填充（c1 → c2(52%) → c3），极光胶囊专用
    pub fn rrect_grad3(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c1: u32, c2: u32, c3: u32, alpha: f32) {
        unsafe {
            let stops = [
                D2DC::D2D1_GRADIENT_STOP { position: 0.0, color: col(c1, alpha) },
                D2DC::D2D1_GRADIENT_STOP { position: 0.52, color: col(c2, alpha) },
                D2DC::D2D1_GRADIENT_STOP { position: 1.0, color: col(c3, alpha) },
            ];
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: windows_numerics::Vector2 { X: x, Y: y },
                endPoint: windows_numerics::Vector2 { X: x + w, Y: y },
            };
            let coll = match self
                .rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let brush = match self.rt.CreateLinearGradientBrush(&props, None, &coll) {
                Ok(b) => b,
                Err(_) => return,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            self.rt.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// 直线
    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, c: u32, alpha: f32, width: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            self.rt.DrawLine(
                windows_numerics::Vector2 { X: x1, Y: y1 },
                windows_numerics::Vector2 { X: x2, Y: y2 },
                &brush,
                width,
                None,
            );
        }
    }

    /// 圆环描边
    pub fn circle_stroke(&self, cx: f32, cy: f32, r: f32, c: u32, alpha: f32, sw: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            let e = D2D1_ELLIPSE {
                point: windows_numerics::Vector2 { X: cx, Y: cy },
                radiusX: r,
                radiusY: r,
            };
            self.rt.DrawEllipse(&e, &brush, sw, None);
        }
    }

    /// 绘制到内存位图（不提交到分层窗口）。返回 memdc，调用方自行 BitBlt 到窗口。
    pub fn draw_to_mem(&mut self, w: i32, h: i32, bg: u32, draw: impl FnOnce(&mut Canvas, f32, f32)) -> HDC {
        self.ensure_size(w, h);
        self.glassy = false;
        self.glass_bg_alpha = 1.0;
        self.has_blur = false;
        self.rt = self.dcrt_base.clone();
        *self.brush.borrow_mut() = None;
        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&col(bg, 1.0)));
            draw(self, w as f32, h as f32);
            if self.rt.EndDraw(None, None).is_err() {
                // 忽略：下一次 WM_PAINT 会重画
            }
        }
        self.memdc
    }

    #[allow(dead_code)]
    pub fn memdc(&self) -> HDC {
        self.memdc
    }

    /// 进度条
    pub fn progress(&self, x: f32, y: f32, w: f32, h: f32, ratio: f32, fg: u32, bg: u32) {
        self.rrect(x, y, w, h, h / 2.0, bg, 1.0);
        let fw = (w * ratio.clamp(0.0, 1.0)).max(h);
        self.rrect(x, y, fw.min(w), h, h / 2.0, fg, 1.0);
    }

    /// 后续绘制整体平移（绝对变换，非累加）。传 (0,0) 复位。
    pub fn offset(&self, dx: f32, dy: f32) {
        unsafe {
            self.rt.SetTransform(&windows_numerics::Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: dx,
                M32: dy,
            });
        }
    }

    /// 压入矩形裁剪区（用于页面滑动时裁掉溢出内容）
    pub fn push_clip(&self, x: f32, y: f32, w: f32, h: f32) {
        unsafe {
            self.rt.PushAxisAlignedClip(
                &D2DC::D2D_RECT_F {
                    left: x,
                    top: y,
                    right: x + w.max(0.0),
                    bottom: y + h.max(0.0),
                },
                D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            );
        }
    }

    pub fn pop_clip(&self) {
        unsafe {
            self.rt.PopAxisAlignedClip();
        }
    }
}

/// 字重快捷常量
pub const W_NORMAL: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT(400);
pub const W_MED: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT(500);
pub const W_SEMI: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT(600);
pub const W_BOLD: DWRITE_FONT_WEIGHT = DWRITE_FONT_WEIGHT(700);

#[allow(dead_code)]
fn _pcwstr_marker(_p: PCWSTR) {}
