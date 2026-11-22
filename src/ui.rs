//! UI：胶囊态 + 展开面板的布局、绘制与命中测试

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, POINTL, RECT, WPARAM};
use windows::Win32::System::Ole::{IDropTarget, IDropTarget_Impl, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::{implement, Ref};

use crate::anim::mix;
use crate::app::App;
use crate::modules::{clock, salary, typing, workday};
use crate::render::{s, Canvas, W_BOLD, W_MED, W_NORMAL, W_SEMI};

// ---- 设计常量（96DPI 基准，实际绘制乘以 s()）----
// 加宽到 430：中间段要放「时间 + 状态徽标 + 日期 + 节日」，
// 右侧段要放完整的「今日工作进度」等字样，340 已经装不下
pub const PILL_W: i32 = 430;
pub const PILL_H: i32 = 58;
pub const PANEL_W: i32 = 430; // 单块紧凑摘要面板：与胶囊同宽
pub const PANEL_H: i32 = 254;

/// 小黑条（遮挡收起态）：不带任何内容的一条迷你黑条
pub const SLIVER_W: i32 = 132;
pub const SLIVER_H: i32 = 10;
const SLIVER_BG: u32 = 0x0C0C0E;

/// 工作模式下的胶囊右侧轮播条目数（今日已赚 / 已工作 / 进度 / 距午休-下班 / 打字）
const WORK_PILL_ITEMS: usize = 5;

/// 胶囊右侧轮播条目数：随模式与自选数量变化（盯盘模式 = 自选数 [+ 今日盈亏]），
/// 轮播取模必须用这个函数，不能再用固定常量。
/// 只在轮播推进时（约每 4 秒）调用一次，避免每帧读 watchlist 文件。
pub fn pill_count(app: &App) -> usize {
    if !app.cfg.stock_mode() {
        return WORK_PILL_ITEMS;
    }
    let items = crate::modules::stock::read_items();
    if items.is_empty() {
        1 // 空自选时显示一条引导
    } else {
        items.len() + usize::from(items.iter().any(|i| i.qty > 0.0))
    }
}

// ---- 调色板（对照设计稿）----
const BG: u32 = 0x131317;
const PILL_BG: u32 = 0x17171C;
const ROW: u32 = 0x212128;
const ROW2: u32 = 0x24242B;
const BORDER: u32 = 0x2B2B33;
const TXT: u32 = 0xF2F2F6;
const TXT2: u32 = 0x9A9AA4;
const TXT3: u32 = 0x66666F;
/// 数值旁的单位：比普通次级文字亮，但仍明显区别于大号数值
const UNIT: u32 = 0xC2C2CC;


const GREEN: u32 = 0x4ADE80;
const ORANGE: u32 = 0xF5A623;
/// A 股行情红涨绿跌：涨用红的醒目值（与 GREEN 相对）
const RED: u32 = 0xF4554F;

#[derive(Clone, PartialEq)]
pub enum HitId {
    Pill,
    /// 底部分页图标：悬停放大，点击直接跳到对应页
    PageDot(usize),
    Collapse,
    /// 面板右上角图钉：锁定面板，遮挡时不再自动收起
    Pin,
    /// 待办页「＋ 新增待办」按钮
    TodoAdd,
    /// 盯盘助手页「＋ 编辑自选持仓」按钮（弹程序内编辑窗口）
    WatchEdit,
    /// 待办行「完成/未完成」切换（点击整行）
    TodoToggle(usize),
    /// 待办行尾「×」删除按钮（悬停显示）
    TodoDel(usize),
    /// 文件条目「用默认程序直接打开文件」
    FileOpen(usize),
    /// 文件条目行尾「×」删除按钮（悬停显示，二次确认）
    FileDel(usize),
    ClipCopy(usize),
    WaterOk,
    WaterAdd,
    SedWalk,
    MailCopy,
    /// 面板顶部提醒条的确认按钮（久坐/喝水/日程）
    ReminderAck,
}

pub struct Hit {
    pub id: HitId,
    pub rect: RECT,
}

fn push_hit(hits: &mut Vec<Hit>, id: HitId, x: f32, y: f32, w: f32, h: f32) {
    hits.push(Hit {
        id,
        rect: RECT {
            left: x as i32,
            top: y as i32,
            right: (x + w) as i32,
            bottom: (y + h) as i32,
        },
    });
}

/// 内容可视带的纵向范围（页面裁剪区：标题栏之下、底部翻页条之上）
const BODY_CLIP_TOP: f32 = 48.0;
fn body_clip_bottom() -> f32 {
    s(PANEL_H as f32) - s(44.0)
}

/// 推入命中区并纵向裁到内容可视带——滚动后移出可视区的行不可见也不可点
fn push_hit_body(hits: &mut Vec<Hit>, id: HitId, x: f32, y: f32, w: f32, h: f32) {
    push_hit_body_from(hits, id, x, y, w, h, s(BODY_CLIP_TOP));
}

/// 同上，但可视带顶部可指定（待办页列表在固定按钮之下才开始露出）
fn push_hit_body_from(hits: &mut Vec<Hit>, id: HitId, x: f32, y: f32, w: f32, h: f32, vtop_min: f32) {
    let vtop = y.max(vtop_min);
    let vbot = (y + h).min(body_clip_bottom());
    if vbot > vtop + 1.0 {
        push_hit(hits, id, x, vtop, w, vbot - vtop);
    }
}

/// 列表页内容超出可视区的高度（滚轮可滚的最大距离）；不可滚动页返回 0
pub fn panel_scroll_max(app: &App, kind: &str) -> f32 {
    let row = if kind == "watch" { s(WATCH_ROW_H) } else { s(27.0) };
    let n = match kind {
        "schedule" => app.merged_schedule().len(),
        "todo" => app.store.todos.len(),
        "files" => app.store.files.len(),
        "clips" => app.store.clips.len(),
        "watch" => crate::modules::stock::read_items().len(),
        _ => 0,
    };
    if n == 0 {
        return 0.0;
    }
    // 待办页顶部多一块「+ 新增待办」固定按钮区；盯盘页顶部有「+ 编辑自选持仓」按钮 + 盈亏卡
    let top_offset = match kind {
        "todo" => s(TODO_ADD_H) + s(6.0),
        "watch" => s(WATCH_CARD_H) + s(8.0),
        _ => 0.0,
    };
    let content_end = s(BODY_TOP) + top_offset + n as f32 * row + s(4.0);
    (content_end - body_clip_bottom()).max(0.0)
}

// 文本宽度估算（cjk ≈ size，ascii ≈ size*0.56）
fn text_w(txt: &str, size: f32) -> f32 {
    let sz = s(size);
    txt.chars()
        .map(|ch| {
            let c = ch as u32;
            if c > 0x2E7F {
                sz
            } else {
                sz * 0.56
            }
        })
        .sum()
}

// ---------------- 总入口 ----------------

pub fn draw_frame(app: &App, cv: &mut Canvas, w: f32, h: f32, hits: &mut Vec<Hit>) -> f32 {
    let o = app.anims.open.v.clamp(0.0, 1.0);
    let m = app.anims.mini.v.clamp(0.0, 1.0);
    let mut local: Vec<Hit> = Vec::new();
    let mut scroll_max = 0.0f32;

    // 小黑条层：被遮挡收起时的一条迷你黑条，不带任何内容
    if m > 0.001 {
        draw_sliver(cv, w, m, &mut local);
    }

    // 胶囊层：展开时淡出，由面板背景接着盖住，形成交叉过渡而非硬切；
    // 收成小黑条时也随 m 淡出
    if o < 0.999 {
        let pa = app.anims.pill.v.clamp(0.0, 1.0) * (1.0 - m);
        if pa > 0.008 {
            let pw = s(PILL_W as f32);
            let ph = s(PILL_H as f32).min(h);
            let px = (w - pw) / 2.0;
            draw_pill(app, cv, px, 0.0, pw, ph, pa, &mut local);
        }
    }

    // 面板层
    if o > 0.001 {
        let bg_a = app.anims.panel.v.clamp(0.0, 1.0);
        let ct = app.anims.content.v.clamp(0.0, 1.0);
        // 内容整体从下方 10px 浮上来，与外框动效错开形成层次
        let dy = (1.0 - ct) * s(10.0);
        cv.push_clip(0.0, 0.0, w, h); // 过渡中不让内容溢出圆角
        scroll_max = draw_panel(app, cv, w, h, o, bg_a, ct, dy, &mut local);
        cv.pop_clip();
    }

    // 过渡未落定时不提供命中区：避免点到"半路"的控件
    let live = app.anims.open.settled()
        && app.anims.content.settled()
        && app.anims.page.settled()
        && app.anims.mini.settled();
    if live {
        hits.extend(local);
    }
    scroll_max
}

// ---------------- 小黑条 ----------------

/// 被其它窗口压住时收成的迷你黑条：屏幕顶部居中、无任何内容，
/// 鼠标移上去才展开成主面板胶囊
fn draw_sliver(cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let sw = s(SLIVER_W as f32);
    let sh = s(SLIVER_H as f32);
    let x = (w - sw) / 2.0;
    cv.rrect(x, 0.0, sw, sh, sh / 2.0, SLIVER_BG, alpha);
    cv.rrect_stroke(x + 0.5, 0.5, sw - 1.0, sh - 1.0, sh / 2.0, BORDER, alpha * 0.85, 1.0);
    push_hit(hits, HitId::Pill, x, 0.0, sw, sh);
}

// ---------------- 胶囊 ----------------

fn draw_pill(app: &App, cv: &mut Canvas, x: f32, y: f32, pw: f32, ph: f32, alpha: f32, hits: &mut Vec<Hit>) {
    cv.rrect(x, y, pw, ph, ph / 2.0, PILL_BG, alpha);
    cv.rrect_stroke(x + 0.5, y + 0.5, pw - 1.0, ph - 1.0, ph / 2.0, BORDER, alpha * 0.9, 1.0);
    push_hit(hits, HitId::Pill, x, y, pw, ph);

    // 内容切换（提醒/验证码插入）时淡入 + 轻微上浮，避免文字硬闪
    let pc = app.anims.pill_content.v.clamp(0.0, 1.0);
    if pc <= 0.01 {
        return;
    }
    cv.offset(0.0, (1.0 - pc) * s(5.0));
    draw_pill_content(app, cv, x, y, pw, ph, alpha * pc, hits);
    cv.offset(0.0, 0.0);
}

fn draw_pill_content(app: &App, cv: &mut Canvas, x: f32, y: f32, pw: f32, ph: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let st = clock::local_time();

    // 提醒态优先（久坐 / 喝水 / 日程）：不做超时，用户点确认才消失
    if let Some(t) = &app.reminder {
        match t.kind {
                crate::app::ToastKind::Water => {
                    let cups = app.store.water_cups;
                    let goal = app.cfg.health.water_goal_cups.max(1);
                    let msg = format!("该喝水了 · 今日 {}/{} 杯", cups.min(goal), goal);
                    let mw = text_w(&msg, 13.0);
                    let bx = x + 22.0;
                    cv.circle(bx, y + ph / 2.0, s(6.0), ORANGE, alpha);
                    cv.text_ex(bx + 12.0, y, pw - 140.0, ph, &msg, 13.0, W_SEMI, TXT, alpha, 0, 1);
                    let bar_w = 64.0_f32.min(pw - bx - 12.0 - mw - 84.0).max(20.0);
                    let bar_x = (bx + 12.0 + mw + 14.0).min(x + pw - 100.0);
                    cv.progress(bar_x, y + ph / 2.0 - s(3.0), bar_w, s(6.0), cups as f32 / goal as f32, ORANGE, ROW2);
                    // 确认按钮
                    let bw = s(58.0);
                    let btn_x = x + pw - bw - 16.0;
                    let by = y + ph / 2.0 - s(12.0);
                    cv.rrect(btn_x, by, bw, s(24.0), s(12.0), ROW2, alpha);
                    cv.rrect_stroke(btn_x + 0.5, by + 0.5, bw - 1.0, s(23.0), s(12.0), ORANGE, alpha * 0.8, 1.0);
                    cv.text_c(btn_x, by, bw, s(24.0), "喝一杯", 11.0, W_MED, ORANGE, alpha);
                    push_hit(hits, HitId::WaterOk, btn_x, by, bw, s(24.0));
                    return;
                }
                crate::app::ToastKind::Sedentary => {
                    let msg = crate::app::sed_reminder_msg(app.cfg.health.sedentary_minutes);
                    let bx = x + 22.0;
                    cv.circle(bx, y + ph / 2.0, s(6.0), ORANGE, alpha);
                    cv.text_ex(bx + 12.0, y, pw - 130.0, ph, &msg, 13.0, W_SEMI, TXT, alpha, 0, 1);
                    let bw = s(72.0);
                    let btn_x = x + pw - bw - 16.0;
                    let by = y + ph / 2.0 - s(12.0);
                    cv.rrect(btn_x, by, bw, s(24.0), s(12.0), ROW2, alpha);
                    cv.rrect_stroke(btn_x + 0.5, by + 0.5, bw - 1.0, s(23.0), s(12.0), ORANGE, alpha * 0.8, 1.0);
                    cv.text_c(btn_x, by, bw, s(24.0), "起来走走", 11.0, W_MED, ORANGE, alpha);
                    push_hit(hits, HitId::SedWalk, btn_x, by, bw, s(24.0));
                    return;
                }
                // 日程临近：胶囊常亮显示，点「知道了」或点胶囊本身都能清掉
                crate::app::ToastKind::Schedule => {
                    let msg = if t.msg.is_empty() { "日程临近".to_string() } else { t.msg.clone() };
                    let bx = x + 22.0;
                    cv.circle(bx, y + ph / 2.0, s(6.0), crate::theme::light(), alpha);
                    cv.text_ex(bx + 12.0, y, pw - 140.0, ph, &msg, 13.0, W_SEMI, TXT, alpha, 0, 1);
                    let bw = s(58.0);
                    let btn_x = x + pw - bw - 16.0;
                    let by = y + ph / 2.0 - s(12.0);
                    cv.rrect(btn_x, by, bw, s(24.0), s(12.0), ROW2, alpha);
                    cv.rrect_stroke(btn_x + 0.5, by + 0.5, bw - 1.0, s(23.0), s(12.0), BORDER, alpha * 0.8, 1.0);
                    cv.text_c(btn_x, by, bw, s(24.0), "知道了", 11.0, W_MED, TXT, alpha);
                    push_hit(hits, HitId::ReminderAck, btn_x, by, bw, s(24.0));
                    return;
                }
                // 盯盘涨跌提醒：红色圆点 + 精简文案 + 确认按钮
                crate::app::ToastKind::Stock => {
                    let msg = if t.msg.is_empty() { "盯盘提醒".to_string() } else { t.msg.clone() };
                    let disp = ellipsize(&msg, 13.0, pw - 150.0);
                    let bx = x + 22.0;
                    cv.circle(bx, y + ph / 2.0, s(6.0), RED, alpha);
                    cv.text_ex(bx + 12.0, y, pw - 140.0, ph, &disp, 13.0, W_SEMI, TXT, alpha, 0, 1);
                    let bw = s(58.0);
                    let btn_x = x + pw - bw - 16.0;
                    let by = y + ph / 2.0 - s(12.0);
                    cv.rrect(btn_x, by, bw, s(24.0), s(12.0), ROW2, alpha);
                    cv.rrect_stroke(btn_x + 0.5, by + 0.5, bw - 1.0, s(23.0), s(12.0), RED, alpha * 0.8, 1.0);
                    cv.text_c(btn_x, by, bw, s(24.0), "知道了", 11.0, W_MED, RED, alpha);
                    push_hit(hits, HitId::ReminderAck, btn_x, by, bw, s(24.0));
                    return;
                }
        }
    }

    // 验证码态
    if app.code_pending || app.code_until.map_or(false, |t| Instant_now() < t) {
        {
            if let Ok(shared) = app.mail_shared.lock() {
                if let Some(mc) = &shared.latest {
                    let from = if mc.from.len() > 18 { format!("{}…", &mc.from[..18]) } else { mc.from.clone() };
                    let msg = format!("验证码 {} · {}", mc.code, from);
                    cv.text(x + 20.0, y, pw - 118.0, ph, &msg, 13.0, W_SEMI, TXT, alpha);
                    // 点复制后 5 秒内按钮保持绿底「已复制」，与胶囊同时消失
                    let copied = app
                        .code_copied_at
                        .map_or(false, |t| Instant_now() < t + std::time::Duration::from_secs(5));
                    let bw = s(56.0);
                    let btn_x = x + pw - bw - 16.0;
                    let by = y + ph / 2.0 - s(12.0);
                    let (bg, fg, label) = if copied {
                        (GREEN, 0xFFFFFF, "已复制")
                    } else {
                        (crate::theme::main(), 0xFFFFFF, "复制")
                    };
                    cv.rrect(btn_x, by, bw, s(24.0), s(12.0), bg, alpha);
                    cv.text_c(btn_x, by, bw, s(24.0), label, 11.0, W_SEMI, fg, alpha);
                    push_hit(hits, HitId::MailCopy, btn_x, by, bw, s(24.0));
                    return;
                }
            }
        }
    }

    // 默认态：从左到右三段 —— 动态图标 | 时间 + 上班状态 + 日期 | 打工人信息（上下轮播）
    let pad = s(18.0);
    let mut cx = x + pad;
    let cy = y + ph / 2.0;
    let right = x + pw - s(18.0);
    let now_f = clock::secs_of_day_f(&st);
    let now_min = now_f / 60.0;

    // 右侧段先画，好把剩余宽度让给左侧两段
    let money_left = if app.cfg.modules.salary {
        draw_pill_info(cv, app, right, y, ph, now_f, now_min, alpha)
    } else {
        // 没有薪资模块时右侧显示 WPM
        let wpm = format!("{} WPM", typing::wpm_now());
        let ww = text_w(&wpm, 13.0) * 1.08 + s(4.0);
        cv.text_ex(right - ww, y, ww, ph, &wpm, 13.0, W_SEMI, crate::theme::light(), alpha, 2, 1);
        right - ww
    };

    // 最左段：动态图标。相位永不停止（空闲缓慢呼吸），打字/声音时跳动更明显；
    // 形态自动切换：声音活跃 → 音乐态，最近有按键 → 键盘态，否则默认态。
    // 槽位 34×26：小尺寸走「极光徽章」渲染（icon.rs），内容元素放大约 2 倍；
    // 占宽 34+6=40 与旧版（26+10）持平，不会挤压后面的日期。
    {
        let ww = s(34.0);
        let gap = s(6.0);
        if cx + ww + gap < money_left {
            let sig = crate::modules::icon::Signal {
                energy: app.icon_energy,
                phase: app.icon_phase,
                state: app.icon_state,
                prev: app.icon_state_prev,
                blend: app.icon_blend,
                key_idx: app.icon_key_idx,
                key_age: app.icon_key_age,
            };
            crate::modules::icon::draw_icon(cv, &app.cfg.icon, cx + ww / 2.0, cy, ww, s(26.0), sig, alpha);
            cx += ww + gap;
        }
    }

    // 中间段：日期（下方小字节日）| 上班状态徽标 | 时间
    // 日期在左、时间在右；放不下时逐级退化（整日期 → 11/15 → 省略号）
    // 时间显示到秒：时分秒统一大字同色，读作完整的 HH:MM:SS
    let time_str = clock::hhmmss(&st);
    let time_w = text_w(&time_str, 21.0) + s(3.0);

    // 状态徽标：盯盘模式下固定显示「盯盘中」（与上班/午休状态无关），
    // 否则按法定工作日 + 上午/午休/下午/下班时间判定
    let phs = if app.cfg.modules.salary { Some(workday::phase_now(&st, &app.cfg.salary)) } else { None };
    let chip: Option<(String, u32)> = if app.cfg.stock_mode() {
        Some(("盯盘中".to_string(), crate::theme::main()))
    } else {
        phs.map(|p| (p.label().to_string(), p.color()))
    };
    let chip_tw = chip.as_ref().map(|c| text_w(&c.0, 10.5)).unwrap_or(0.0);
    let chip_w = s(5.0) + s(4.0) + chip_tw + s(9.0);

    let gap = s(7.0);
    // 日期之后还要让出的宽度：间隔 + 状态徽标 + 间隔 + 时间
    let tail = gap + if chip.is_some() { chip_w + gap } else { 0.0 } + time_w;

    if app.cfg.modules.clock {
        // 节日名放在日期下方（小字），与日期共用左边界、不再额外占水平空间
        let fes = workday::display_festival(&st);
        let date_end = money_left - gap - tail; // 日期段最多画到这里
        let avail = (date_end - cx).max(0.0);
        let fes_tw = fes.as_ref().map(|f| text_w(f, 9.5)).unwrap_or(0.0);
        let mut used = 0.0f32;
        if avail > s(26.0) {
            let date_full = clock::date_short(&st);
            let date_min = format!("{}/{}", st.wMonth, st.wDay);
            // 块宽同时容纳日期与更长的节日名，避免下方小字被挤掉
            let need_full = text_w(&date_full, 12.5).max(fes_tw);
            let need_min = text_w(&date_min, 12.5).max(fes_tw);
            let shown = if avail >= need_full {
                date_full
            } else if avail >= need_min {
                date_min
            } else {
                ellipsize(&date_full, 12.5, avail)
            };
            used = text_w(&shown, 12.5).max(fes_tw).min(avail);
            if fes.is_some() {
                // 有节日：日期上移，节日小字在下方，两行整体垂直居中
                cv.text_ex(cx, cy - s(18.0), used, s(18.0), &shown, 12.5, W_MED, TXT2, alpha * 0.95, 0, 1);
            } else {
                cv.text_ex(cx, y, used, ph, &shown, 12.5, W_MED, TXT2, alpha * 0.95, 0, 1);
            }
        }
        if let Some(f) = fes.as_ref() {
            // 节日小字：主题弱化色，紧贴日期正下方
            cv.text_ex(cx, cy + s(1.0), avail, s(14.0), f, 9.5, W_MED, crate::theme::light(), alpha * 0.92, 0, 1);
        }
        if used > 0.0 {
            cx += used + gap;
        }
    }

    // 状态徽标（日期之后）：工作模式为上班阶段，盯盘模式为「盯盘中」
    if let Some((label, col)) = chip.as_ref() {
        if cx + chip_w + gap + time_w <= money_left {
            let col = *col;
            cv.rrect(cx, cy - s(9.0), chip_w, s(18.0), s(9.0), col, alpha * 0.13);
            cv.circle(cx + s(6.5), cy, s(2.8), col, alpha * 0.95);
            cv.text_ex(cx + s(12.0), cy - s(9.5), chip_tw + 2.0, s(19.0), label, 10.5, W_MED, col, alpha, 0, 1);
            cx += chip_w + gap;
        }
    }

    // 时间（整体居中于胶囊）：以胶囊水平中线为基准定位，再夹逼到
    // 「不压住左侧日期/徽标」与「不碰右侧打工人信息」之间；空间不足时贴左内容右侧。
    let min_tx = cx;
    let max_tx = money_left - time_w;
    let time_x = if max_tx >= min_tx {
        (x + (pw - time_w) / 2.0).clamp(min_tx, max_tx)
    } else {
        min_tx
    };
    cv.text_ex(time_x, y, time_w, ph, &time_str, 21.0, W_BOLD, TXT, alpha, 0, 1);
}

/// 胶囊右侧信息的值：
/// - Plain：整段同色（金额）
/// - Segs：每段「数值大字彩色 + 单位小字灰」（时长、字数），让数值与单位一眼可分
/// - Progress：主题色进度条 + 百分比（今日工作进度）
enum PillValue {
    Plain(String, u32),
    Segs(Vec<(String, String)>, u32),
    Progress(f32),
}

/// 进度条宽度（运行时缩放，不能用 const）
fn pill_bar_w() -> f32 {
    s(54.0)
}
/// 时长分段里：数字与单位之间的间距
fn pill_nu_gap() -> f32 {
    s(2.0)
}
/// 时长分段之间（时 / 分 / 秒）的间距
fn pill_seg_gap() -> f32 {
    s(1.5)
}
/// 中文标签与数值之间的间距
fn pill_label_gap() -> f32 {
    s(3.0)
}

impl PillValue {
    /// 值区估算宽度（与绘制保持一致，略偏保守）
    fn width(&self) -> f32 {
        match self {
            PillValue::Plain(v, _) => text_w(v, 13.0) + s(6.0),
            PillValue::Segs(segs, _) => {
                let mut w = 0.0f32;
                for (num, unit) in segs {
                    w += text_w(num, 13.0) + pill_nu_gap() + text_w(unit, 9.5);
                }
                w + pill_seg_gap() * segs.len().saturating_sub(1) as f32
            }
            PillValue::Progress(_) => pill_bar_w() + s(6.0) + text_w("100%", 9.5) + s(4.0),
        }
    }
}

/// 胶囊右侧「打工人信息」轮播条目：(标签, 值)
/// 时长精确到秒、每秒都在变；数值与单位分色分字号，进度用条展示。
/// 胶囊右侧轮播条目（按当前模式选择内容）：
/// - 工作模式：今日已赚 / 已工作 / 进度 / 距午休-下班 / 打字
/// - 盯盘模式：每只自选的「股价 + 涨跌幅」，有持仓再追加「今日盈亏」
fn pill_items(app: &App, now_f: f64, now_min: f64) -> Vec<(String, PillValue)> {
    if app.cfg.stock_mode() {
        pill_stock_items()
    } else {
        pill_work_items(app, now_f, now_min)
    }
}

/// 盯盘模式条目：每只自选一条「名称｜股价 涨跌幅」，红涨绿跌。
/// 有持仓时追加一条今日盈亏（`Σ(现价-昨收)×数量`）。
fn pill_stock_items() -> Vec<(String, PillValue)> {
    let items = crate::modules::stock::read_items();
    if items.is_empty() {
        return vec![
            ("盯盘助手".to_string(), PillValue::Plain("暂无自选".to_string(), TXT3)),
        ];
    }
    let quotes = crate::modules::stock::snapshot();
    let mut out: Vec<(String, PillValue)> = Vec::with_capacity(items.len() + 1);
    for it in &items {
        let q = crate::modules::stock::quote_for(&quotes, &it.code);
        // 名称优先取自选文件里的（已由行情补全），空则回落快照名称、再不行用代码
        let name = if !it.name.is_empty() {
            it.name.clone()
        } else {
            q.map(|x| x.name.clone()).unwrap_or_else(|| it.code.clone())
        };
        // 名称过长截断：右侧块宽有限（盯盘模式上限更紧，要给「盯盘中」徽标让位），
        // 长名称会把股价挤出可视区，这里统一收到 4 字
        let name = if name.chars().count() > 4 {
            format!("{}…", name.chars().take(4).collect::<String>())
        } else {
            name
        };
        let (text, col) = match q {
            Some(x) if x.price > 0.0 => {
                (format!("{:.2} {:+.2}%", x.price, x.chg_pct), color_pct(x.chg_pct))
            }
            // 行情还没到（刚启动 / 收盘未拉到）时给占位，避免显示 0.00 误导
            _ => ("--".to_string(), TXT3),
        };
        out.push((name, PillValue::Plain(text, col)));
    }
    if items.iter().any(|i| i.qty > 0.0) {
        let p = crate::modules::stock::today_pnl(&quotes, &items);
        out.push((
            "今日盈亏".to_string(),
            PillValue::Plain(format!("¥{:+.2}", p), pnl_color(p)),
        ));
    }
    out
}

/// 工作模式条目：今日工作相关（已赚 / 已工作 / 进度 / 距午休-下班 / 打字）
fn pill_work_items(app: &App, now_f: f64, now_min: f64) -> Vec<(String, PillValue)> {
    let light = crate::theme::light();
    vec![
        (
            "今天已经赚了".to_string(),
            PillValue::Plain(format!("¥{}", salary::fmt_money(app.sal.earned_today_f(now_f))), GREEN),
        ),
        (
            "今天已经工作了".to_string(),
            PillValue::Segs(salary::dur_segs(app.sal.worked_secs_excl_lunch(now_f)), light),
        ),
        (
            "今日工作进度".to_string(),
            PillValue::Progress(app.sal.progress(now_min)),
        ),
        {
            let (lbl, secs) = app.sal.next_break_secs(now_f);
            (lbl.to_string(), PillValue::Segs(salary::dur_segs(secs), light))
        },
        (
            "今天已经输入了".to_string(),
            PillValue::Segs(vec![(fmt_count(app.store.typing_keys), "字".to_string())], light),
        ),
    ]
}

/// 打字字数：过万折成「1.2万」，避免撑破胶囊
fn fmt_count(n: u64) -> String {
    if n >= 10_000 {
        format!("{:.1}万", n as f64 / 10_000.0)
    } else {
        n.to_string()
    }
}

/// 绘制右侧信息区并返回其左边界。上下轮播：旧条目上滑淡出、新条目自下滑入，
/// 与面板翻页同一套弹簧手感（起手带初速、收尾自然减速）。
fn draw_pill_info(cv: &mut Canvas, app: &App, right: f32, y: f32, ph: f32, now_f: f64, now_min: f64, alpha: f32) -> f32 {
    let items = pill_items(app, now_f, now_min);
    // 固定块宽 = 最长条目宽度：轮播时左边界不动，日期宽度才不会忽宽忽窄。
    // 各类值的宽度由 PillValue::width() 统一估算，右对齐绘制天然兼容偏差
    let mut block = s(74.0);
    for (label, val) in items.iter() {
        // 值区宽 + 标签间距 + 标签实宽 + 余量；不要高估，否则会挤掉左侧日期
        block = block.max(val.width() + pill_label_gap() + text_w(label, 10.5) + s(4.0));
    }
    // 上限按模式收紧：盯盘模式左侧多了「盯盘中」三字徽标（比两字的上班徽标宽约 10px），
    // 右侧块给得太宽会把徽标挤掉（徽标放不下时整块跳过不画）。126 保证徽标 + 时间都留得下。
    let cap = if app.cfg.stock_mode() { s(126.0) } else { s(178.0) };
    let block = block.min(cap);
    let p = app.anims.rot.v.clamp(0.0, 1.0);

    // 裁剪区向右多留 10px：数值右对齐到 right+6（吃进胶囊内边距一部分），
    // 避免最后一个字符贴着裁剪边界被削掉半截
    cv.push_clip(right - block - s(2.0), y, block + s(12.0), ph);
    let cur = &items[app.rot_i % items.len()];
    draw_info_line(cv, cur, right, y + ph - p * ph, ph, alpha);
    if p < 0.999 {
        let prev = &items[app.rot_prev % items.len()];
        draw_info_line(cv, prev, right, y - p * ph, ph, alpha * (1.0 - p * 0.6));
    }
    cv.pop_clip();
    right - block
}

/// 单条信息：先画值、得到其真实左缘，再把标签右对齐到「左缘 - 固定间距」，
/// 确保中文标签与数值之间始终留白、绝不贴一起。
/// Plain 整段同色；Segs 数值大字彩色 + 单位小字灰；Progress 主题色进度条 + 小字百分比。
fn draw_info_line(cv: &mut Canvas, item: &(String, PillValue), right: f32, y0: f32, ph: f32, alpha: f32) {
    let (label, val) = item;
    let vr = right + s(6.0); // 值区右缘（略微伸进内边距，避开裁剪边界）

    // 画值区，返回它的真实左缘 x（标签据此定位，间距不依赖估算）
    let value_left = match val {
        PillValue::Plain(value, col) => {
            let mut size = 13.0f32;
            while size > 9.0 && text_w(value, size) * 1.25 > s(126.0) {
                size -= 0.5;
            }
            // 用真实文字宽（text_ex 右对齐、盒略加宽），不要拿含 1.25 余量的盒宽当左缘，
            // 否则标签会被推远一大截。金额那样 ASCII 为主的串，text_w 已足够接近真实宽度。
            let tw = text_w(value, size);
            cv.text_ex(vr - tw - s(2.0), y0, tw + s(6.0), ph, value, size, W_SEMI, *col, alpha, 2, 1);
            vr - tw - s(2.0)
        }
        PillValue::Segs(segs, col) => {
            // 从右往左依次画：单位(小字灰) → 数值(大字彩色)
            // 用 text1（不折行）：分段盒宽很窄，text_ex 会误把数字折成两行
            let mut x = vr;
            for (num, unit) in segs.iter().rev() {
                let uw = text_w(unit, 9.5);
                x -= uw;
                cv.text1(x, y0, uw + s(2.0), ph, unit, 9.5, W_MED, UNIT, alpha);
                let nw = text_w(num, 13.0);
                x -= pill_nu_gap() + nw;
                cv.text1(x, y0, nw + s(2.0), ph, num, 13.0, W_SEMI, *col, alpha);
                x -= pill_seg_gap();
            }
            x + pill_seg_gap() // 撤销最后一段多减的段间距
        }
        PillValue::Progress(prog) => {
            let pct = format!("{}%", (prog.clamp(0.0, 1.0) * 100.0).round() as i32);
            let pw = text_w(&pct, 9.5);
            let bw = pill_bar_w();
            // 百分比（小字灰，右缘贴齐；text1 不折行）
            cv.text1(vr - pw - s(2.0), y0, pw + s(4.0), ph, &pct, 9.5, W_MED, TXT2, alpha);
            // 进度条：灰色底槽 + 主题色填充
            let bx = vr - pw - s(8.0) - bw;
            let by = y0 + ph / 2.0 - s(3.5);
            let bh = s(7.0);
            cv.rrect(bx, by, bw, bh, s(3.5), TXT3, alpha * 0.30);
            let fw = (bw * prog.clamp(0.0, 1.0)).max(bh).min(bw);
            cv.rrect(bx, by, fw, bh, s(3.5), crate::theme::main(), alpha);
            bx
        }
    };

    // 标签右对齐到「值区真实左缘 - 固定间距」，保证留白、不重叠
    let lw = text_w(label, 10.5) * 1.15 + s(4.0);
    let lx = value_left - pill_label_gap() - lw;
    cv.text_ex(lx, y0, lw + s(2.0), ph, label, 10.5, W_NORMAL, TXT2, alpha, 2, 1);
}

use std::time::Instant;
fn Instant_now() -> Instant {
    Instant::now()
}

/// 胶囊当前内容模式（0 默认 / 1 喝水提醒 / 2 久坐提醒 / 3 邮箱验证码 / 4 日程提醒 / 5 拖文件提示）
/// 模式切换时触发淡入，避免文字硬替换
pub fn pill_mode_key(app: &App) -> u32 {
    if let Some(t) = &app.reminder {
        return match t.kind {
            crate::app::ToastKind::Water => 1,
            crate::app::ToastKind::Sedentary => 2,
            crate::app::ToastKind::Schedule => 4,
            crate::app::ToastKind::Stock => 6,
        };
    }
    if app.code_pending || app.code_until.map_or(false, |t| Instant_now() < t) {
        return 3;
    }
    0
}

// ---------------- 面板（点击整块翻页 · 多页轮播）----------------

const HEADER_H: f32 = 48.0;
const BODY_TOP: f32 = 76.0;
/// 待办页「＋ 新增待办」按钮区高度（固定在标题下，列表从其下方滚动）
const TODO_ADD_H: f32 = 32.0;
/// 盯盘页标题右侧「＋ 编辑」小按钮高度（与标题同一水平行）
const WATCH_BTN_H: f32 = 18.0;
/// 盯盘页盈亏卡高度（压扁，把主视觉让给行情列表）
const WATCH_CARD_H: f32 = 44.0;
/// 盯盘页行情行高（放大以便看清）
const WATCH_ROW_H: f32 = 34.0;

/// 依据配置推导实际参与轮播的页面（顺序即展示顺序；模块关闭时自动跳过）
pub fn effective_pages(cfg: &crate::config::Config) -> Vec<&'static str> {
    let m = &cfg.modules;
    let mut out: Vec<&'static str> = Vec::new();
    for p in cfg.panel.pages.iter() {
        if !p.on {
            continue;
        }
        let id = match crate::config::PANEL_KINDS.iter().find(|(k, _)| *k == p.id) {
            Some((k, _)) => *k,
            None => continue,
        };
        if m.page_on(id) {
            out.push(id);
        }
    }
    if out.is_empty() {
        out.push("overview"); // 兜底：全部关掉也至少有一页
    }
    out
}

