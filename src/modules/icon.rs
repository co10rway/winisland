//! 灵动岛图标 —— 胶囊最左侧那枚「三态」动态图标
//!
//! 三种形态对应三份 SVG 设计稿（assets/ 下同名文件）：
//! - 默认 `dynamic-island.svg`：极光胶囊 + 呼吸圆点 + 三根静态声波条
//! - 键盘 `island-typing.svg`：三颗按键 + 空格进度条，按键随打字闪烁/下压
//! - 音乐 `island-music.svg`：五根频谱条，高度跟随系统声音峰值
//!
//! 双尺度渲染：
//! - **小尺寸（<48px，胶囊槽位 / 设置预览）**：丢掉外层方底板，极光胶囊直接
//!   铺满槽位（横向按 192 设计宽、纵向按 72 设计高各自缩放），内部元素比
//!   等比缩放大约 2 倍 —— 方底板在 22~30px 时会把按键/频谱条压成糊点；
//! - **大尺寸（≥48px，预览导出 / 展开面板）**：完整绘制 256×256 方底板设计稿。
//!
//! 形态自动切换（每帧判定，切换带 ~0.22s 交叉淡入 + 弹跳）：
//! - 声音活跃（sound_react 开启且峰值超阈值）→ 音乐态；
//! - 最近 ~2 秒内有按键（modules.typing 开启）→ 键盘态；
//! - 其余时间 → 默认态（低伏缓呼吸，不死寂）。
//!
//! 只用 Canvas 基础图元（圆角矩形 / 圆 / 渐变），全部入口共用同一绘制代码。

use crate::anim::mix;
use crate::modules::{audio, typing};
use crate::render::Canvas;

/// (id, 名称) —— id 存进 config.json，一旦发布不可改
pub const ICON_KINDS: [(&str, &str); 1] = [("glow", "灵动岛")];

/// 三态编号（Signal.state / App.icon_state）
pub const ST_DEFAULT: u8 = 0;
pub const ST_TYPING: u8 = 1;
pub const ST_MUSIC: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub struct Signal {
    /// 综合活跃能量 0..1（打字 + 声音，已平滑）
    pub energy: f32,
    /// 累积相位（秒），活跃时推进更快
    pub phase: f32,
    /// 当前形态（ST_*）；无 App 状态的预览场景传 ST_DEFAULT
    pub state: u8,
    /// 切换前的形态（交叉淡入的出场方）
    pub prev: u8,
    /// 形态切换进度 0..1（1 = 稳定在 state）
    pub blend: f32,
    /// 键盘态：本次被点亮的是哪颗键（0..2）与距该次敲击的秒数
    pub key_idx: u8,
    pub key_age: f32,
}

