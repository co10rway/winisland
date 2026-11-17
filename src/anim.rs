//! 动画内核
//!
//! 所有交互过渡（展开/收起、翻页、悬停、进度填充）都通过这里推进。
//!
//! 采用**临界阻尼弹簧**而非固定步长的线性插值：
//! - 用解析解积分，与帧率无关（掉帧/高刷都不会变速）
//! - 起步有加速度、收尾自然减速，不做匀速"机器感"
//! - 阻尼比取 1.0，理论上不过冲，尺寸变化的布局不会抖动

use std::time::Instant;

use crate::ui::HitId;

// ---------------- 弹簧 ----------------

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    /// 当前值
    pub v: f32,
    /// 目标值
    pub target: f32,
    /// 速度（像素/秒 或 单位/秒）
    vel: f32,
    /// 角频率 rad/s：越大越快。18 ≈ 0.30s 收尾，26 ≈ 0.20s
    pub omega: f32,
    /// 到位阈值
    pub eps: f32,
}

impl Spring {
    pub const fn new(v: f32, omega: f32) -> Self {
        Self { v, target: v, vel: 0.0, omega, eps: 0.0012 }
    }

    /// 直接置位（不做动画）
    pub fn set(&mut self, v: f32) {
        self.v = v;
        self.target = v;
        self.vel = 0.0;
    }

    /// 给一个初速度，让过渡"带势"启动，避免从 0 速度缓慢加速的黏滞感
    pub fn kick(&mut self, vel: f32) {
        self.vel = vel;
    }

    pub fn settled(&self) -> bool {
        (self.v - self.target).abs() <= self.eps && self.vel.abs() <= self.eps * 8.0
    }

    /// 推进 dt 秒并返回当前值。
    /// 临界阻尼解析解：x(t) = (x0 + (v0 + ω·x0)·t)·e^(−ωt)，任意 dt 都稳定。
    pub fn step(&mut self, dt: f32) -> f32 {
        if self.settled() {
            self.v = self.target;
            self.vel = 0.0;
            return self.v;
        }
        let w = self.omega;
        let x0 = self.v - self.target;
        let v0 = self.vel;
        let e = (-w * dt).exp();
        let k = v0 + w * x0;
        self.v = self.target + (x0 + k * dt) * e;
        self.vel = (v0 - w * k * dt) * e;
        self.v
    }
}

// ---------------- 缓动 ----------------

/// 三次缓出：用于纯时间轴（非线性弹簧）场景
#[allow(dead_code)]
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// 颜色分量线性插值
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t.clamp(0.0, 1.0)
}

/// 0xRRGGBB 颜色插值
pub fn mix(c1: u32, c2: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| ((c1 >> s) & 0xFF) as f32;
    let dh = |s: u32| ((c2 >> s) & 0xFF) as f32;
    let r = lerp(ch(16), dh(16), t) as u32;
    let g = lerp(ch(8), dh(8), t) as u32;
    let b = lerp(ch(0), dh(0), t) as u32;
    (r << 16) | (g << 8) | b
}

// ---------------- 状态容器 ----------------

pub struct Anims {
    /// 展开程度 0..1，直接决定窗口高度（胶囊 58 → 面板 254）
    pub open: Spring,
    /// 面板背景不透明度（快速变实，负责把胶囊"盖住"）
    pub panel: Spring,
    /// 面板内容：淡入 + 上浮
    pub content: Spring,
    /// 胶囊层不透明度
    pub pill: Spring,
    /// 胶囊内文切换（提醒/验证码出现时淡入）
    pub pill_content: Spring,
    /// 翻页过渡进度 0..1
    pub page: Spring,
    /// 翻页时滑出的旧页索引
    pub page_from: usize,
    /// 翻页方向：+1 从右进 / -1 从左进
    pub page_dir: f32,
    /// 饮水进度 0..1（平滑填充，不跳档）
    pub water: Spring,
    /// 胶囊右侧信息的上下轮播进度 0..1
    pub rot: Spring,
    /// 小黑条程度 0..1（1 = 收成无内容的迷你黑条）
    pub mini: Spring,
    /// 各控件的悬停进度（0 无 / 1 完全悬停）
    hovers: Vec<(HitId, Spring)>,
    /// 上一帧时间戳
    pub last: Instant,
}

impl Anims {
    pub fn new() -> Self {
        Self {
            open: Spring::new(0.0, 21.0),
            panel: Spring::new(0.0, 34.0),
            content: Spring::new(0.0, 13.0),
            pill: Spring::new(1.0, 30.0),
            pill_content: Spring::new(1.0, 18.0),
            page: Spring::new(1.0, 24.0),
            page_from: 0,
            page_dir: 1.0,
            water: Spring::new(0.0, 15.0),
            rot: Spring::new(1.0, 26.0),
            mini: Spring::new(0.0, 18.0),
            hovers: Vec::new(),
            last: Instant::now(),
        }
    }

    /// 启动一次翻页过渡
    pub fn page_to(&mut self, from: usize, dir: f32) {
        self.page_from = from;
        self.page_dir = if dir >= 0.0 { 1.0 } else { -1.0 };
        self.page.v = 0.0;
        self.page.target = 1.0;
        self.page.kick(13.0); // 带初速：起手就有位移，收尾自然减速
    }

    /// 启动一次胶囊右侧信息的上滑轮播（与翻页同一套弹簧手感）
    pub fn rot_to(&mut self) {
        self.rot.v = 0.0;
        self.rot.target = 1.0;
        self.rot.kick(14.0); // 带初速：起手就位移，收尾自然减速
    }

    /// 推进悬停进度：当前悬停的控件趋向 1，其余趋向 0
    pub fn hover_step(&mut self, hover: Option<&HitId>, dt: f32) {
        for (id, sp) in self.hovers.iter_mut() {
            let on = hover.map_or(false, |h| h == &*id);
            sp.target = if on { 1.0 } else { 0.0 };
            sp.step(dt);
        }
        if let Some(h) = hover {
            if !self.hovers.iter().any(|(k, _)| k == h) {
                let mut sp = Spring::new(0.0, 26.0);
                sp.target = 1.0;
                self.hovers.push((h.clone(), sp));
            }
        }
        // 已经淡出到 0 且不再悬停的项直接回收
        self.hovers.retain(|(id, sp)| !(sp.v <= 0.004 && Some(id) != hover));
    }

    /// 某控件的悬停进度 0..1
    pub fn hover_p(&self, id: &HitId) -> f32 {
        self.hovers
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, sp)| sp.v.clamp(0.0, 1.0))
            .unwrap_or(0.0)
    }

    /// 是否还有过渡在跑（决定要不要继续出帧）
    pub fn busy(&self) -> bool {
        !(self.open.settled()
            && self.panel.settled()
            && self.content.settled()
            && self.pill.settled()
            && self.pill_content.settled()
            && self.page.settled()
            && self.water.settled()
            && self.rot.settled()
            && self.mini.settled())
            || !self.hovers.is_empty()
    }
}