fn draw_panel(app: &App, cv: &mut Canvas, w: f32, h: f32, o: f32, bg_a: f32, ct: f32, dy: f32, hits: &mut Vec<Hit>) -> f32 {
    // 圆角随展开在「胶囊半圆 → 面板圆角」之间连续过渡，避免变形时出现拐角
    let r = s(PILL_H as f32) * 0.5 * (1.0 - o) + s(24.0) * o;
    cv.rrect(0.0, 0.0, w, h, r, BG, bg_a);
    if bg_a > 0.35 {
        cv.rrect_stroke(0.5, 0.5, w - 1.0, h - 1.0, r, BORDER, bg_a * 0.9, 1.0);
    }
    if ct < 0.02 {
        return 0.0;
    }

    let pad = s(18.0);
    let header_h = s(HEADER_H);
    let st = clock::local_time();

    // 内容整体上浮
    cv.offset(0.0, dy);

    // 头部：有提醒时整条换成提醒条（必须点确认才消失）；否则是日期 + 时间 + 收起
    if let Some(r) = &app.reminder {
        draw_reminder_bar(app, cv, w, header_h, ct, hits, r);
        cv.line(pad, header_h + s(2.0), w - pad, header_h + s(2.0), BORDER, ct * 0.6, s(1.0));
    } else {
        let date_s = clock::date_cn(&st);
        cv.text_ex(pad, 0.0, s(190.0), header_h, &date_s, 13.0, W_MED, TXT2, ct, 0, 1);
        let dw = text_w(&date_s, 13.0);
        cv.text_ex(pad + dw + s(10.0), 0.0, s(96.0), header_h, &clock::hhmmss(&st), 18.0, W_BOLD, TXT, ct, 0, 1);

        let bw = s(28.0);
        let bx = w - bw - pad;
        let by = header_h / 2.0 - bw / 2.0;
        let chp = app.anims.hover_p(&HitId::Pin);
        let cx = bx + bw / 2.0;
        let cy = by + bw / 2.0;
        // 图钉：锁定后用主题色实心圆，未锁定为描边；下方针脚 + 横线表示钉住
        let pin_col = if app.pinned { crate::theme::main() } else { mix(TXT2, TXT, chp) };
        let bg = if app.pinned { mix(ROW, crate::theme::main(), 0.28) } else { mix(ROW, 0x33333E, chp) };
        cv.circle(cx, cy, bw / 2.0, bg, ct);
        if app.pinned {
            cv.circle(cx, cy - s(3.5), s(3.6), pin_col, ct);
            cv.line(cx, cy - s(0.5), cx, cy + s(6.0), pin_col, ct, s(1.4));
            cv.line(cx - s(4.5), cy + s(6.0), cx + s(4.5), cy + s(6.0), pin_col, ct, s(1.4));
        } else {
            cv.circle_stroke(cx, cy - s(3.5), s(3.6), pin_col, ct, s(1.2));
            cv.line(cx, cy - s(0.5), cx, cy + s(6.0), pin_col, ct, s(1.2));
            cv.line(cx - s(4.5), cy + s(6.0), cx + s(4.5), cy + s(6.0), pin_col, ct, s(1.2));
        }
        push_hit(hits, HitId::Pin, bx - s(4.0), by - s(4.0), bw + s(8.0), bw + s(8.0));

        // 头部其余区域（时间/日期）也可点击收起 —— 和胶囊同一位置，
        // 展开后点原处就能收回
        push_hit(hits, HitId::Collapse, pad - s(6.0), 0.0, w - pad * 2.0 - bw, header_h);

        cv.line(pad, header_h + s(2.0), w - pad, header_h + s(2.0), BORDER, ct * 0.6, s(1.0));
    }

    // ---- 页面：水平滑动 + 裁剪（旧页滑出、新页跟进，等同一次物理翻页）----
    let pages = effective_pages(&app.cfg);
    let idx = app.page.min(pages.len().saturating_sub(1));
    let p = app.anims.page.v.clamp(0.0, 1.0);
    let from = app.anims.page_from.min(pages.len().saturating_sub(1));
    let cur_kind = pages.get(idx).copied().unwrap_or("overview");
    let clip_top = s(48.0);
    let clip_h = (h - s(44.0) - clip_top).max(1.0);

    if p < 0.999 && from != idx {
        let d = app.anims.page_dir;
        cv.push_clip(0.0, clip_top, w, clip_h);
        cv.offset(-d * p * w, dy);
        draw_page(app, cv, pages.get(from).copied().unwrap_or("overview"), w, ct, &mut Vec::new());
        cv.offset(d * (1.0 - p) * w, dy);
        draw_page(app, cv, cur_kind, w, ct, &mut Vec::new());
        cv.pop_clip();
    } else {
        cv.push_clip(0.0, clip_top, w, clip_h);
        draw_page(app, cv, cur_kind, w, ct, hits);
        // 可滚动页：右侧细滚动条，位置随内容滚动量
        let max = panel_scroll_max(app, cur_kind);
        if max > 0.0 {
            let vh = clip_h;
            let content = vh + max;
            let th = (vh * vh / content).max(s(24.0));
            let ty = clip_top + (app.panel_scroll / max) * (vh - th);
            cv.rrect(w - s(5.0), ty, s(3.0), th, s(1.5), TXT3, ct * 0.4);
        }
        cv.pop_clip();
    }

    // 底部翻页条：不参与滑动
    cv.offset(0.0, dy);
    draw_pager(app, cv, w, h, idx, &pages, ct, hits);
    cv.offset(0.0, 0.0);
    0.0
}

