//! 打字统计 — WH_KEYBOARD_LL 低级钩子只做按键计数，
//! 绝不记录键值、窗口标题或任何内容（隐私：零收集）。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

pub static KEYS_TOTAL: AtomicU64 = AtomicU64::new(0);
static BUCKET: AtomicU32 = AtomicU32::new(0);
/// 上次按键时刻（秒计数）；u64::MAX = 从未按键（不能用 0 当哨兵：
/// 启动后第一秒内按键时 TICK_NOW 也是 0，会被误判成「从未按键」）
static LAST_KEY_TICK: AtomicU64 = AtomicU64::new(u64::MAX);
static TICK_NOW: AtomicU64 = AtomicU64::new(0);

/// 近 90 秒每秒按键数环形缓冲
static SEC_RING: Mutex<[u32; 90]> = Mutex::new([0u32; 90]);

/// 键盘脉冲：每次按键 +1，图标读取并清零 —— 让按键「按下的瞬间」就产生波形反馈，
/// 不再依赖秒级窗口统计（那样要攒几下才升、停手后要等几秒才降）
static IMPULSE: AtomicU32 = AtomicU32::new(0);

/// 钩子回调里调用：仅 +1 计数
pub fn bump_key() {
    KEYS_TOTAL.fetch_add(1, Ordering::Relaxed);
    BUCKET.fetch_add(1, Ordering::Relaxed);
    IMPULSE.fetch_add(1, Ordering::Relaxed);
    LAST_KEY_TICK.store(TICK_NOW.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// 取走自上次调用以来的按键脉冲数（读取即清零）
pub fn take_impulse() -> u32 {
    IMPULSE.swap(0, Ordering::Relaxed)
}

/// 主循环每秒调用：把这一秒的按键数写入环形缓冲
pub fn tick_second() {
    let now_tick = TICK_NOW.fetch_add(1, Ordering::Relaxed);
    let n = BUCKET.swap(0, Ordering::Relaxed);
    let idx = (now_tick as usize + 1) % 90;
    if let Ok(mut ring) = SEC_RING.lock() {
        ring[idx] = n;
    }
}

/// 最近 n 秒（含当前秒桶）按键数
pub fn recent_keys(n: usize) -> u32 {
    let now_tick = TICK_NOW.load(Ordering::Relaxed);
    let cur = BUCKET.load(Ordering::Relaxed);
    let mut sum = cur;
    if let Ok(ring) = SEC_RING.lock() {
        for i in 0..n.min(89) {
            let idx = (now_tick as usize).wrapping_sub(i) % 90;
            sum += ring[idx];
        }
    }
    sum
}

/// 最后一次按键距今多少秒；从未按键时返回 u64::MAX
pub fn idle_secs_since_key() -> u64 {
    let last = LAST_KEY_TICK.load(Ordering::Relaxed);
    if last == u64::MAX {
        return u64::MAX;
    }
    TICK_NOW.load(Ordering::Relaxed).saturating_sub(last)
}

/// 今日总字数（按键数近似，1 按键 ≈ 1 字符）
#[allow(dead_code)]
pub fn keys_today(store: &crate::store::Store) -> u64 {
    store.typing_keys
}

/// 最近 60 秒 WPM（按键 / 5）
pub fn wpm_now() -> u64 {
    (recent_keys(60) as u64) / 5
}

// 波形与图标的绘制统一放在 `modules::icon`，这里只提供能量输入：
// `recent_keys` / `wpm_now` / `idle_secs_since_key`。