/// 每帧调用一次：推进能量/相位，并判定当前该显示哪种形态。
///
/// 能量响应模型（快升快降，不要慢半拍）：
/// - **键盘**：每次按键注入一个尖峰，**当帧生效**，不经过任何平滑；
/// - **声音**：直接读未平滑的瞬时峰值，有声音立刻顶满；
/// - **回落**：没有任何新输入时按 ~90ms 时间常数立刻塌下来。
pub fn advance(app: &mut crate::app::App, dt: f32) {
    // —— 能量合成（键盘尖峰 + 声音瞬时峰值，增益调大让动效更「跳」）——
    let imp = typing::take_impulse();
    if app.cfg.modules.typing && imp > 0 {
        let add = 0.45 + 0.18 * (imp.min(3) as f32);
        app.icon_energy = (app.icon_energy + add).min(1.0);
        // 每敲一次随机点亮一颗键（LCG 散列，尽量不重复上一颗）——不是三颗齐闪
        app.icon_key_seed = app.icon_key_seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut next = ((app.icon_key_seed >> 16) % 3) as u8;
        if next == app.icon_key_idx {
            next = (next + 1 + ((app.icon_key_seed >> 9) & 1) as u8) % 3;
        }
        app.icon_key_idx = next;
        app.icon_key_age = 0.0;
    }
    // 选键衰减计时：连续敲击时不断被重置，停手后迅速过期
    app.icon_key_age = (app.icon_key_age + dt).min(9.0);

    if app.cfg.sound_react {
        let s = (audio::raw() * 3.2).min(1.0);
        if s > app.icon_energy {
            app.icon_energy = s;
        }
    }

    let k = 1.0 - (-dt * 22.0).exp();
    app.icon_energy -= app.icon_energy * k;
    if app.icon_energy < 0.002 {
        app.icon_energy = 0.0;
    }

    app.icon_phase += dt * (1.0 + app.icon_energy * 3.2);
    if app.icon_phase > 10_000.0 {
        app.icon_phase = 0.0; // 长时间运行后避免 f32 精度退化
    }

    // —— 形态判定：声音 > 键盘 > 默认（各带迟滞防抖，避免高频闪烁）——
    // 声音：峰值冒头即续命 1.2s —— 音乐安静段里音量会掉到 0，
    // 没有这段保持就会在「音乐/默认」之间来回闪
    let sound_hot = app.cfg.sound_react && (audio::peak() > 0.03 || audio::raw() > 0.12);
    app.icon_sound_hold = if sound_hot {
        1.2
    } else {
        (app.icon_sound_hold - dt).max(0.0)
    };
    let sound_active = app.cfg.sound_react && app.icon_sound_hold > 0.0;
    // 键盘：最后一次按键在 2 秒内（按秒粒度计数，自带 2s 迟滞）
    let typing_active = app.cfg.modules.typing && typing::idle_secs_since_key() < 2;
    let target = if sound_active {
        ST_MUSIC
    } else if typing_active {
        ST_TYPING
    } else {
        ST_DEFAULT
    };
    if target != app.icon_state {
        app.icon_state_prev = app.icon_state;
        app.icon_state = target;
        app.icon_blend = 0.0;
    }
    if app.icon_blend < 1.0 {
        app.icon_blend = (app.icon_blend + dt * 4.5).min(1.0); // ~0.22s 完成
        if app.icon_blend >= 1.0 {
            app.icon_state_prev = app.icon_state;
        }
    }
}

/// 是否处于「高活跃」期：决定帧循环是全速 vsync 还是空闲节流 ~30fps
pub fn active(app: &crate::app::App) -> bool {
    app.icon_energy > 0.14
        || app.icon_blend < 1.0
        || (app.cfg.modules.typing && typing::idle_secs_since_key() < 4)
        || (app.cfg.sound_react && audio::peak() > 0.035)
}

/// 绘制入口。`cx/cy` 为图标中心，`w/h` 为包围盒尺寸（均为物理像素）。
///
/// 内容坐标全部来自 256×256 设计稿：横向以极光胶囊宽 192、纵向以胶囊高 72
/// 为基准各自缩放（小尺寸徽章模式）或整体等比缩放（大尺寸底板模式）。
#[allow(clippy::too_many_arguments)]
pub fn draw_icon(cv: &mut Canvas, _id: &str, cx: f32, cy: f32, w: f32, h: f32, sig: Signal, alpha: f32) {
    let small = w.min(h) < 48.0;
    let (kx, ky) = if small {
        // 徽章模式：极光胶囊铺满槽位，内容元素放大约 2 倍
        (w / 192.0, h / 72.0)
    } else {
        let k = w.min(h) / 256.0;
        (k, k)
    };

    // 背景：小尺寸画极光徽章（亮渐变环 + 深芯），大尺寸画完整方底板
    if small {
        // 高能量时徽章外圈泛一层同色光晕：打字/放歌时整枚图标像被点亮
        if sig.energy > 0.2 {
            let g = 2.5 * (h / 24.0);
            cv.rrect_grad3(cx - w / 2.0 - g, cy - h / 2.0 - g, w + 2.0 * g, h + 2.0 * g, h / 2.0 + g, A_C, A_B, A_P, alpha * 0.14 * sig.energy);
        }
        let inset = (0.075 * h).max(1.5);
        cv.rrect_grad3(cx - w / 2.0, cy - h / 2.0, w, h, h / 2.0, A_C, A_B, A_P, alpha);
        cv.rrect(
            cx - w / 2.0 + inset,
            cy - h / 2.0 + inset,
            w - 2.0 * inset,
            h - 2.0 * inset,
            (h - 2.0 * inset) / 2.0,
            C_INNER,
            alpha,
        );
    } else {
        draw_tile(cv, cx, cy, kx, alpha);
    }

    // 切换弹跳：进场放大冲过头再回落（0.82→峰值~1.05→1.0），出场轻微收缩
    let b = sig.blend;
    let pop_in = 0.82 + 0.18 * b + 0.13 * (b * std::f32::consts::PI).sin();
    let pop_out = 1.0 - 0.18 * b;

    draw_dot(cv, cx, cy, kx * pop_in, ky * pop_in, sig, alpha);
    if b < 1.0 && sig.prev != sig.state {
        draw_state(cv, sig.prev, cx, cy, kx * pop_out, ky * pop_out, sig, alpha * (1.0 - b));
    }
    draw_state(cv, sig.state, cx, cy, kx * pop_in, ky * pop_in, sig, alpha * b);
}