/// 面板顶部提醒条（久坐 / 喝水 / 日程）：淡琥珀底 + 圆点 + 文案 + 确认按钮。
/// 整条和按钮都可点确认；提醒不会自动消失，也没有倒计时。
fn draw_reminder_bar(
    app: &App,
    cv: &mut Canvas,
    w: f32,
    header_h: f32,
    alpha: f32,
    hits: &mut Vec<Hit>,
    r: &crate::app::Toast,
) {
    use crate::app::ToastKind;
    let pad = s(18.0);
    let x = pad - s(4.0);
    let bar_w = w - x * 2.0;
    let bar_y = s(7.0);
    let bar_h = header_h - s(10.0);
    cv.rrect(x, bar_y, bar_w, bar_h, s(12.0), ORANGE, alpha * 0.14);
    cv.rrect_stroke(x + 0.5, bar_y + 0.5, bar_w - 1.0, bar_h - 1.0, s(12.0), ORANGE, alpha * 0.55, s(1.0));

    let msg = match r.kind {
        ToastKind::Water => {
            let cups = app.store.water_cups;
            let goal = app.cfg.health.water_goal_cups.max(1);
            format!("该喝水了 · 今日 {}/{} 杯", cups.min(goal), goal)
        }
        ToastKind::Sedentary => {
            crate::app::sed_reminder_msg(app.cfg.health.sedentary_minutes)
        }
        ToastKind::Schedule => {
            if r.msg.is_empty() {
                "日程临近".to_string()
            } else {
                r.msg.clone()
            }
        }
        ToastKind::Stock => {
            if r.msg.is_empty() {
                "盯盘提醒".to_string()
            } else {
                r.msg.clone()
            }
        }
    };

    let bw = s(78.0);
    let bh = s(26.0);
    let bx = w - pad + s(4.0) - bw;
    let by = header_h / 2.0 - bh / 2.0;

    cv.circle(x + s(14.0), header_h / 2.0, s(4.0), ORANGE, alpha);
    let tx = x + s(24.0);
    let tw = (bx - s(10.0) - tx).max(s(24.0));
    cv.text_ex(tx, 0.0, tw, header_h, &ellipsize(&msg, 12.0, tw), 12.0, W_SEMI, TXT, alpha, 0, 1);

    let hp = app.anims.hover_p(&HitId::ReminderAck);
    cv.rrect(bx, by, bw, bh, bh / 2.0, ORANGE, alpha * (0.20 + 0.25 * hp));
    cv.rrect_stroke(bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0, bh / 2.0, ORANGE, alpha * 0.9, s(1.0));
    cv.text_c(bx, by, bw, bh, r.kind.ack_label(), 11.5, W_SEMI, TXT, alpha);
    push_hit(hits, HitId::ReminderAck, bx, by, bw, bh);
    // 整条也可点确认（放在按钮之后，命中仍按最上层判定）
    push_hit(hits, HitId::ReminderAck, x, bar_y, bar_w, bar_h);
}

