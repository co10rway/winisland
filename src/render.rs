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
use windows::core::{w, PCWSTR};

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
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let screen = GetWindowDC(None);
            let memdc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            Ok(Canvas {
                dcrt,
                dwrite,
                fmts: HashMap::new(),
                brush: std::cell::RefCell::new(None),
                memdc,
                hbmp: HBITMAP::default(),
                cap_w: 0,
                cap_h: 0,
                w: 0,
                h: 0,
            })
        }
    }

    /// 取一个已设置好颜色的纯色画刷（复用同一对象）。返回 clone 只是 AddRef，
    /// 比 CreateSolidColorBrush 便宜一个数量级。
    fn solid(&self, c: u32, alpha: f32) -> Option<ID2D1SolidColorBrush> {
        let color = col(c, alpha);
        let mut slot = self.brush.borrow_mut();
        if slot.is_none() {
            match unsafe { self.dcrt.CreateSolidColorBrush(&color, None) } {
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
        unsafe {
            self.dcrt.BeginDraw();
            self.dcrt.Clear(Some(&D2DC::D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            draw(self, w as f32, h as f32);
            if self.dcrt.EndDraw(None, None).is_err() {
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
            self.dcrt.FillRectangle(
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
            self.dcrt.FillRoundedRectangle(&rr, &brush);
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
            self.dcrt.DrawRoundedRectangle(&rr, &brush, sw, None);
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
            self.dcrt.FillEllipse(&e, &brush);
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
            self.dcrt.DrawText(
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
                .dcrt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            {
                Ok(c) => c,
                Err(_) => return,
            };
            let brush = match self.dcrt.CreateLinearGradientBrush(&props, None, &coll) {
                Ok(b) => b,
                Err(_) => return,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect: D2DC::D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
                radiusX: r,
                radiusY: r,
            };
            self.dcrt.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// 直线
    pub fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, c: u32, alpha: f32, width: f32) {
        let Some(brush) = self.solid(c, alpha) else { return };
        unsafe {
            self.dcrt.DrawLine(
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
            self.dcrt.DrawEllipse(&e, &brush, sw, None);
        }
    }

    /// 绘制到内存位图（不提交到分层窗口）。返回 memdc，调用方自行 BitBlt 到窗口。
    pub fn draw_to_mem(&mut self, w: i32, h: i32, bg: u32, draw: impl FnOnce(&mut Canvas, f32, f32)) -> HDC {
        self.ensure_size(w, h);
        unsafe {
            self.dcrt.BeginDraw();
            self.dcrt.Clear(Some(&col(bg, 1.0)));
            draw(self, w as f32, h as f32);
            if self.dcrt.EndDraw(None, None).is_err() {
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
            self.dcrt.SetTransform(&windows_numerics::Matrix3x2 {
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
            self.dcrt.PushAxisAlignedClip(
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
            self.dcrt.PopAxisAlignedClip();
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