// ---------------------------------------------------------------------------
// 公共部件（设计稿 256 坐标 → 本地：cx + (x-128)*kx，cy + (y-124)*ky）
// ---------------------------------------------------------------------------

/// 大尺寸完整底板：深蓝渐变圆角方 + 白描边 + 极光胶囊 + 深色内芯
fn draw_tile(cv: &mut Canvas, cx: f32, cy: f32, k: f32, alpha: f32) {
    // 底板 240×240 r56：#202B48 → #090D18（对角渐变近似为纵向）
    cv.rrect_grad(cx - 120.0 * k, cy - 116.0 * k, 240.0 * k, 240.0 * k, 56.0 * k, 0x202B48, 0x090D18, false, alpha);
    // 内描边 #FFFFFF @9%，宽 2
    cv.rrect_stroke(cx - 117.0 * k, cy - 113.0 * k, 234.0 * k, 234.0 * k, 53.0 * k, 0xFFFFFF, alpha * 0.09, (2.0 * k).max(0.6));
    // 极光外晕（y 下移 6，15% 透明）
    cv.rrect_grad3(cx - 96.0 * k, cy - 30.0 * k, 192.0 * k, 74.0 * k, 37.0 * k, A_C, A_B, A_P, alpha * 0.15);
    // 极光胶囊主体（三段横向渐变）
    cv.rrect_grad3(cx - 96.0 * k, cy - 36.0 * k, 192.0 * k, 72.0 * k, 36.0 * k, A_C, A_B, A_P, alpha);
    // 内芯（深底，状态内容画在它上面）
    cv.rrect(cx - 90.0 * k, cy - 30.0 * k, 180.0 * k, 60.0 * k, 30.0 * k, C_INNER, alpha);
}

/// 呼吸圆点（三态共用）：空闲缓慢呼吸，能量高时明显放大、加亮、双层光晕
fn draw_dot(cv: &mut Canvas, cx: f32, cy: f32, kx: f32, ky: f32, sig: Signal, alpha: f32) {
    let e = sig.energy;
    let breathe = 0.5 + 0.5 * (sig.phase * 1.4).sin();
    // 呼吸加强：半径摆幅 14%→28%，透明度谷底 0.58→0.34（明暗对比拉开一倍多）
    let r = 8.0 * ky * (1.0 + 0.28 * breathe + 0.35 * e);
    let a = (alpha * (0.34 + 0.62 * breathe + 0.15 * e)).min(alpha);
    let dx = cx - 53.0 * kx; // 设计稿 x=75 → 128-53
    if e > 0.15 {
        // 双层光晕：能量越高越亮，敲键/重低音时圆点像被「点亮」
        cv.circle(dx, cy, r * 2.2, C_DOT, a * 0.22 * e);
        cv.circle(dx, cy, r * 3.4, C_DOT, a * 0.08 * e);
    } else {
        // 空闲呼吸柔光：吸气时泛出一圈微光，呼吸感更立体
        cv.circle(dx, cy, r * 1.9, C_DOT, a * 0.16 * breathe);
    }
    cv.circle(dx, cy, r, C_DOT, a);
}