fn draw_page(app: &App, cv: &mut Canvas, kind: &str, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    match kind {
        "schedule" => page_schedule(app, cv, w, alpha),
        "todo" => page_todo(app, cv, w, alpha, hits),
        "files" => page_files(app, cv, w, alpha, hits),
        "clips" => page_clips(app, cv, w, alpha, hits),
        "health" => page_health(app, cv, w, alpha, hits),
        "salary" => page_salary(app, cv, w, alpha),
        "watch" => page_watch(app, cv, w, alpha, hits),
        _ => page_overview(app, cv, w, alpha, hits),
    }
}

// ---- 小工具 ----

fn ellipsize(txt: &str, size: f32, maxw: f32) -> String {
    if text_w(txt, size) <= maxw {
        return txt.to_string();
    }
    let mut out = String::new();
    for ch in txt.chars() {
        out.push(ch);
        if text_w(&out, size) + text_w("…", size) > maxw {
            out.pop();
            break;
        }
    }
    out.push('…');
    out
}

/// 页标题行；右上角优先显示操作反馈（绿，带淡入淡出），否则显示统计信息（灰）
fn draw_label(app: &App, cv: &mut Canvas, w: f32, title: &str, right: Option<String>, alpha: f32) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    cv.text1(pad, s(53.0), iw, s(18.0), title, 9.5, W_SEMI, TXT3, alpha);

    if let Some(f) = app.flash.as_ref().filter(|f| f.alive()) {
        let fa = f.alpha();
        // 反馈文字从下方 3px 浮上来
        let yy = s(53.0) + (1.0 - fa) * s(3.0);
        cv.text_ex(pad, yy, iw, s(18.0), &f.msg, 9.5, W_MED, GREEN, alpha * fa, 2, 1);
        return;
    }
    if let Some(r) = right {
        cv.text_ex(pad, s(53.0), iw, s(18.0), &r, 9.5, W_MED, TXT3, alpha, 2, 1);
    }
}

