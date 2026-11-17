//! 主题色：全局运行时主色，岛面板与设置窗口共用。
//!
//! 主色存在原子变量里（UI 单线程，Relaxed 足够），绘制函数直接读，
//! 没有锁与查表开销；切换主题时由设置页写入并立即生效。

use std::sync::atomic::{AtomicU32, Ordering};

/// (id, 名称, 主色) —— id 存入 config.json，一旦发布不可改
pub const THEMES: [(&str, &str, u32); 7] = [
    ("blue", "海蓝", 0x4E8FFF),
    ("teal", "青碧", 0x2DD4BF),
    ("green", "青草", 0x4ADE80),
    ("purple", "罗兰", 0xA78BFA),
    ("pink", "樱粉", 0xF472B6),
    ("orange", "落橙", 0xFB923C),
    ("red", "绯红", 0xF87171),
];

static MAIN: AtomicU32 = AtomicU32::new(0x4E8FFF);

/// 启动时从配置初始化
pub fn init(id: &str) {
    let c = THEMES.iter().find(|(k, _, _)| *k == id).map(|(_, _, c)| *c).unwrap_or(0x4E8FFF);
    MAIN.store(c, Ordering::Relaxed);
}

/// 设置页点击色块：按索引切换
pub fn set_index(i: usize) {
    let c = THEMES.get(i).map(|(_, _, c)| *c).unwrap_or(0x4E8FFF);
    MAIN.store(c, Ordering::Relaxed);
}

/// 当前选中的主题索引（设置页绘制选中态用）
pub fn index() -> usize {
    let c = MAIN.load(Ordering::Relaxed);
    THEMES.iter().position(|(_, _, x)| *x == c).unwrap_or(0)
}

/// 当前主题的 id（保存配置用）
pub fn current_id() -> &'static str {
    let i = index();
    THEMES[i].0
}

/// 主色（强调、进度、选中）
#[inline]
pub fn main() -> u32 {
    MAIN.load(Ordering::Relaxed)
}

/// 提亮版（次级强调文字、悬停）
#[inline]
pub fn light() -> u32 {
    mix_to(main(), 0xFFFFFF, 0.35)
}

/// 压暗版（时间胶囊底、按钮加深）
#[inline]
pub fn dark() -> u32 {
    mix_to(main(), 0x101018, 0.62)
}

/// 极浅版（按钮悬停文字）
#[inline]
pub fn tint() -> u32 {
    mix_to(main(), 0xFFFFFF, 0.78)
}

#[inline]
fn mix_to(c1: u32, c2: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |c: u32, s: u32| ((c >> s) & 0xFF) as f32;
    let mix = |s: u32| (ch(c1, s) + (ch(c2, s) - ch(c1, s)) * t).round() as u32;
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}