// ---------------------------------------------------------------------------
// 三态内容
// ---------------------------------------------------------------------------

/// 默认态：三根静态声波条（设计稿 x154/172/190 → 中心偏移 26/44/62，
/// 高 10/22/12，宽 8，全部垂直居中），空闲 ±14% 缓呼吸
fn draw_default(cv: &mut Canvas, cx: f32, cy: f32, kx: f32, ky: f32, sig: Signal, alpha: f32) {
    let e = sig.energy;
    const BARS: [(f32, f32, u32); 3] = [(26.0, 10.0, 0x6A9CFF), (44.0, 22.0, 0x9189FF), (62.0, 12.0, 0xBE8BFF)];
    for (i, &(dx, bh, c)) in BARS.iter().enumerate() {
        let i = i as f32;
        // 呼吸加强：摆幅 ±26% + 波速提到 2.2，三根条相位错开形成波浪感
        let sway = 1.0 + 0.26 * (sig.phase * 2.2 + i * 1.1).sin();
        let hh = bh * ky * sway * (1.0 + 0.7 * e);
        let bw = (8.0 * kx).max(1.7);
        // 亮度也随呼吸起伏（暗↔亮循环），不只是高度在动
        let glow = 0.5 + 0.5 * (sig.phase * 2.2 + i * 1.1).sin();
        let col = mix(mix(c, 0xFFFFFF, 0.18 + 0.30 * glow), 0xFFFFFF, (0.35 * e).min(0.4));
        cv.rrect(cx + dx * kx - bw / 2.0, cy - hh / 2.0, bw, hh, bw / 2.0, col, alpha);
    }
}

/// 键盘态：三颗按键（设计稿 x103/135/167 → 偏移 -25/7/39，24×21 r5 @y106）
/// + 空格进度条（x103 宽 88 高 9 @y134）
fn draw_typing(cv: &mut Canvas, cx: f32, cy: f32, kx: f32, ky: f32, sig: Signal, alpha: f32) {
    let e = sig.energy;
    const KEYS: [(f32, u32); 3] = [(-25.0, 0x568BFF), (7.0, 0xA2CBFF), (39.0, 0xAD7BFF)];
    let kw = 24.0 * kx;
    let kh = 21.0 * ky;
    let kr = (5.0 * kx.min(ky)).max(0.8);
    for (i, &(dx, c)) in KEYS.iter().enumerate() {
        // 每次按键只点亮一颗：由 App 在 advance() 里随机选中（key_idx）。
        // 波形：前 60ms 保持满亮（按键「按下」的瞬间感），随后 220ms 线性回落
        let flash = if i as u8 == sig.key_idx {
            let t = sig.key_age;
            if t < 0.06 {
                1.0
            } else {
                (1.0 - (t - 0.06) / 0.22).clamp(0.0, 1.0)
            }
        } else {
            0.0
        };
        let col = mix(c, 0xFFFFFF, (flash * 0.95).min(0.95));
        // 敲击瞬间其余两颗压暗一半，亮起的那颗对比更强（停在原位不跳变）
        let ka = if flash > 0.02 {
            alpha
        } else if sig.key_age < 0.28 {
            alpha * 0.50
        } else {
            alpha
        };
        let press = 2.6 * ky * flash; // 被敲的键明显下沉
        let grow = 1.0 + 0.10 * flash; // 亮起的键略微撑大，进一层
        let kwf = kw * grow;
        let khf = kh * grow;
        let x = cx + dx * kx - (kwf - kw) / 2.0;
        let y = cy - 18.0 * ky + press - (khf - kh) / 2.0;
        // 闪光晕：只有被敲的那颗键泛光，一眼看出「哪颗键在响应」
        if flash > 0.06 {
            cv.rrect(x - kwf * 0.45, y - khf * 0.45, kwf * 1.90, khf * 1.90, kr * 1.9, c, alpha * 0.50 * flash);
        }
        cv.rrect(x, y, kwf, khf, kr, col, ka);
    }
    // 空格条：底槽淡极光，前景按能量填充（读作「输入进度」；
    // 保底 35% 宽度，停手时也保留设计稿里的那条空格，不会缩没）
    let sx = cx - 25.0 * kx;
    let sy = cy + 10.0 * ky; // y134 → 134-124
    let sw = 88.0 * kx;
    let sh = 9.0 * ky;
    let sr = (4.5 * ky).max(0.8);
    cv.rrect_grad3(sx, sy, sw, sh, sr, A_C, A_B, A_P, alpha * 0.25);
    let fw = (sw * (0.35 + 0.65 * e)).min(sw);
    cv.rrect_grad3(sx, sy, fw, sh, sr, A_C, A_B, A_P, alpha);
}