fn empty_hint(cv: &mut Canvas, w: f32, txt: &str, alpha: f32) {
    let pad = s(18.0);
    cv.text_ex(pad, s(BODY_TOP), w - pad * 2.0, s(26.0), txt, 10.5, W_NORMAL, TXT3, alpha, 1, 1);
}

fn mini_stat(cv: &mut Canvas, x: f32, y: f32, w: f32, label: &str, value: &str, color: u32, alpha: f32) {
    cv.text1(x, y, w, s(12.0), label, 8.5, W_NORMAL, TXT3, alpha);
    cv.text1(x, y + s(11.0), w, s(18.0), value, 11.5, W_SEMI, color, alpha);
}

/// 行悬停：柔和高亮随悬停进度淡入淡出，返回当前进度
fn row_hover(app: &App, cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, id: &HitId, alpha: f32) -> f32 {
    let p = app.anims.hover_p(id);
    if p > 0.004 {
        cv.rrect(x, y, w, h, s(9.0), 0x2C2C36, alpha * 0.9 * p);
    }
    p
}

// ---- 0 概览 ----

fn page_overview(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let st = clock::local_time();
    let now_min = clock::secs_of_day(&st) / 60;

    let merged = if app.cfg.modules.schedule { app.merged_schedule() } else { Vec::new() };
    let sched_n = merged.len();
    let undone = if app.cfg.modules.todo { app.store.todos.iter().filter(|t| !t.done).count() } else { 0 };
    draw_label(app, cv, w, crate::config::panel_name("overview"), Some(format!("{} 项", sched_n + undone)), alpha);

    let row_h = s(26.0);
    let mut y = s(BODY_TOP);
    let mut shown = 0usize;

    if !merged.is_empty() {
        for it in merged.iter().take(2) {
            if shown >= 3 {
                break;
            }
            let mins = clock::parse_hm(&it.time).unwrap_or(0);
            let past = mins < now_min;
            cv.text_ex(pad, y, s(44.0), row_h, &it.time, 11.0, W_SEMI, if past { TXT3 } else { crate::theme::light() }, alpha, 0, 1);
            cv.text1(pad + s(50.0), y, iw - s(50.0), row_h, &it.title, 11.5, W_NORMAL, if past { TXT2 } else { TXT }, alpha);
            y += row_h;
            shown += 1;
        }
    }
    if app.cfg.modules.todo {
        for (idx, it) in app.store.todos.iter().enumerate() {
            if shown >= 3 {
                break;
            }
            if it.done {
                continue;
            }
            let r = s(6.0) * (1.0 + 0.10 * app.anims.hover_p(&HitId::TodoToggle(idx)));
            let cxx = pad + r + s(2.0);
            let cyy = y + row_h / 2.0;
            row_hover(app, cv, pad - s(6.0), y + s(1.0), iw + s(12.0), row_h - s(2.0), &HitId::TodoToggle(idx), alpha);
            cv.circle(cxx, cyy, r, ROW2, alpha);
            cv.circle_stroke(cxx, cyy, r, TXT3, alpha * 0.8, s(1.0));
            cv.text1(pad + s(20.0), y, iw - s(20.0), row_h, &it.title, 11.5, W_NORMAL, TXT, alpha);
            push_hit(hits, HitId::TodoToggle(idx), pad, y, iw, row_h);
            y += row_h;
            shown += 1;
        }
    }
    if shown == 0 {
        cv.text1(pad, y, iw, row_h, "今天没有日程和待办", 11.0, W_NORMAL, TXT3, alpha);
    }

    // 底部状态条：喝水 | 久坐 | 今日已赚
    let sy = s(160.0);
    let sh = s(32.0);
    cv.rrect(pad, sy, iw, sh, s(10.0), ROW, alpha);
    let gap = s(10.0);
    let cw = (iw - gap * 2.0) / 3.0;

    let goal = app.cfg.health.water_goal_cups.max(1);
    let cups = app.store.water_cups;
    let idle_min = crate::app::now_sed_secs(app.last_sed_unix) / 60;
    let sal = app.sal.earned_today_f(clock::secs_of_day_f(&st));
    let money = format!("¥{}", crate::modules::salary::fmt_money(sal));

    for i in 1..3 {
        let dx = pad + s(14.0) + cw * i as f32;
        cv.line(dx, sy + s(8.0), dx, sy + sh - s(8.0), BORDER, alpha * 0.9, s(1.0));
    }
    mini_stat(cv, pad + s(14.0), sy + s(4.0), cw - s(12.0), "喝水", &format!("{}/{} 杯", cups.min(goal), goal), crate::theme::light(), alpha);
    mini_stat(cv, pad + s(14.0) + cw, sy + s(4.0), cw - s(12.0), "久坐", &format!("{} 分钟", idle_min), if idle_min >= app.cfg.health.sedentary_minutes { ORANGE } else { TXT2 }, alpha);
    mini_stat(cv, pad + s(14.0) + cw * 2.0, sy + s(4.0), cw - s(12.0), "今日已赚", &money, GREEN, alpha);
}

// ---- 1 日程 ----

fn page_schedule(app: &App, cv: &mut Canvas, w: f32, alpha: f32) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let now_min = clock::secs_of_day(&clock::local_time()) / 60;
    // 本地日程 + ICS 订阅日程合并展示
    let merged = app.merged_schedule();
    let total = merged.len();
    // 日程页只做展示：注册 0 个命中区，点这里就是普通的翻页
    draw_label(app, cv, w, crate::config::panel_name("schedule"), None, alpha);

    if total == 0 {
        empty_hint(cv, w, "还没有日程 · 在设置里订阅外部日历（ICS 或企业微信 CalDAV）", alpha);
        return;
    }
    let row_h = s(27.0);
    let sc = app.panel_scroll.min(panel_scroll_max(app, "schedule"));
    let mut y = s(BODY_TOP) - sc;
    for it in merged.iter() {
        let mins = clock::parse_hm(&it.time).unwrap_or(-1);
        let is_past = mins >= 0 && mins < now_min;
        let is_next = !is_past && mins >= now_min;
        let tw = s(38.0);
        cv.rrect(pad, y + s(4.0), tw, row_h - s(8.0), s(8.0), if is_past { ROW } else { crate::theme::dark() }, alpha);
        cv.text_c(pad, y + s(4.0), tw, row_h - s(8.0), &it.time, 9.5, W_SEMI, if is_past { TXT3 } else { crate::theme::light() }, alpha);
        cv.text1(
            pad + tw + s(10.0),
            y,
            iw - tw - s(10.0),
            row_h,
            &it.title,
            11.5,
            if is_next { W_MED } else { W_NORMAL },
            if is_past { TXT2 } else { TXT },
            alpha,
        );
        y += row_h;
    }
}

// ---- 2 待办 ----

