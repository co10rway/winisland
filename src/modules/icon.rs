//! 灵动岛图标 —— 胶囊最左侧那枚「一直活着」的动态声浪
//!
//! 唯一形态：**声浪 glow** —— 参照播放器频谱效果图：
//! - 一排密集细条从底部基线升起，顶部圆润
//! - 包络是平滑的频谱形状：中部群峰高耸、向两端以细小的尾巴延展
//! - 三层辉光：外圈大而淡的光晕 → 中层柔光 → 中心亮芯，叠出「发光」质感
//! - 一条极淡的基线横贯底部
//!
//! 反馈敏感：energy 由「打字速度 + 系统音量峰值」合成，起音 ~30ms 跟手、
//! 落音稍慢；能量抬高群峰、加亮辉光、加快流动。空闲时低伏缓呼吸，不死寂。
//! 只用 Canvas 基础图元，胶囊（26×22）与设置页预览共用同一入口。

use crate::anim::mix;
use crate::modules::{audio, typing};
use crate::render::Canvas;
use crate::theme;

/// (id, 名称) —— id 存进 config.json，一旦发布不可改
pub const ICON_KINDS: [(&str, &str); 1] = [("glow", "声浪")];

/// 未知 id（历史版本的全部旧 id）回落到唯一形态
pub fn index(id: &str) -> usize {
    let _ = id;
    0
}

pub fn name(_id: &str) -> &'static str {
    ICON_KINDS[0].1
}

#[derive(Clone, Copy, Debug)]
pub struct Signal {
    /// 综合活跃能量 0..1（打字 + 声音，已平滑）
    pub energy: f32,
    /// 累积相位（秒），活跃时推进更快
    pub phase: f32,
}

/// 每帧调用一次：把「按键 / 声音」的瞬时输入立刻变成波形能量。
///
/// 响应模型（用户要求：快升快降，不要慢半拍）
/// - **键盘**：每次按键注入一个尖峰，**当帧生效**，不经过任何平滑；
/// - **声音**：直接读未平滑的瞬时峰值，有声音立刻顶满；
/// - **回落**：没有任何新输入时按 ~90ms 时间常数立刻塌下来（0.3 秒内基本归零），
///   而不是像以前那样靠「最近 6 秒按键数」慢慢退。
pub fn advance(app: &mut crate::app::App, dt: f32) {
    // 每次按键 = 一个立刻可见的尖峰；连打时脉冲叠加，能量维持在高位
    let imp = typing::take_impulse();
    if app.cfg.modules.typing && imp > 0 {
        let add = 0.34 + 0.14 * (imp.min(3) as f32);
        app.icon_energy = (app.icon_energy + add).min(1.0);
    }

    // 声音：瞬时峰值直接顶上（不做平滑），停止发声后下一帧就往下掉
    if app.cfg.sound_react {
        let s = (audio::raw() * 2.8).min(1.0);
        if s > app.icon_energy {
            app.icon_energy = s;
        }
    }

    // 衰减：无新输入时快速回落（~90ms 常数，肉眼是「立刻降下去」）
    let k = 1.0 - (-dt * 22.0).exp();
    app.icon_energy -= app.icon_energy * k;
    if app.icon_energy < 0.002 {
        app.icon_energy = 0.0;
    }

    // 相位恒速推进（空闲也在动）；能量越高流动越快，尖峰时视觉上会「抖一下」
    app.icon_phase += dt * (1.0 + app.icon_energy * 2.8);
    if app.icon_phase > 10_000.0 {
        app.icon_phase = 0.0; // 长时间运行后避免 f32 精度退化
    }
}

/// 是否处于「高活跃」期：决定帧循环是全速 vsync 还是空闲节流 ~30fps
pub fn active(app: &crate::app::App) -> bool {
    app.icon_energy > 0.14
        || (app.cfg.modules.typing && typing::idle_secs_since_key() < 4)
        || (app.cfg.sound_react && audio::peak() > 0.035)
}

/// 绘制入口。`cx/cy` 为图标中心，`w/h` 为包围盒尺寸（均为物理像素）
pub fn draw_icon(cv: &mut Canvas, _id: &str, cx: f32, cy: f32, w: f32, h: f32, sig: Signal, alpha: f32) {
    draw_glow(cv, cx, cy, w, h, sig, alpha);
}

/// 高斯峰包络
fn bump(u: f32, c: f32, sigma: f32) -> f32 {
    let d = (u - c) / sigma;
    (-d * d * 0.5).exp()
}

/// 声浪：底部对齐的密集细条 + 三层辉光 + 淡基线
fn draw_glow(cv: &mut Canvas, cx: f32, cy: f32, w: f32, h: f32, sig: Signal, alpha: f32) {
    let e = sig.energy;
    let n = 15usize; // 条数：细条之间要留出肉眼可见的暗隙，才有「频谱条」的质感
    let step = w / n as f32;
    let core = (step * 0.52).max(0.9); // 亮芯宽度（约留一半间隙）
    let baseline = cy + h / 2.0 - 1.0;
    let t = sig.phase * 1.45;

    // 三座高斯峰随相位漂移 → 离散的频谱群峰
    let c1 = 0.42 + 0.16 * t.sin();
    let c2 = 0.34 + 0.13 * (t * 1.7 + 1.2).sin();
    let c3 = 0.63 + 0.14 * (t * 1.3 + 3.1).sin();

    // 极淡基线，把整排细条「托」在一条线上（参考图的那条横线）
    let line_col = mix(theme::main(), 0xFFFFFF, 0.25 + e * 0.20);
    cv.line(cx - w / 2.0, baseline, cx + w / 2.0, baseline, line_col, alpha * (0.10 + 0.10 * e), 1.0);

    for i in 0..=n {
        let u = i as f32 / n as f32;
        // 相邻条各走各的微相位 → 频谱特有的锯齿纹理，而不是一条光滑弧线
        let jitter = 0.70 + 0.30 * (i as f32 * 2.9 + t * 3.1).sin().abs();
        let v = ((0.34 + e * 0.72) * bump(u, c1, 0.11)
            + (0.24 + e * 0.52) * bump(u, c2, 0.062)
            + (0.21 + e * 0.46) * bump(u, c3, 0.055)
            + 0.09
            + 0.06 * (u * 21.0 + t * 2.2).sin())
            * jitter;
        // 两端收细但不归零：保留延展到边缘的小尾巴
        let fade = (u * std::f32::consts::PI).sin().powf(0.40);
        let v = (v * fade).clamp(0.02, 1.0);
        let hh = (v * h * (0.62 + e * 0.38)).max(0.5);
        let x = cx - w / 2.0 + step * i as f32;

        // 三层辉光：外晕（宽而淡）→ 柔光 → 亮芯
        let col = mix(theme::main(), 0xFFFFFF, 0.16 + e * 0.22);
        let hot = mix(theme::light(), 0xFFFFFF, 0.15 + v * 0.40);
        let r = core / 2.0;
        cv.rrect(x - core * 1.2, baseline - hh, core * 2.4, hh, r, col, alpha * (0.06 + 0.12 * v));
        cv.rrect(x - core * 0.8, baseline - hh, core * 1.6, hh, r, col, alpha * (0.14 + 0.24 * v));
        cv.rrect(x - r / 2.0, baseline - hh, core, hh, r / 2.0, hot, alpha * (0.55 + 0.45 * v));
    }
}