/// 音乐态：五根频谱条（设计稿 x103/122.5/142/161.5/181 → 偏移 -25/-5.5/14/33.5/53，
/// 高 14/28/42/26/16，宽 10，全部垂直居中），高度随音量大幅起舞 + 柔光
fn draw_music(cv: &mut Canvas, cx: f32, cy: f32, kx: f32, ky: f32, sig: Signal, alpha: f32) {
    let e = sig.energy;
    const BARS: [(f32, f32, u32); 5] = [
        (-25.0, 14.0, 0x54E6F2),
        (-5.5, 28.0, 0x5BB7FF),
        (14.0, 42.0, 0x7B9AFF),
        (33.5, 26.0, 0x9A84FF),
        (53.0, 16.0, 0xBB7AFF),
    ];
    for (i, &(dx, bh, c)) in BARS.iter().enumerate() {
        let i = i as f32;
        let breathe = 1.0 + 0.12 * (sig.phase * 1.6 + i * 0.9).sin(); // 空闲缓呼吸
        let pulse = 0.5 + 0.5 * (sig.phase * 6.0 + i * 1.3).sin(); // 随能量大幅起舞
        let hh = (bh * ky * (breathe + e * 1.1 * pulse)).clamp(3.0 * ky, 48.0 * ky);
        let bw = (10.0 * kx).max(1.8);
        let col = mix(c, 0xFFFFFF, (0.45 * e * pulse + 0.10 * e).min(0.55));
        // 高能量时条底垫一层同色柔光，频谱「发光」质感
        if e > 0.25 {
            let gy = cy - hh / 2.0 - bh * ky * 0.12;
            let gh = hh + bh * ky * 0.24;
            cv.rrect(cx + dx * kx - bw * 1.1, gy, bw * 3.2, gh, bw * 1.6, c, alpha * 0.10 * e);
        }
        cv.rrect(cx + dx * kx - bw / 2.0, cy - hh / 2.0, bw, hh, bw / 2.0, col, alpha);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_state(cv: &mut Canvas, st: u8, cx: f32, cy: f32, kx: f32, ky: f32, sig: Signal, alpha: f32) {
    match st {
        ST_TYPING => draw_typing(cv, cx, cy, kx, ky, sig, alpha),
        ST_MUSIC => draw_music(cv, cx, cy, kx, ky, sig, alpha),
        _ => draw_default(cv, cx, cy, kx, ky, sig, alpha),
    }
}

// 极光三停渐变 + 内芯 + 圆点（取自三份 SVG 的公共配色）
const A_C: u32 = 0x54F5F2;
const A_B: u32 = 0x568BFF;
const A_P: u32 = 0xB16BFF;
const C_INNER: u32 = 0x080C17;
const C_DOT: u32 = 0x65FAEF;