fn page_todo(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let total = app.store.todos.len();
    let done = app.store.todos.iter().filter(|t| t.done).count();
    draw_label(app, cv, w, crate::config::panel_name("todo"), Some(format!("{}/{} 完成", done, total)), alpha);

    // ── 「＋ 新增待办」固定按钮（置于标题下，始终可见）──
    let ay = s(BODY_TOP);
    let ah = s(TODO_ADD_H);
    let ahp = app.anims.hover_p(&HitId::TodoAdd);
    cv.rrect(pad, ay, iw, ah, s(9.0), mix(0x1E1E25, crate::theme::main(), ahp * 0.5), alpha * (0.85 + 0.15 * ahp));
    cv.rrect_stroke(pad, ay, iw, ah, s(9.0), crate::theme::main(), alpha * (0.5 + 0.5 * ahp), s(1.0));
    cv.text1(pad, ay, iw, ah, "＋ 新增待办", 12.0, W_SEMI, mix(TXT2, crate::theme::tint(), ahp), alpha);
    push_hit(hits, HitId::TodoAdd, pad, ay, iw, ah);

    if total == 0 {
        // 空状态引导：放在「＋ 新增待办」按钮下方，避免盖住按钮本身
        let hy = s(BODY_TOP) + s(TODO_ADD_H) + s(6.0);
        cv.text_ex(pad, hy, iw, s(22.0), "暂无待办，点上方「＋ 新增待办」添加第一条", 10.0, W_NORMAL, TXT3, alpha, 1, 1);
        return;
    }
    let row_h = s(27.0);
    let sc = app.panel_scroll.min(panel_scroll_max(app, "todo"));
    let list_top = s(BODY_TOP) + s(TODO_ADD_H) + s(6.0);
    let mut y = list_top - sc;
    let r = s(6.5);

    // 列表独立裁剪：滚动时行不会画到上方「＋ 新增待办」按钮上（此前的文字遮挡）
    cv.push_clip(0.0, list_top - s(2.0), w, (body_clip_bottom() - list_top + s(2.0)).max(0.0));

    // 完成/未完成
    for want_done in [false, true] {
        for (idx, it) in app.store.todos.iter().enumerate() {
            if it.done != want_done {
                continue;
            }
            let cxx = pad + r + s(1.0);
            let cyy = y + row_h / 2.0;
            let hp = row_hover(app, cv, pad - s(6.0), y + s(1.0), iw + s(12.0), row_h - s(2.0), &HitId::TodoToggle(idx), alpha);
            let rr = r * (1.0 + 0.10 * hp);
            if it.done {
                cv.circle(cxx, cyy, rr, GREEN, alpha * 0.9);
                cv.line(cxx - s(3.0), cyy, cxx - s(0.5), cyy + s(2.6), 0x0F1B12, alpha, s(1.6));
                cv.line(cxx - s(0.5), cyy + s(2.6), cxx + s(3.4), cyy - s(2.6), 0x0F1B12, alpha, s(1.6));
            } else {
                cv.circle(cxx, cyy, rr, ROW2, alpha);
                cv.circle_stroke(cxx, cyy, rr, mix(TXT3, crate::theme::light(), hp), alpha * 0.85, s(1.0));
            }
            // 该行是否处于「待二次确认删除」态
            let armed = app.todo_del_arm == Some(idx);
            // 标题（右侧预留删除按钮空间；确认态下多留出「确认删除」文字宽度）
            let reserve = if armed { s(78.0) } else { s(34.0) };
            cv.text1(pad + s(20.0), y, iw - reserve, row_h, &it.title, 11.5, W_NORMAL, if it.done { TXT3 } else { TXT }, alpha);

            if armed {
                // 二次确认条：整行右侧浮出红色「确认删除」胶囊，必须再点一次才真删
                let by = y + s(2.5);
                let bh = row_h - s(5.0);
                let bw = s(64.0);
                let bx = pad + iw - bw;
                cv.rrect(bx, by, bw, bh, s(6.0), 0xE2453E, alpha * 0.95);
                cv.text_ex(bx, by, bw, bh, "确认删除", 10.0, W_SEMI, 0xFFFFFF, alpha, 1, 1);
            } else {
                // 行尾「×」删除：只在悬停该行时显示（柔和高亮随悬停淡入淡出）
                let del_x = pad + iw - s(24.0);
                let dhp = app.anims.hover_p(&HitId::TodoDel(idx));
                let dvisible = dhp.max(hp);
                if dvisible > 0.004 {
                    let dcx = del_x + s(8.0);
                    let dcy = y + row_h / 2.0;
                    cv.circle(dcx, dcy, s(8.5), mix(0x2C2C36, 0xFF5A5A, dhp), alpha * 0.95 * dvisible);
                    cv.line(dcx - s(3.2), dcy, dcx + s(3.2), dcy, 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
                    cv.line(dcx - s(3.2), dcy - s(3.2), dcx + s(3.2), dcy + s(3.2), 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
                    cv.line(dcx - s(3.2), dcy + s(3.2), dcx + s(3.2), dcy - s(3.2), 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
                }
            }
            // 命中区：整行切换完成；行尾删除（确认态下命中区扩展到「确认删除」胶囊）
            push_hit_body_from(hits, HitId::TodoToggle(idx), pad, y, iw, row_h, list_top);
            let del_w = if armed { s(64.0) } else { s(24.0) };
            let del_x0 = pad + iw - del_w;
            push_hit_body_from(hits, HitId::TodoDel(idx), del_x0, y, del_w, row_h, list_top);
            y += row_h;
        }
    }
    cv.pop_clip();
}

// ---- 3 文件 ----

fn page_files(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let dragging = app.drag_hover;
    let cap = app.cfg.filehub_cap;
    // 上限被调小后现有条目可能暂时超限，显示实际条数 / cap（如 12/5），不隐藏真实数量
    draw_label(
        app,
        cv,
        w,
        crate::config::panel_name("files"),
        Some(format!("{}/{} 个", app.store.files.len(), cap)),
        alpha,
    );

    // 拖动文件悬停时，页面顶部浮出一条醒目的投放引导
    if dragging {
        let hy = s(BODY_TOP) - s(2.0);
        let hh = s(24.0);
        cv.rrect(pad, hy, iw, hh, s(8.0), mix(0x1E1E25, crate::theme::main(), 0.28), alpha);
        cv.rrect_stroke(pad, hy, iw, hh, s(8.0), crate::theme::main(), alpha * 0.9, s(1.4));
        cv.text_c(pad, hy, iw, hh, "松开鼠标，暂存到中转站", 11.0, W_SEMI, crate::theme::tint(), alpha);
    }
    let list_top = if dragging { s(BODY_TOP) + s(24.0) + s(2.0) } else { s(BODY_TOP) };

    if app.store.files.is_empty() {
        let hy = list_top + s(6.0);
        cv.text_ex(pad, hy, iw, s(24.0), "把文件拖到灵动岛上即可暂存", 10.5, W_NORMAL, TXT3, alpha, 1, 1);
        return;
    }
    let row_h = s(27.0);
    let sc = app.panel_scroll.min(panel_scroll_max(app, "files"));
    let mut y = list_top - sc;

    cv.push_clip(0.0, list_top - s(2.0), w, (body_clip_bottom() - list_top + s(2.0)).max(0.0));
    for (i, f) in app.store.files.iter().enumerate() {
        let armed = app.file_del_arm == Some(i);
        let sz = crate::modules::files::fmt_size(f.size);
        let szw = text_w(&sz, 9.5);
        // 右侧给删除按钮/确认胶囊预留空间
        let del_reserve = if armed { s(78.0) } else { s(28.0) };
        let name_w = iw - szw - del_reserve - s(10.0);
        let hp = row_hover(app, cv, pad - s(6.0), y + s(1.0), iw + s(12.0), row_h - s(2.0), &HitId::FileOpen(i), alpha);
        cv.rrect(pad, y + s(7.0), s(12.0), row_h - s(14.0), s(3.0), crate::theme::main(), alpha * (0.22 + 0.68 * hp));
        cv.text1(pad + s(20.0), y, name_w, row_h, &ellipsize(&f.name, 11.5, name_w), 11.5, W_NORMAL, mix(TXT, 0xFFFFFF, hp * 0.6), alpha);
        // 大小文字右移到删除按钮左侧
        cv.text_ex(pad + s(20.0) + name_w, y, szw + s(4.0), row_h, &sz, 9.5, W_NORMAL, mix(TXT3, TXT2, hp), alpha, 2, 1);

        if armed {
            // 二次确认：红色「确认删除」胶囊
            let by = y + s(2.5);
            let bh = row_h - s(5.0);
            let bw = s(64.0);
            let bx = pad + iw - bw;
            cv.rrect(bx, by, bw, bh, s(6.0), 0xE2453E, alpha * 0.95);
            cv.text_ex(bx, by, bw, bh, "确认删除", 10.0, W_SEMI, 0xFFFFFF, alpha, 1, 1);
        } else {
            // 行尾「×」删除：悬停该行时淡入
            let del_cx = pad + iw - s(10.0);
            let dhp = app.anims.hover_p(&HitId::FileDel(i));
            let dvisible = dhp.max(hp);
            if dvisible > 0.004 {
                let dcy = y + row_h / 2.0;
                cv.circle(del_cx, dcy, s(8.5), mix(0x2C2C36, 0xFF5A5A, dhp), alpha * 0.95 * dvisible);
                cv.line(del_cx - s(3.2), dcy, del_cx + s(3.2), dcy, 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
                cv.line(del_cx - s(3.2), dcy - s(3.2), del_cx + s(3.2), dcy + s(3.2), 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
                cv.line(del_cx - s(3.2), dcy + s(3.2), del_cx + s(3.2), dcy - s(3.2), 0xFFFFFF, alpha * 0.75 * dvisible, s(1.6));
            }
        }
        // 命中区：整行打开目录；行尾删除（确认态扩展到胶囊）
        push_hit_body_from(hits, HitId::FileOpen(i), pad, y, iw - s(24.0), row_h, list_top);
        let del_w = if armed { s(64.0) } else { s(24.0) };
        let del_x0 = pad + iw - del_w;
        push_hit_body_from(hits, HitId::FileDel(i), del_x0, y, del_w, row_h, list_top);
        y += row_h;
    }
    cv.pop_clip();
}

// ---- 4 剪贴板 ----

fn page_clips(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    draw_label(app, cv, w, crate::config::panel_name("clips"), Some(format!("{} 条", app.store.clips.len())), alpha);

    if app.store.clips.is_empty() {
        empty_hint(cv, w, "复制任意文本后会自动出现在这里", alpha);
        return;
    }
    let row_h = s(27.0);
    let sc = app.panel_scroll.min(panel_scroll_max(app, "clips"));
    let mut y = s(BODY_TOP) - sc;
    for (i, c) in app.store.clips.iter().enumerate() {
        let flat: String = c
            .text
            .chars()
            .map(|ch| if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch })
            .collect();
        let txt = ellipsize(flat.trim(), 11.0, iw - s(16.0));
        let hp = row_hover(app, cv, pad - s(6.0), y + s(1.0), iw + s(12.0), row_h - s(2.0), &HitId::ClipCopy(i), alpha);
        cv.circle(pad + s(3.0), y + row_h / 2.0, s(2.0), mix(crate::theme::main(), crate::theme::light(), hp), alpha * (0.8 + 0.2 * hp));
        cv.text1(pad + s(12.0), y, iw - s(12.0), row_h, &txt, 11.0, W_NORMAL, TXT, alpha);
        push_hit_body(hits, HitId::ClipCopy(i), pad, y, iw, row_h);
        y += row_h;
    }
}

// ---- 5 健康 ----

fn page_health(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let goal = app.cfg.health.water_goal_cups.max(1);
    let cups = app.store.water_cups;
    draw_label(app, cv, w, crate::config::panel_name("health"), Some(format!("目标 {} 杯", goal)), alpha);

    // 饮水卡
    let y = s(BODY_TOP);
    let ch = s(58.0);
    cv.rrect(pad, y, iw, ch, s(12.0), ROW, alpha);
    cv.text1(pad + s(14.0), y + s(8.0), iw - s(120.0), s(14.0), "今日饮水", 9.5, W_NORMAL, TXT2, alpha);
    cv.text1(pad + s(14.0), y + s(22.0), iw - s(120.0), s(26.0), &format!("{} / {} 杯", cups, goal), 15.0, W_BOLD, crate::theme::light(), alpha);

    let bw = s(72.0);
    let bh = s(26.0);
    let bx = pad + iw - s(14.0) - bw;
    let by = y + s(10.0);
    let whp = app.anims.hover_p(&HitId::WaterAdd);
    cv.rrect(bx, by, bw, bh, bh / 2.0, mix(crate::theme::main(), crate::theme::dark(), whp), alpha * (0.16 + 0.13 * whp));
    cv.rrect_stroke(bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0, bh / 2.0, crate::theme::main(), alpha * (0.65 + 0.3 * whp), s(1.0));
    cv.text_c(bx, by, bw, bh, "喝一杯", 10.5, W_SEMI, mix(crate::theme::light(), crate::theme::tint(), whp), alpha);
    push_hit(hits, HitId::WaterAdd, bx, by, bw, bh);

    // 进度条跟着弹簧平滑填充，喝一杯不会突然跳一截
    let wr = app.anims.water.v.clamp(0.0, 1.0);
    cv.progress(pad + s(14.0), y + s(48.0), iw - s(28.0), s(5.0), wr, crate::theme::main(), ROW2);

    // 久坐 / 目标
    let gap = s(10.0);
    let half = (iw - gap) / 2.0;
    let y2 = s(142.0);
    let h2 = s(50.0);
    let idle_min = crate::app::now_sed_secs(app.last_sed_unix) / 60;
    let warn = idle_min >= app.cfg.health.sedentary_minutes;

    cv.rrect(pad, y2, half, h2, s(12.0), ROW, alpha);
    cv.text1(pad + s(12.0), y2 + s(8.0), half - s(24.0), s(14.0), "久坐时长", 9.5, W_NORMAL, TXT2, alpha);
    cv.text1(pad + s(12.0), y2 + s(22.0), half - s(24.0), s(20.0), &format!("{} 分钟", idle_min), 13.0, W_BOLD, if warn { ORANGE } else { TXT }, alpha);

    let x2 = pad + half + gap;
    cv.rrect(x2, y2, half, h2, s(12.0), ROW, alpha);
    cv.text1(x2 + s(12.0), y2 + s(8.0), half - s(24.0), s(14.0), "今日还差", 9.5, W_NORMAL, TXT2, alpha);
    let left = goal.saturating_sub(cups);
    let left_txt = if left == 0 { "已达标".to_string() } else { format!("{} 杯", left) };
    cv.text1(
        x2 + s(12.0),
        y2 + s(22.0),
        half - s(24.0),
        s(20.0),
        &left_txt,
        13.0,
        W_BOLD,
        if left == 0 { GREEN } else { crate::theme::light() },
        alpha,
    );
}

// ---- 6 薪资 ----

fn page_salary(app: &App, cv: &mut Canvas, w: f32, alpha: f32) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let st = clock::local_time();
    let secs = clock::secs_of_day(&st);
    let sal = app.sal.earned_today_f(clock::secs_of_day_f(&st));
    let money = format!("¥{}", crate::modules::salary::fmt_money(sal));
    let cur = "¥";
    draw_label(app, cv, w, crate::config::panel_name("salary"), Some(format!("时薪 {}{:.2}", cur, app.sal.per_sec * 3600.0)), alpha);

    // 主数字
    let y = s(BODY_TOP);
    let ch = s(62.0);
    cv.rrect(pad, y, iw, ch, s(12.0), ROW, alpha);
    cv.text1(pad + s(14.0), y + s(9.0), iw - s(28.0), s(14.0), "今日已赚", 9.5, W_NORMAL, TXT2, alpha);
    cv.text1(pad + s(14.0), y + s(24.0), iw - s(28.0), s(30.0), &money, 21.0, W_BOLD, GREEN, alpha);
    let rate = format!("每秒 +{}{:.2}", cur, app.sal.per_sec);
    cv.text_ex(pad, y + s(10.0), iw - s(14.0), s(14.0), &rate, 9.0, W_NORMAL, TXT3, alpha, 2, 1);
    let span = format!("{} – {}", app.cfg.salary.work_start, app.cfg.salary.work_end);
    cv.text_ex(pad, y + s(26.0), iw - s(14.0), s(14.0), &span, 9.0, W_NORMAL, TXT3, alpha, 2, 1);

    // 工时进度（扣除午休）
    let worked = app.sal.worked_secs_excl_lunch(secs as f64) as i64;
    let total = (((app.sal.end_min - app.sal.start_min).max(0)) * 60
        - ((app.sal.lunch_end - app.sal.lunch_start).max(0.0) as i64) * 60)
        .max(60);
    let y2 = s(146.0);
    let h2 = s(46.0);
    cv.rrect(pad, y2, iw, h2, s(12.0), ROW, alpha);
    let label = format!("已工作 {}h {:02}m / {}h", worked / 3600, (worked % 3600) / 60, total / 3600);
    cv.text1(pad + s(14.0), y2 + s(8.0), iw - s(28.0), s(16.0), &label, 10.5, W_MED, TXT, alpha);
    let ratio = if total > 0 { worked as f32 / total as f32 } else { 0.0 };
    cv.progress(pad + s(14.0), y2 + s(28.0), iw - s(28.0), s(5.0), ratio, GREEN, ROW2);
}

// ---- 8 盯盘助手（A 股红涨绿跌）----

fn page_watch(app: &App, cv: &mut Canvas, w: f32, alpha: f32, hits: &mut Vec<Hit>) {
    let pad = s(18.0);
    let iw = w - pad * 2.0;
    let quotes = crate::modules::stock::snapshot();
    let items = crate::modules::stock::read_items();

    let holding: Vec<&crate::modules::stock::Item> =
        items.iter().filter(|i| i.qty > 0.0).collect();
    draw_label(
        app,
        cv,
        w,
        &format!("{} · {} 只持仓", crate::config::panel_name("watch"), holding.len()),
        None,
        alpha,
    );

    // ── 标题行右侧「＋ 编辑」小按钮：与标题同一水平，紧凑胶囊 ──
    let ebw = text_w("＋ 编辑", 9.5) + s(16.0);
    let ebx = pad + iw - ebw;
    let eby = s(53.0);
    let ebh = s(WATCH_BTN_H);
    let ahp = app.anims.hover_p(&HitId::WatchEdit);
    cv.rrect(ebx, eby, ebw, ebh, s(6.0), mix(0x1E1E25, crate::theme::main(), ahp * 0.5), alpha * (0.85 + 0.15 * ahp));
    cv.rrect_stroke(ebx, eby, ebw, ebh, s(6.0), crate::theme::main(), alpha * (0.5 + 0.5 * ahp), s(1.0));
    cv.text1(ebx, eby, ebw, ebh, "＋ 编辑", 9.5, W_SEMI, mix(TXT2, crate::theme::tint(), ahp), alpha);
    push_hit(hits, HitId::WatchEdit, ebx, eby, ebw, ebh);

    if items.is_empty() {
        // 空状态引导：标题行下方即可，不再有独立按钮区
        let hy = s(BODY_TOP);
        cv.text_ex(pad, hy, iw, s(24.0), "暂无自选，点右上角「＋ 编辑」添加第一条", 10.5, W_NORMAL, TXT3, alpha, 1, 1);
        cv.text_ex(pad, hy + s(24.0), iw, s(18.0), "每行一条：代码 名称 成本价 数量（可只填代码）· 红涨绿跌", 8.5, W_NORMAL, TXT3, alpha, 1, 1);
        return;
    }

    // ── 顶部盈亏卡：两个口径左右分栏并列，各自标注基准，消除「对不上」的疑惑 ──
    let y = s(BODY_TOP);
    let ch = s(WATCH_CARD_H);
    cv.rrect(pad, y, iw, ch, s(11.0), ROW, alpha);
    if !holding.is_empty() {
        let fp = crate::modules::stock::float_pnl(&quotes, &items);
        let pnl = crate::modules::stock::today_pnl(&quotes, &items);
        let half = iw * 0.5;
        let left = pad + s(12.0);
        let right = pad + half + s(2.0);
        // 中间竖向分隔线
        cv.line(pad + half, y + s(8.0), pad + half, y + ch - s(8.0), BORDER, alpha * 0.9, s(1.0));
        // 左栏：持仓浮盈
        cv.text1(left, y + s(4.0), half - s(18.0), s(12.0), "持仓浮盈", 9.5, W_NORMAL, TXT2, alpha);
        cv.text1(left, y + s(16.0), half - s(18.0), s(25.0), &format!("¥{:+.2}", fp), 18.0, W_BOLD, pnl_color(fp), alpha);
        // 右栏：今日盈亏
        cv.text1(right, y + s(4.0), half - s(16.0), s(12.0), "今日盈亏", 9.5, W_NORMAL, TXT2, alpha);
        cv.text1(right, y + s(16.0), half - s(16.0), s(25.0), &format!("¥{:+.2}", pnl), 18.0, W_BOLD, pnl_color(pnl), alpha);
    } else {
        cv.text1(pad + s(14.0), y + s(9.0), iw - s(28.0), s(13.0), "盯盘盈亏", 9.5, W_NORMAL, TXT2, alpha);
        cv.text1(pad + s(14.0), y + s(22.0), iw - s(28.0), s(23.0), "暂无持仓 · 仅监控自选", 14.0, W_SEMI, TXT, alpha);
        cv.text_ex(pad, y + s(13.0), iw - s(28.0), s(14.0), "填成本价与数量后显示收益", 8.5, W_NORMAL, TXT3, alpha, 2, 1);
    }

    // ── 行情列表（可滚动）── 与卡片保持间距，避免首行被卡片圆角遮挡
    let list_top = y + ch + s(8.0);
    let row_h = s(WATCH_ROW_H);
    let sc = app.panel_scroll.min(panel_scroll_max(app, "watch"));
    let mut yy = list_top - sc;
    cv.push_clip(0.0, list_top - s(2.0), w, (body_clip_bottom() - list_top + s(2.0)).max(0.0));

    // 列宽：名称+代码 0.52 / 现价 0.23 / 涨跌幅 0.25
    let name_w = iw * 0.52;
    let price_x = pad + name_w;
    let pct_x = pad + iw * 0.75;
    for (idx, it) in items.iter().enumerate() {
        let q = crate::modules::stock::quote_for(&quotes, &it.code);
        // 行间细分割线，避免放大后各行粘连
        if idx > 0 {
            cv.line(pad, yy, pad + iw, yy, BORDER, alpha * 0.55, s(1.0));
        }
        // 名称：优先已识别名称，缺省用行情快照名称。名称与代码分两段绘制，
        // 代码用较小字号紧跟其后，名称本身获得更宽空间，避免长名称被裁。
        let nm = if it.name.is_empty() { q.map(|x| x.name.as_str()).unwrap_or("") } else { it.name.as_str() };
        let name_text_w = name_w - s(50.0); // 给 6 位代码留出固定宽度
        cv.text1(pad, yy + s(1.0), name_text_w, row_h - s(2.0), nm, 12.5, W_SEMI, TXT, alpha);
        cv.text1(pad + name_text_w, yy + s(3.0), s(50.0), row_h - s(6.0), &it.code, 10.0, W_NORMAL, TXT3, alpha);
        // 现价
        let pc = q.map(|x| color_pct(x.chg_pct));
        let price = q.map(|x| format!("{:.2}", x.price)).unwrap_or_else(|| "--".into());
        cv.text_ex(price_x, yy, iw * 0.23, row_h, &price, 14.0, W_BOLD, pc.unwrap_or(TXT), alpha, 1, 1);
        // 涨跌幅
        let pct = q.map(|x| format!("{:+.2}%", x.chg_pct)).unwrap_or_else(|| "-".into());
        cv.text_ex(pct_x, yy, iw * 0.25, row_h, &pct, 12.5, W_SEMI, pc.unwrap_or(TXT3), alpha, 2, 1);
        yy += row_h;
    }
    cv.pop_clip();
}

/// A 股习惯：正红负绿，0 用灰
fn pnl_color(v: f64) -> u32 {
    if v > 0.001 {
        RED
    } else if v < -0.001 {
        GREEN
    } else {
        TXT
    }
}

fn color_pct(pct: f64) -> u32 {
    if pct > 0.001 {
        RED
    } else if pct < -0.001 {
        GREEN
    } else {
        TXT
    }
}

// ---- 底部分页圆点：默认小圆点，悬停时该点变成对应页面图标 ----

/// 底部一排页点：默认仍是小圆点，当前页主题色实心；鼠标悬停到某点时，该点渐变成
/// 对应页面的图标（带高亮底圈 + 页面名标签），左右邻居被推开并放大成波浪。
/// 点一下直接跳到对应页。单页时只保留隐私声明。
fn draw_pager(app: &App, cv: &mut Canvas, w: f32, h: f32, idx: usize, pages: &[&str], alpha: f32, hits: &mut Vec<Hit>) {
    let n = pages.len();
    let cy = h - s(28.0);

    if n <= 1 {
        cv.text_ex(0.0, cy - s(10.0), w, s(20.0), "零收集 · 零上传 · 零通讯", 8.5, W_NORMAL, TXT3, alpha * 0.85, 1, 1);
        return;
    }

    let gap = s(16.0);
    let total = gap * (n as f32 - 1.0);
    let x0 = (w - total) / 2.0;
    let base_r = s(2.6);
    let amp = s(5.0);
    let icon_size = s(26.0);
    let ring_r = icon_size / 2.0 + s(5.0);

    // 波浪中心 = 悬停进度最高的点；进出都由弹簧进度驱动，平滑回落
    let (mut center, mut cp) = (None, 0.0f32);
    for i in 0..n {
        let v = app.anims.hover_p(&HitId::PageDot(i));
        if v > cp {
            cp = v;
            center = Some(i);
        }
    }

    for i in 0..n {
        let base_x = x0 + gap * i as f32;
        let p = match center {
            Some(c) if cp > 0.004 => {
                let d = (i as i32 - c as i32).abs();
                cp * match d { 0 => 1.0, 1 => 0.55, 2 => 0.2, _ => 0.0 }
            }
            _ => 0.0,
        };
        // 只有正悬停的那个点变图标；邻居保持圆点波浪放大
        let icon_p = if center == Some(i) { cp } else { 0.0 };

        // Dock 让位：邻居按距离向外推开，给放大的图标腾出空间
        let mut x = base_x;
        if let Some(c) = center {
            if cp > 0.004 && i != c {
                let d = (i as i32 - c as i32).abs();
                let push = match d { 1 => s(10.0), 2 => s(5.0), 3 => s(2.0), _ => 0.0 };
                x += (i as f32 - c as f32).signum() * push * cp;
            }
        }

        let r = base_r + amp * p;
        let col = if i == idx { crate::theme::main() } else { mix(TXT3, TXT, p) };
        if icon_p < 0.999 {
            cv.circle(x, cy, r, col, alpha * (1.0 - icon_p));
        }
        if icon_p > 0.01 {
            let accent = if i == idx { crate::theme::main() } else { TXT };
            cv.circle(x, cy, ring_r * icon_p, mix(ROW, accent, 0.18), alpha * icon_p);
            cv.circle_stroke(x, cy, ring_r * icon_p, accent, alpha * 0.55 * icon_p, s(1.0));
            draw_page_icon(cv, pages[i], x, cy, icon_size * icon_p, accent, alpha * icon_p);
        }

        // 命中区固定在原位、贴合间距：hover 状态稳定，不会因推开/放大而抖动
        push_hit(hits, HitId::PageDot(i), base_x - gap / 2.0, cy - s(12.0), gap, s(24.0));
    }

    // 页面名标签：悬停点的正上方，随弹簧进度渐显
    if let Some(c) = center {
        if cp > 0.05 {
            let label = crate::config::panel_name(pages[c]);
            let fs = 10.5;
            let tw = text_w(label, fs);
            let bw = tw + s(14.0);
            let bh = s(20.0);
            let bx = (x0 + gap * c as f32 - bw / 2.0).clamp(s(4.0), w - bw - s(4.0));
            let by = cy - ring_r - s(5.0) - bh;
            cv.rrect(bx, by, bw, bh, s(6.0), mix(ROW, 0x000000, 0.3), alpha * cp);
            cv.text1(bx + s(7.0), by, tw, bh, label, fs, W_NORMAL, TXT, alpha * cp);
        }
    }

    // 页码淡字贴在右侧（箭头已移除）
    let pg = format!("{}/{}", idx + 1, n);
    cv.text_ex(w - s(34.0), cy - s(10.0), s(28.0), s(20.0), &pg, 8.5, W_NORMAL, TXT3, alpha * 0.8, 2, 1);
}

/// 8 个页面图标：统一线性笔画，坐标按 24×24 网格缩放到实际图标尺寸
fn draw_page_icon(cv: &mut Canvas, kind: &str, cx: f32, cy: f32, size: f32, col: u32, alpha: f32) {
    let k = size / 24.0;
    let l = cx - size / 2.0;
    let t = cy - size / 2.0;
    let px = |x: f32| l + x * k;
    let py = |y: f32| t + y * k;
    let sw = (s(1.3) * (size / s(18.0))).clamp(s(1.3), s(2.2));
    match kind {
        "overview" => {
            for &(x, y) in &[(3.0f32, 3.0f32), (14.0, 3.0), (3.0, 14.0), (14.0, 14.0)] {
                cv.rrect_stroke(px(x), py(y), 7.0 * k, 7.0 * k, 1.6 * k, col, alpha, sw);
            }
        }
        "schedule" => {
            cv.rrect_stroke(px(3.0), py(5.0), 18.0 * k, 16.0 * k, 2.5 * k, col, alpha, sw);
            cv.line(px(7.0), py(3.0), px(7.0), py(7.0), col, alpha, sw);
            cv.line(px(17.0), py(3.0), px(17.0), py(7.0), col, alpha, sw);
            cv.line(px(3.0), py(10.0), px(21.0), py(10.0), col, alpha, sw);
            cv.line(px(8.0), py(14.0), px(11.0), py(14.0), col, alpha, sw);
            cv.line(px(8.0), py(17.0), px(14.0), py(17.0), col, alpha, sw);
        }
        "todo" => {
            cv.line(px(4.0), py(7.0), px(5.6), py(8.6), col, alpha, sw);
            cv.line(px(5.6), py(8.6), px(8.5), py(5.7), col, alpha, sw);
            cv.line(px(4.0), py(16.0), px(5.6), py(17.6), col, alpha, sw);
            cv.line(px(5.6), py(17.6), px(8.5), py(14.7), col, alpha, sw);
            cv.line(px(12.0), py(7.0), px(20.0), py(7.0), col, alpha, sw);
            cv.line(px(12.0), py(16.0), px(20.0), py(16.0), col, alpha, sw);
        }
        "files" => {
            cv.rrect_stroke(px(3.0), py(8.0), 18.0 * k, 12.0 * k, 2.0 * k, col, alpha, sw);
            cv.line(px(3.0), py(12.0), px(21.0), py(12.0), col, alpha, sw);
            cv.line(px(7.0), py(8.0), px(8.5), py(5.8), col, alpha, sw);
            cv.line(px(8.5), py(5.8), px(11.0), py(5.8), col, alpha, sw);
        }
        "clips" => {
            cv.rrect_stroke(px(5.0), py(4.0), 14.0 * k, 17.0 * k, 2.0 * k, col, alpha, sw);
            cv.rrect_stroke(px(9.0), py(2.0), 6.0 * k, 4.5 * k, 1.2 * k, col, alpha, sw);
            cv.line(px(9.0), py(11.0), px(15.0), py(11.0), col, alpha, sw);
            cv.line(px(9.0), py(15.0), px(15.0), py(15.0), col, alpha, sw);
        }
        "health" => {
            cv.circle_stroke(px(12.0), py(13.5), 5.5 * k, col, alpha, sw);
            cv.line(px(8.6), py(9.6), px(12.0), py(3.0), col, alpha, sw);
            cv.line(px(12.0), py(3.0), px(15.4), py(9.6), col, alpha, sw);
        }
        "salary" => {
            cv.circle_stroke(px(12.0), py(12.0), 9.0 * k, col, alpha, sw);
            cv.line(px(8.5), py(8.0), px(12.0), py(12.0), col, alpha, sw);
            cv.line(px(12.0), py(12.0), px(15.5), py(8.0), col, alpha, sw);
            cv.line(px(12.0), py(12.0), px(12.0), py(17.5), col, alpha, sw);
            cv.line(px(9.0), py(13.5), px(15.0), py(13.5), col, alpha, sw);
            cv.line(px(9.0), py(16.0), px(15.0), py(16.0), col, alpha, sw);
        }
        "watch" => {
            cv.line(px(3.0), py(17.0), px(8.0), py(12.0), col, alpha, sw);
            cv.line(px(8.0), py(12.0), px(12.0), py(15.0), col, alpha, sw);
            cv.line(px(12.0), py(15.0), px(20.0), py(6.0), col, alpha, sw);
            cv.line(px(16.0), py(6.0), px(20.0), py(6.0), col, alpha, sw);
            cv.line(px(20.0), py(6.0), px(20.0), py(10.0), col, alpha, sw);
        }
        _ => cv.circle(px(12.0), py(12.0), 3.0 * k, col, alpha),
    }
}


// ---------------- OLE 拖放（文件中转站）----------------

/// OLE 拖放回调所在线程发往主窗口的自定义消息（TRAY_MSG 已占 WM_APP+1）。
/// 关键：绝不能在 OLE 回调里直接重绘（D2D + UpdateLayeredWindow 很慢，
/// 会阻塞拖放循环、导致系统判定为拒绝并显示"禁止"光标），
/// 必须 PostMessage 到主线程消息循环再处理。
pub const WM_DRAG_ENTER: u32 = WM_APP + 2;
pub const WM_DRAG_LEAVE: u32 = WM_APP + 3;
pub const WM_DROP_FILES: u32 = WM_APP + 4;

thread_local! {
    /// 必须持有 IDropTarget 强引用直到 RevokeDragDrop ——
    /// RegisterDragDrop 不会 AddRef，局部变量释放后系统内指针悬空，
    /// 拖放直接失效（"禁止"光标、不触发 DragEnter）。这是上一版失效的根因。
    static DROP_TARGET: RefCell<Option<IDropTarget>> = const { RefCell::new(None) };
}
static DROP_HWND: AtomicIsize = AtomicIsize::new(0);

#[implement(IDropTarget)]
struct DropTarget;

/// 把自定义消息投递到主窗口；HWND 存原子量避免回调里访问 thread_local APP。
fn post_drag_msg(msg: u32) {
    let h = DROP_HWND.load(Ordering::Acquire);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, _pdataobj: Ref<'_, windows::Win32::System::Com::IDataObject>, _grfkeystate: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
        // 告诉系统接受复制操作 —— 光标从"禁止"变为"复制"
        unsafe { *pdweffect = DROPEFFECT_COPY };
        crate::app::diag_log("DragEnter: 接受拖放");
        post_drag_msg(WM_DRAG_ENTER);
        Ok(())
    }
    fn DragOver(&self, _grfkeystate: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *pdweffect = DROPEFFECT_COPY };
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        crate::app::diag_log("DragLeave: 取消高亮");
        post_drag_msg(WM_DRAG_LEAVE);
        Ok(())
    }
    fn Drop(&self, pdataobj: Ref<'_, windows::Win32::System::Com::IDataObject>, _grfkeystate: MODIFIERKEYS_FLAGS, _pt: &POINTL, pdweffect: *mut DROPEFFECT) -> windows::core::Result<()> {
        use windows::Win32::System::Com::{FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
        use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
        use windows::Win32::System::Ole::ReleaseStgMedium;
        unsafe { *pdweffect = DROPEFFECT_COPY };
        let obj = match pdataobj.ok() {
            Ok(o) => o,
            Err(e) => {
                crate::app::diag_log(&format!("Drop: IDataObject 无效 {:?}", e));
                return Ok(());
            }
        };
        let mut paths: Vec<String> = Vec::new();
        unsafe {
            let fmt = FORMATETC {
                cfFormat: 15, // CF_HDROP
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                ptd: std::ptr::null_mut(),
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            let mut medium = match obj.GetData(&fmt) {
                Ok(m) => m,
                Err(e) => {
                    crate::app::diag_log(&format!("Drop: GetData(CF_HDROP) 失败 {:?}", e));
                    return Ok(());
                }
            };
            let hdrop = HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
            crate::app::diag_log(&format!("Drop: 收到 {} 个文件", count));
            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None) as usize;
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len + 1];
                let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
                buf.truncate(n);
                paths.push(String::from_utf16_lossy(&buf));
            }
            ReleaseStgMedium(&mut medium);
        }
        // 解析结果交给主线程处理（用 Box 传堆指针，避免回调里重绘/长时间占用）
        if !paths.is_empty() {
            let boxed = Box::new(paths);
            let ptr = Box::into_raw(boxed) as isize;
            let h = DROP_HWND.load(Ordering::Acquire);
            if h != 0 {
                unsafe {
                    let _ = PostMessageW(Some(HWND(h as *mut _)), WM_DROP_FILES, WPARAM(0), LPARAM(ptr));
                }
            } else {
                unsafe { drop(Box::from_raw(ptr as *mut Vec<String>)); }
            }
        }
        post_drag_msg(WM_DRAG_LEAVE);
        Ok(())
    }
}

pub fn register_ole_drop(hwnd: HWND) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let dt: IDropTarget = DropTarget.into();
        RegisterDragDrop(hwnd, &dt)?;
        // 关键：持有强引用到 RevokeDragDrop，防止 COM 对象被释放导致悬空指针
        DROP_TARGET.with(|c| *c.borrow_mut() = Some(dt));
        DROP_HWND.store(hwnd.0 as isize, Ordering::Release);
    }
    Ok(())
}

pub fn revoke_ole_drop(hwnd: HWND) -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        RevokeDragDrop(hwnd)?;
    }
    DROP_TARGET.with(|c| *c.borrow_mut() = None);
    DROP_HWND.store(0, Ordering::Release);
    Ok(())
}

