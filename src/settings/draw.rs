
// 设置窗绘制：页面、表单行、时间选择器等的离屏渲染（与窗口/控件逻辑解耦）。

use super::*;

// ───────────────────────── 绘制 ─────────────────────────

pub(crate) fn draw_all(cv: &mut Canvas, w: f32, h: f32, page: usize, hover: HitId, hits: &mut Vec<Hit>) {
    draw_titlebar(cv, w, hover, hits);
    draw_sidebar(cv, h, page, hover, hits);
    let list = pages();
    if let Some(p) = list.get(page) {
        draw_page(cv, p, page, hover, hits);
    }
    draw_footer(cv, w, h, hover, hits);
    draw_time_picker(cv, w, h, hover, hits);
}

/// 自绘时间选择浮层：半透明遮罩 + 居中卡片 + 时/分步进 + 确定/取消
fn draw_time_picker(cv: &mut Canvas, w: f32, h: f32, hover: HitId, hits: &mut Vec<Hit>) {
    let Some((page, slot)) = TIME_PICK.with(|c| c.get()) else { return };
    // 遮罩：点击空白处取消
    cv.rrect(0.0, 0.0, w, h, 0.0, 0x000000, 0.55);
    push_hit(hits, HitId::TimeCancel, 0.0, 0.0, w, h);

    let pw = s(300.0);
    let ph = s(288.0);
    let px = (w - pw) / 2.0;
    let py = (h - ph) / 2.0;
    cv.rrect(px, py, pw, ph, s(16.0), 0x1C1C23, 1.0);
    cv.rrect_stroke(px + 0.5, py + 0.5, pw - 1.0, ph - 1.0, s(16.0), 0x3A3A48, 1.0, s(1.0));

    let title = match slot { 4 => "上午上班", 5 => "下班时间", 6 => "午休开始", _ => "下午上班" };
    cv.text_c(px, py + s(16.0), pw, s(28.0), title, 15.0, W_SEMI, TXT, 1.0);
    cv.text_c(px, py + s(46.0), pw, s(18.0), "点击数字框直接输入 · 或 ▲ ▼ 调整", 10.5, W_NORMAL, TXT3, 0.9);

    // 时 / 分两列
    let col_y = py + s(76.0);
    let col_w = s(96.0);
    let gap = s(20.0);
    let hx = px + (pw - col_w * 2.0 - gap) / 2.0;
    let mx = hx + col_w + gap;

    draw_time_col(cv, hx, col_y, col_w, TIME_H.with(|c| c.get()), "时", 0, hover, HitId::TimeStep(0, 1), HitId::TimeStep(0, -1), hits);
    draw_time_col(cv, mx, col_y, col_w, TIME_M.with(|c| c.get()), "分", 1, hover, HitId::TimeStep(1, 1), HitId::TimeStep(1, -1), hits);
    cv.text_c(px + (pw - s(20.0)) / 2.0, col_y + s(40.0), s(20.0), s(56.0), ":", 24.0, W_MED, TXT3, 0.9);

    // 底部按钮：取消 / 确定
    let bw = s(110.0);
    let by = py + ph - s(58.0);
    let cancel_x = px + (pw - bw * 2.0 - s(16.0)) / 2.0;
    let ok_x = cancel_x + bw + s(16.0);
    let cancel_hov = hover == HitId::TimeCancel;
    let ok_hov = hover == HitId::TimeOk;
    draw_button(cv, cancel_x, by, bw, s(38.0), "取消", false, cancel_hov, 1.0);
    draw_button(cv, ok_x, by, bw, s(38.0), "确定", true, ok_hov, 1.0);
    push_hit(hits, HitId::TimeCancel, cancel_x, by, bw, s(38.0));
    push_hit(hits, HitId::TimeOk, ok_x, by, bw, s(38.0));
    // 防止 page / slot 未使用警告（title 已用 slot）
    let _ = page;
}

/// 时间选择器的一列：上箭头 + 大数字 + 下箭头 + 单位标签
/// `part` 0=时 1=分；当该列正在键盘输入时，画输入缓冲 + 光标，边框高亮。
#[allow(clippy::too_many_arguments)]
fn draw_time_col(cv: &mut Canvas, x: f32, y: f32, w: f32, val: u32, unit: &str, part: usize, hover: HitId, up: HitId, down: HitId, hits: &mut Vec<Hit>) {
    let bh = s(30.0);
    let num_y = y + bh;
    let num_h = s(56.0);
    let editing = TIME_EDIT.with(|c| c.get()) == Some(part);
    let buf = TIME_BUF.with(|b| b.borrow()[part].clone());
    // 数字框（编辑态描蓝边）
    cv.rrect(x, num_y, w, num_h, s(10.0), INPUT_BG, 1.0);
    cv.rrect_stroke(
        x + 0.5, num_y + 0.5, w - 1.0, num_h - 1.0, s(10.0),
        if editing { 0x4B85F0 } else { INPUT_BD }, 1.0,
        if editing { s(1.6) } else { s(1.0) },
    );
    if editing {
        // 输入中：显示缓冲，末尾画一条竖光标
        let disp = if buf.is_empty() { format!("{val:02}") } else { buf.clone() };
        cv.text_c(x, num_y, w - s(6.0), num_h, &disp, 26.0, W_SEMI, TXT, 1.0);
        let txt_w = tw(&disp, 26.0);
        let cursor_x = x + (w - txt_w) / 2.0 + txt_w / 2.0 + s(2.0);
        cv.rrect(cursor_x, num_y + s(13.0), s(2.0), num_h - s(26.0), 0.0, 0x7FB4FF, 1.0);
    } else {
        cv.text_c(x, num_y, w, num_h, &format!("{val:02}"), 26.0, W_SEMI, TXT, 1.0);
    }
    // 单位
    cv.text_c(x, y + bh + num_h + s(4.0), w, s(18.0), unit, 10.5, W_NORMAL, TXT3, 0.9);
    // 数字框本体可点击 → 进入键盘输入
    push_hit(hits, HitId::TimeNum(part), x, num_y, w, num_h);
    // 上 / 下箭头按钮
    let ah = s(26.0);
    let up_hov = hover == up;
    let down_hov = hover == down;
    cv.rrect(x, y, w, ah, s(7.0), if up_hov { 0x2C2C36 } else { 0x23232B }, 1.0);
    cv.text_c(x, y, w, ah, "▲", 11.0, W_MED, if up_hov { TXT } else { TXT2 }, 1.0);
    let dy = y + bh + num_h + s(24.0);
    cv.rrect(x, dy, w, ah, s(7.0), if down_hov { 0x2C2C36 } else { 0x23232B }, 1.0);
    cv.text_c(x, dy, w, ah, "▼", 11.0, W_MED, if down_hov { TXT } else { TXT2 }, 1.0);
    push_hit(hits, up, x, y, w, ah);
    push_hit(hits, down, x, dy, w, ah);
}

/// --preview-settings [输出目录] [--scroll=N]：把每一页离屏渲染成 BMP（仅调试用）。
/// --scroll=N 模拟内容滚动 N 像素（验证滚动裁剪用）。
pub fn preview_pages() {
    crate::app::install_preview_app(false);
    let out = std::env::args().nth(2).unwrap_or_else(|| "preview-settings".to_string());
    let scroll: f32 = std::env::args()
        .find(|a| a.starts_with("--scroll="))
        .and_then(|a| a[9..].parse().ok())
        .unwrap_or(0.0);
    let _ = std::fs::create_dir_all(&out);
    let mut cv = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-settings: canvas 初始化失败: {e}");
            return;
        }
    };
    // 调试：--preview-timepick 让 page2 渲染时打开「下班时间」选择器
    if std::env::args().any(|a| a == "--preview-timepick") {
        TIME_H.with(|c| c.set(18));
        TIME_M.with(|c| c.set(30));
        TIME_PICK.with(|c| c.set(Some((2, 5))));
        time_val_set(2, 5, "18:30");
    }
    let (w, h) = client_size();
    let n = pages().len();
    for i in 0..n {
        SCROLL.with(|c| c.set(scroll));
        let mut hits = Vec::new();
        let memdc = cv.draw_to_mem(w, h, BG, |cv, w, h| {
            draw_all(cv, w, h, i, HitId::None, &mut hits);
        });
        unsafe {
            crate::app::dump_bmp(memdc, w, h, &format!("{out}/page{i}.bmp"));
        }
    }
}

fn draw_titlebar(cv: &mut Canvas, w: f32, hover: HitId, hits: &mut Vec<Hit>) {
    // 徽标：与灵动岛胶囊同源的蓝色渐变方块
    let bx = s(20.0);
    let by = s(13.0);
    let bs = s(30.0);
    cv.rrect_grad(bx, by, bs, bs, s(9.0), crate::theme::light(), crate::theme::dark(), false, 1.0);
    cv.rrect(bx + bs * 0.28, by + bs * 0.43, bs * 0.42, s(4.0), s(2.0), 0xFFFFFF, 0.95);
    cv.circle(bx + bs * 0.76, by + bs * 0.45, s(1.6), 0xFFFFFF, 0.9);

    cv.text1(bx + bs + s(12.0), s(13.0), s(220.0), s(19.0), "WinIsland", 13.5, W_SEMI, TXT, 1.0);
    cv.text1(bx + bs + s(12.0), s(31.0), s(220.0), s(15.0), "偏好设置", 10.5, W_NORMAL, TXT3, 1.0);

    let bw = s(WIN_BTN);
    let gap = s(6.0);
    let bty = s(TITLE_H / 2.0) - bw / 2.0;
    let close_x = w - s(16.0) - bw;
    let min_x = close_x - gap - bw;

    let hov_close = hover == HitId::Close;
    let hov_min = hover == HitId::Min;
    if hov_close {
        cv.rrect(close_x, bty, bw, bw, s(8.0), DANGER, 0.2);
    } else if hov_min {
        cv.rrect(min_x, bty, bw, bw, s(8.0), 0xFFFFFF, 0.07);
    }

    let cxi = min_x + bw / 2.0;
    let cyi = bty + bw / 2.0;
    cv.line(cxi - s(4.5), cyi, cxi + s(4.5), cyi, if hov_min { TXT } else { TXT2 }, 1.0, s(1.0));

    let ccx = close_x + bw / 2.0;
    let ccy = bty + bw / 2.0;
    let icon_c = if hov_close { 0xFF9EA2 } else { TXT2 };
    cv.line(ccx - s(4.0), ccy - s(4.0), ccx + s(4.0), ccy + s(4.0), icon_c, 1.0, s(1.0));
    cv.line(ccx + s(4.0), ccy - s(4.0), ccx - s(4.0), ccy + s(4.0), icon_c, 1.0, s(1.0));

    push_hit(hits, HitId::Min, min_x, bty, bw, bw);
    push_hit(hits, HitId::Close, close_x, bty, bw, bw);

    cv.line(0.0, s(TITLE_H) - 0.5, w, s(TITLE_H) - 0.5, HAIR, 1.0, s(1.0));
}

fn draw_sidebar(cv: &mut Canvas, h: f32, page: usize, hover: HitId, hits: &mut Vec<Hit>) {
    let sw = s(SIDE_W);
    cv.rect(0.0, 0.0, sw, h, SIDE_BG, 1.0);
    cv.line(sw - 0.5, 0.0, sw - 0.5, h, HAIR, 1.0, s(1.0));

    let item_x = s(12.0);
    let item_w = sw - s(24.0);
    let item_h = s(NAV_H);
    let mut y = s(TITLE_H + 12.0);

    for (gi, gname) in GROUPS.iter().enumerate() {
        cv.text1(s(24.0), y, sw - s(36.0), s(24.0), gname, 10.0, W_SEMI, TXT3, 0.9);
        y += s(26.0);

        for (p, group, label) in NAV.iter() {
            if *group != gi {
                continue;
            }
            let active = *p == page;
            let hov = hover == HitId::Nav(*p);
            if active {
                cv.rrect(item_x, y, item_w, item_h, s(9.0), NAV_ON, 1.0);
                cv.rrect(s(4.0), y + s(9.0), s(3.0), s(16.0), s(1.5), crate::theme::main(), 1.0);
            } else if hov {
                cv.rrect(item_x, y, item_w, item_h, s(9.0), 0xFFFFFF, 0.05);
            }
            let c = if active { TXT } else if hov { 0xDCDCE4 } else { TXT2 };
            let w = if active { W_SEMI } else { W_NORMAL };
            cv.text1(item_x + s(16.0), y, item_w - s(44.0), item_h, label, 12.0, w, c, 1.0);

            // 模块状态点：实心 = 开，空心 = 关
            if (1..=9).contains(p) || *p == 12 {
                let dx = item_x + item_w - s(14.0);
                let dy = y + item_h / 2.0;
                if module_enabled(*p) {
                    cv.circle(dx, dy, s(3.0), crate::theme::main(), 0.9);
                } else {
                    cv.circle_stroke(dx, dy, s(2.6), TXT3, 0.8, s(1.0));
                }
            }
            push_hit(hits, HitId::Nav(*p), item_x, y, item_w, item_h);
            y += item_h + s(2.0);
        }
        y += s(12.0);
    }

    cv.text1(s(24.0), h - s(36.0), sw - s(36.0), s(16.0), "零收集 · 零上传 · 零通讯", 9.5, W_NORMAL, TXT3, 0.85);
    cv.text1(s(24.0), h - s(21.0), sw - s(36.0), s(16.0), concat!("v", env!("CARGO_PKG_VERSION")), 9.5, W_NORMAL, TXT3, 0.7);
}

fn draw_page(cv: &mut Canvas, page: &Page, idx: usize, hover: HitId, hits: &mut Vec<Hit>) {
    let cx = s(SIDE_W + PAD);
    let cw = s(WIN_W - SIDE_W - PAD * 2.0);

    cv.text1(cx, s(74.0), cw, s(28.0), page.title, 19.0, W_SEMI, TXT, 1.0);
    cv.text1(cx, s(106.0), cw, s(18.0), page.desc, 11.0, W_NORMAL, TXT3, 1.0);

    let lay = layout(page);
    let dim = !page_on(idx);
    let off = clamp_scroll(idx, &lay);

    // 内容视口：页标题/描述以下、页脚以上。滚动后的行只许在这个范围内绘制和接收点击，
    // 否则会盖到标题栏（含关闭按钮）和页标题上，还会截走它们的命中区。
    let vp_top = s(130.0);
    let vp_bot = s(WIN_H - FOOT_H);
    cv.push_clip(cx, vp_top, cw, vp_bot - vp_top);

    for (cy, ch) in lay.cards.iter() {
        cv.rrect(cx, s(*cy - off), cw, s(*ch), s(CARD_R), CARD, 1.0);
        cv.rrect_stroke(cx + 0.5, s(*cy - off) + 0.5, cw - 1.0, s(*ch) - 1.0, s(CARD_R), CARD_BD, 0.9, s(1.0));
    }

    // 卡片内行间分隔线（连续的行才画）
    let mut prev: Option<f32> = None;
    let mut row_hits: Vec<Hit> = Vec::new();
    for pl in lay.placed.iter() {
        let row = &page.rows[pl.row];
        let is_card_row = matches!(
            row.kind,
            RowKind::Toggle | RowKind::Field | RowKind::Time | RowKind::Action | RowKind::PageRow | RowKind::Theme | RowKind::Backdrop | RowKind::Weekday
        );
        if is_card_row {
            if let Some(py) = prev {
                if (pl.y - py - ROW_H).abs() < 0.01 {
                    cv.line(cx + s(18.0), s(pl.y - off), cx + cw - s(18.0), s(pl.y - off), HAIR, 0.9, s(1.0));
                }
            }
        }
        prev = if is_card_row { Some(pl.y) } else { None };
        draw_row(cv, row, idx, pl, cx, cw, hover, dim, &mut row_hits, off);
    }
    cv.pop_clip();

    // 行命中区裁到视口内：滚出视口的行不允许截走标题栏 / 页脚 / 页标题的点击
    for h in row_hits {
        let top = h.rect.top.max(vp_top as i32);
        let bottom = h.rect.bottom.min(vp_bot as i32);
        if bottom > top {
            hits.push(Hit {
                id: h.id,
                rect: RECT { left: h.rect.left, top, right: h.rect.right, bottom },
            });
        }
    }
}

/// 读取并收敛当前页的滚动偏移（内容不足一屏时恒为 0）
fn clamp_scroll(_idx: usize, lay: &Layout) -> f32 {
    let bottom = lay.placed.iter().map(|p| p.y + p.h).fold(0.0f32, f32::max) + 20.0;
    let max = (bottom - (WIN_H - FOOT_H)).max(0.0); // 视口底 = WIN_H - FOOT_H
    let cur = SCROLL.with(|c| c.get()).clamp(0.0, max);
    SCROLL.with(|c| c.set(cur));
    cur
}

#[allow(clippy::too_many_arguments)]
fn draw_row(cv: &mut Canvas, row: &Row, page: usize, pl: &Placed, cx: f32, cw: f32, hover: HitId, dim: bool, hits: &mut Vec<Hit>, off: f32) {
    let ry = s(pl.y - off);
    let rh = s(pl.h);
    let ta = if dim { 0.62 } else { 1.0 };
    let inner_l = cx + s(18.0);
    let inner_w = cw - s(36.0);

    let has_desc = !row.desc.is_empty();
    let ly = if has_desc { ry + s(9.0) } else { ry + (rh - s(19.0)) / 2.0 };
    let dy = ry + s(28.0);

    match row.kind {
        RowKind::Caption => {
            cv.text1(cx + s(2.0), ry, cw, rh, row.label, 10.5, W_SEMI, TXT2, 0.85);
            if row.slot != 0 {
                // 来源状态徽标：每次重绘实时读 config，开关/输入变化后立即同步
                let (color, txt) = cal_state(row.slot);
                let tag = format!("● {txt}");
                let w = tw(&tag, 10.0);
                cv.text1(cx + cw - w - s(8.0), ry, w + s(2.0), rh, &tag, 10.0, W_NORMAL, color, 0.95);
            }
        }
        RowKind::Note => {
            cv.text1(cx + s(2.0), ry, cw, rh, row.label, 10.5, W_NORMAL, TXT2, 0.8);
        }
        RowKind::MailStatus => {
            let st = crate::modules::mail::last_status();
            let txt = if st.is_empty() { "状态：尚未开始".to_string() } else { format!("状态：{st}") };
            // 失败用红色提示，成功 / 监听中用次要色
            let color = if txt.contains("失败") || txt.contains("未配置") || txt.contains("请先") {
                0xE07A7A
            } else {
                TXT2
            };
            cv.text1(cx + s(2.0), ry, cw, rh, &txt, 10.5, W_NORMAL, color, 0.9);
        }
        RowKind::Hero => {
            draw_hero(cv, cx, ry, cw, rh);
        }
        RowKind::Toggle => {
            cv.text1(inner_l, ly, inner_w - s(64.0), s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, inner_w - s(64.0), s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let on = if page == 0 && row.slot == 1 {
                crate::app::with_cfg(|c| c.auto_collapse)
            } else if page == 0 {
                crate::app::with_cfg(|c| c.autostart)
            } else if page == 7 && row.slot == 1 {
                // 桌面分区：开机自启（slot 0 的启用开关走通用 module_enabled）
                crate::app::with_cfg(|c| c.zones_autostart)
            } else if page == 3 && matches!(row.slot, 10 | 20 | 30) {
                // 日程三个来源各自的开关，不能落到 module_enabled（那是模块总开关）
                crate::app::with_cfg(|c| match row.slot {
                    10 => c.schedule_sub.feishu.on,
                    20 => c.schedule_sub.dingtalk.on,
                    _ => c.schedule_sub.wecom.on,
                })
            } else {
                module_enabled(page)
            };
            let sw = s(44.0);
            let sh = s(26.0);
            let sx = cx + cw - s(18.0) - sw;
            let sy = ry + (rh - sh) / 2.0;
            draw_switch(cv, sx, sy, sw, sh, on, hover == HitId::Toggle(page, row.slot), if dim { 0.6 } else { 1.0 });
            push_hit(hits, HitId::Toggle(page, row.slot), sx - s(6.0), sy - s(6.0), sw + s(12.0), sh + s(12.0));
        }
        RowKind::PageRow => {
            // 名称与开关都从 config 实时取：用户上移/下移后，位置 i 上的 id 已经变了，
            // 若沿用建表时写死的标签就会「标签写的 A、实际控制 B」
            let slot = row.slot as usize;
            let (name, on, mod_off) =
                crate::app::with_cfg(|c| match c.panel.pages.get(slot) {
                    Some(p) => (
                        crate::config::panel_name(&p.id),
                        p.on,
                        !c.modules.page_on(&p.id),
                    ),
                    None => (row.label, false, false),
                });
            let sw = s(44.0);
            let arrows = s(26.0) * 2.0 + s(6.0);
            let txw = (inner_w - sw - arrows - s(26.0)).max(s(60.0));
            let num = format!("{}", slot + 1);
            cv.text_ex(inner_l, ry, s(16.0), rh, &num, 10.0, W_NORMAL, TXT3, ta, 0, 1);
            // 模块关闭时该页不会进轮播：整行降灰，开关置灰且不响应点击
            let lc = if mod_off {
                TXT3
            } else if on {
                TXT
            } else {
                TXT2
            };
            cv.text1(inner_l + s(18.0), ry, txw, rh, name, 13.0, W_MED, lc, ta);

            let ah = s(BTN_H - 4.0);
            let aby = ry + (rh - ah) / 2.0;
            let mut bx = cx + cw - s(18.0) - sw - s(14.0) - arrows;
            for (label, slot, _, bw) in row.btns.iter() {
                let hov = hover == HitId::Btn(page, *slot);
                draw_button(cv, bx, aby, s(*bw), ah, label, false, hov, ta);
                push_hit(hits, HitId::Btn(page, *slot), bx, aby, s(*bw), ah);
                bx += s(*bw) + s(6.0);
            }

            let sx = cx + cw - s(18.0) - sw;
            let sh = s(26.0);
            let sy = ry + (rh - sh) / 2.0;
            // 模块关闭时这一页永远进不了轮播：开关置灰、不响应点击
            let sw_a = if mod_off { 0.3 } else if dim { 0.6 } else { 1.0 };
            draw_switch(cv, sx, sy, sw, sh, on, !mod_off && hover == HitId::Toggle(page, row.slot), sw_a);
            if !mod_off {
                push_hit(hits, HitId::Toggle(page, row.slot), sx - s(6.0), sy - s(6.0), sw + s(12.0), sh + s(12.0));
            }
        }
        RowKind::Theme => {
            cv.text1(inner_l, ly, inner_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, inner_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let cur = crate::theme::index();
            let n = crate::theme::THEMES.len();
            let d = s(22.0);
            let gap = s(13.0);
            let total = n as f32 * d + (n as f32 - 1.0) * gap;
            let mut bx = cx + cw - s(18.0) - total;
            let by = ry + (rh - d) / 2.0;
            for (i, (_, _, c)) in crate::theme::THEMES.iter().enumerate() {
                let hov = hover == HitId::Theme(i);
                if i == cur {
                    // 选中：亮色外圈
                    cv.rrect_stroke(bx - s(2.5), by - s(2.5), d + s(5.0), d + s(5.0), (d + s(5.0)) / 2.0, TXT, ta * 0.92, s(1.6));
                } else if hov {
                    cv.rrect_stroke(bx - s(2.5), by - s(2.5), d + s(5.0), d + s(5.0), (d + s(5.0)) / 2.0, TXT3, ta * 0.85, s(1.2));
                }
                cv.circle(bx + d / 2.0, by + d / 2.0, d / 2.0, *c, ta);
                push_hit(hits, HitId::Theme(i), bx - s(4.0), by - s(4.0), d + s(8.0), d + s(8.0));
                bx += d + gap;
            }
        }
        RowKind::Backdrop => {
            cv.text1(inner_l, ly, inner_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, inner_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let cur = crate::app::with_cfg(|c| c.backdrop.clone());
            let opts = ["无", "毛玻璃"];
            let ids = ["none", "glass"];
            let seg = s(76.0);
            let gap = s(8.0);
            let total = 2.0 * seg + 1.0 * gap;
            let mut bx = cx + cw - s(18.0) - total;
            let by = ry + (rh - s(30.0)) / 2.0;
            let bh = s(30.0);
            for i in 0..2usize {
                let sel = cur == ids[i];
                let hov = hover == HitId::Backdrop(i);
                let bg = if sel { crate::theme::main() } else if hov { 0x24242C } else { 0x1B1B22 };
                cv.rrect(bx, by, seg, bh, s(9.0), bg, ta * if sel { 0.95 } else { 0.9 });
                cv.text_c(bx, by, seg, bh, opts[i], 12.0, if sel { W_SEMI } else { W_NORMAL }, if sel { 0xFFFFFF } else { TXT2 }, ta);
                push_hit(hits, HitId::Backdrop(i), bx, by, seg, bh);
                bx += seg + gap;
            }
        }
        RowKind::Weekday => {
            let w = crate::app::with_cfg(|c| c.salary.weekdays_view());
            let d = s(30.0);
            let gap = s(6.0);
            let total = 7.0 * d + 6.0 * gap;
            let text_w_ = (inner_w - total - s(16.0)).max(s(80.0));
            cv.text1(inner_l, ly, text_w_, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, text_w_, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let labels = ["一", "二", "三", "四", "五", "六", "日"];
            let mut bx = cx + cw - s(18.0) - total;
            let by = ry + (rh - d) / 2.0;
            for i in 0..7usize {
                let on = w[i];
                let hov = hover == HitId::Weekday(i);
                let base = if on {
                    crate::anim::mix(CARD, crate::theme::main(), 0.26)
                } else if hov {
                    0x24242C
                } else {
                    0x1B1B22
                };
                cv.rrect(bx, by, d, d, s(9.0), base, ta);
                cv.rrect_stroke(
                    bx + 0.5, by + 0.5, d - 1.0, d - 1.0, s(9.0),
                    if on { crate::theme::main() } else if hov { 0x3C3C4A } else { CARD_BD },
                    if on { 0.95 } else { 0.85 }, s(1.2),
                );
                cv.text_c(bx, by, d, d, labels[i], 11.5,
                    if on { W_SEMI } else { W_NORMAL }, if on { TXT } else { TXT2 }, ta);
                push_hit(hits, HitId::Weekday(i), bx, by, d, d);
                bx += d + gap;
            }
        }
        RowKind::Field => {
            let bw = s(row.fw);
            let bx = cx + cw - s(18.0) - bw;
            let by = ry + (rh - s(INPUT_H)) / 2.0;
            let text_w = (inner_w - bw - s(16.0)).max(s(40.0));
            cv.text1(inner_l, ly, text_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, text_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let focused_hwnd = edit_hwnd(page, row.slot).unwrap_or_default();
            let focused = unsafe { GetFocus() } == focused_hwnd;
            // 输入框没完整进入内容视口就不画（HWND 在 place_edits 同步隐藏），
            // 避免滚动时出现被平切一半的破碎框；判定基准与 place_edits 一致（外框矩形）
            let vp_top = s(130.0);
            let vp_bot = s(WIN_H - FOOT_H);
            let box_in_vp = by >= vp_top && by + s(INPUT_H) <= vp_bot;
            if box_in_vp {
                draw_input_box(cv, bx, by, bw, s(INPUT_H), focused, ta);
                if !row.suffix.is_empty() {
                    let sw = tw(row.suffix, 10.5);
                    cv.text1(bx + bw - s(12.0) - sw, by, sw + s(2.0), s(INPUT_H), row.suffix, 10.5, W_NORMAL, TXT3, ta);
                }
            } else if focused {
                unsafe {
                    let _ = SetFocus(None);
                }
            }
        }
        RowKind::Time => {
            let bw = s(118.0);
            let bx = cx + cw - s(18.0) - bw;
            let by = ry + (rh - s(INPUT_H)) / 2.0;
            let text_w = (inner_w - bw - s(16.0)).max(s(40.0));
            cv.text1(inner_l, ly, text_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, text_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let hov = hover == HitId::Time(page, row.slot);
            let open = TIME_PICK.with(|c| c.get()) == Some((page, row.slot));
            draw_input_box(cv, bx, by, bw, s(INPUT_H), hov || open, ta);
            // 显示当前值（草稿），未配置则读配置默认
            let mut val = field_text(page, row.slot);
            if val.trim().is_empty() {
                val = crate::app::with_cfg(|c| match row.slot {
                    4 => c.salary.work_start.clone(),
                    5 => c.salary.work_end.clone(),
                    6 => c.salary.lunch_start.clone(),
                    _ => c.salary.lunch_end.clone(),
                });
            }
            if val.trim().is_empty() {
                val = match row.slot { 5 => "18:00".into(), 6 => "12:00".into(), 7 => "13:00".into(), _ => "09:00".into() };
            }
            cv.text1(bx + s(12.0), by, bw - s(46.0), s(INPUT_H), &val, 13.0, W_MED, TXT, ta);
            // 右侧时钟图标（圆环 + 指针）
            let icx = bx + bw - s(19.0);
            let icy = by + s(INPUT_H) / 2.0;
            cv.circle_stroke(icx, icy, s(6.2), TXT2, 0.85 * ta, s(1.2));
            cv.line(icx, icy, icx, icy - s(3.6), TXT2, 0.85 * ta, s(1.2));
            cv.line(icx, icy, icx + s(2.8), icy + s(1.0), TXT2, 0.85 * ta, s(1.2));
            push_hit(hits, HitId::Time(page, row.slot), bx, by, bw, s(INPUT_H));
        }
        RowKind::Action => {
            let mut total = s(10.0) * (row.btns.len() as f32 - 1.0).max(0.0);
            for (_, _, _, bw) in row.btns.iter() {
                total += s(*bw);
            }
            let text_w = (inner_w - total - s(16.0)).max(s(60.0));
            cv.text1(inner_l, ly, text_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, text_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let mut bx = cx + cw - s(18.0) - total;
            let by = ry + (rh - s(BTN_H)) / 2.0;
            for (label, slot, primary, bw) in row.btns.iter() {
                let hov = hover == HitId::Btn(page, *slot);
                draw_button(cv, bx, by, s(*bw), s(BTN_H), label, *primary, hov, ta);
                push_hit(hits, HitId::Btn(page, *slot), bx, by, s(*bw), s(BTN_H));
                bx += s(*bw) + s(10.0);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_switch(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, on: bool, hovered: bool, alpha: f32) {
    let r = h / 2.0;
    if on {
        let (c1, c2) = if hovered { (crate::theme::light(), crate::theme::dark()) } else { (crate::theme::main(), crate::theme::dark()) };
        cv.rrect_grad(x, y, w, h, r, c1, c2, true, alpha);
    } else {
        cv.rrect(x, y, w, h, r, if hovered { 0x35353F } else { 0x2B2B35 }, alpha);
        cv.rrect_stroke(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r, 0x3C3C48, alpha * 0.9, s(1.0));
    }
    let kr = r - s(3.0);
    let kx = if on { x + w - r } else { x + r };
    let ky = y + r;
    cv.circle(kx, ky + s(0.8), kr, 0x000000, 0.18 * alpha);
    cv.circle(kx, ky, kr, if on { 0xFFFFFF } else { 0xC9C9D4 }, alpha);
}

fn draw_input_box(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, focused: bool, alpha: f32) {
    cv.rrect(x, y, w, h, s(9.0), INPUT_BG, alpha);
    if focused {
        cv.rrect_stroke(x - s(1.0), y - s(1.0), w + s(2.0), h + s(2.0), s(10.0), crate::theme::main(), 0.25, s(2.0));
        cv.rrect_stroke(x, y, w, h, s(9.0), crate::theme::main(), 0.95, s(1.0));
    } else {
        cv.rrect_stroke(x, y, w, h, s(9.0), INPUT_BD, alpha * 0.95, s(1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_button(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32, label: &str, primary: bool, hovered: bool, alpha: f32) {
    if primary {
        let (c1, c2) = if hovered { (0x7FB4FF, 0x4B85F0) } else { (crate::theme::light(), crate::theme::dark()) };
        cv.rrect_grad(x, y, w, h, s(8.0), c1, c2, false, alpha);
        cv.text_c(x, y, w, h, label, 12.0, W_SEMI, 0xFFFFFF, alpha);
    } else {
        cv.rrect(x, y, w, h, s(8.0), if hovered { 0x2C2C36 } else { 0x23232B }, alpha);
        cv.rrect_stroke(x + 0.5, y + 0.5, w - 1.0, h - 1.0, s(8.0), if hovered { 0x3C3C4A } else { 0x30303B }, alpha, s(1.0));
        cv.text_c(x, y, w, h, label, 12.0, W_MED, if hovered { TXT } else { 0xE2E2EA }, alpha);
    }
}

/// 应用主图标（与 exe / 托盘 / assets/icon.ico 同源：dynamic-island.svg）。
/// 按 viewBox 256×256 的比例用 Canvas 图元分层复刻，参数为中心与边长。
fn draw_logo(cv: &Canvas, cx: f32, cy: f32, size: f32) {
    let x0 = cx - size / 2.0;
    let y0 = cy - size / 2.0;
    let k = size / 256.0;
    let p = |v: f32| v * k; // SVG 坐标 → 实际偏移

    // 1) 深色圆角底（SVG base 渐变 #202B48→#090D18，对角；此处用垂直近似）
    cv.rrect_grad(x0 + p(8.0), y0 + p(8.0), p(240.0), p(240.0), p(56.0),
                  0x202B48, 0x090D18, false, 1.0);
    // 2) 内描边 白色 9%
    cv.rrect_stroke(x0 + p(11.0), y0 + p(11.0), p(234.0), p(234.0), p(53.0),
                    0xFFFFFF, 0.09, p(2.0));
    // 3) 胶囊外发光（aurora @15%）
    cv.rrect_grad(x0 + p(32.0), y0 + p(94.0), p(192.0), p(74.0), p(37.0),
                  0x54F5F2, 0xB16BFF, true, 0.15);
    // 4) 胶囊主体：三色 aurora（青→蓝→紫）；rrect_grad 仅两色，用青→紫近似，
    //    中段线性插值自然呈蓝调，46px 下与 SVG 观感一致
    cv.rrect_grad(x0 + p(32.0), y0 + p(88.0), p(192.0), p(72.0), p(36.0),
                  0x54F5F2, 0xB16BFF, true, 1.0);
    // 5) 内层挖空 #080C17
    cv.rrect(x0 + p(38.0), y0 + p(94.0), p(180.0), p(60.0), p(30.0), 0x080C17, 1.0);
    // 6) 青色呼吸点 #65FAEF
    cv.circle(x0 + p(75.0), y0 + p(124.0), p(8.0), 0x65FAEF, 1.0);
    // 7) 三根律动竖条（圆头，SVG stroke-width 8 / linecap round）
    for (bx, by1, len, col) in [
        (154.0, 119.0, 10.0, 0x6A9CFF),
        (172.0, 113.0, 22.0, 0x9189FF),
        (190.0, 118.0, 12.0, 0xBE8BFF),
    ] {
        cv.rrect(x0 + p(bx - 4.0), y0 + p(by1 - 4.0), p(8.0), p(len + 8.0), p(4.0), col, 1.0);
    }
}

fn draw_hero(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32) {
    cv.rrect_grad(x, y, w, h, s(CARD_R), crate::theme::dark(), 0x15151A, true, 1.0);
    cv.rrect_stroke(x + 0.5, y + 0.5, w - 1.0, h - 1.0, s(CARD_R), crate::theme::light(), 0.9, s(1.0));

    let bs = s(46.0);
    let bx = x + s(20.0);
    let by = y + (h - bs) / 2.0;
    draw_logo(cv, bx + bs / 2.0, by + bs / 2.0, bs);

    let tx = bx + bs + s(18.0);
    cv.text1(tx, y + s(22.0), w - (tx - x) - s(20.0), s(26.0), "WinIsland", 17.0, W_SEMI, TXT, 1.0);
    let ver = concat!("v", env!("CARGO_PKG_VERSION"));
    let vw = tw(ver, 10.0) + s(16.0);
    cv.rrect(tx, y + s(52.0), vw, s(20.0), s(10.0), crate::theme::main(), 0.18);
    cv.text_c(tx, y + s(52.0), vw, s(20.0), ver, 10.0, W_MED, 0x9CC4FF, 1.0);
    cv.text1(tx + vw + s(10.0), y + s(52.0), w - (tx - x) - vw - s(30.0), s(20.0), "Rust 原生 · 单一 exe · 零运行时依赖", 10.5, W_NORMAL, TXT2, 0.95);
}

fn draw_footer(cv: &mut Canvas, w: f32, h: f32, hover: HitId, hits: &mut Vec<Hit>) {
    let fy = h - s(FOOT_H);
    cv.rect(s(SIDE_W), fy, w - s(SIDE_W), s(FOOT_H), FOOT_BG, 1.0);
    cv.line(s(SIDE_W), fy + 0.5, w, fy + 0.5, HAIR, 1.0, s(1.0));

    let bh = s(34.0);
    let by = fy + (s(FOOT_H) - bh) / 2.0;

    // 右侧「保存并应用」按钮
    let sw = s(120.0);
    let sx = w - s(PAD) - sw;
    draw_button(cv, sx, by, sw, bh, "保存并应用", true, hover == HitId::Sync, 1.0);
    push_hit(hits, HitId::Sync, sx, by, sw, bh);

    let st = STATUS.with(|c| c.borrow().clone());
    let (msg, c, saved) = match st {
        Some((m, until)) if Instant::now() < until => (m, GREEN, true),
        _ => ("修改后点击「保存并应用」保存到本机并刷新面板".to_string(), TXT3, false),
    };
    let tx = s(SIDE_W + PAD);
    let maxw = (sx - s(12.0) - tx).max(s(40.0));
    if saved {
        cv.circle(tx + s(3.0), fy + s(FOOT_H) / 2.0, s(3.0), GREEN, 0.95);
        cv.text1(tx + s(14.0), by, maxw - s(14.0), bh, &msg, 11.0, W_MED, c, 1.0);
    } else {
        cv.text1(tx, by, maxw, bh, &msg, 11.0, W_NORMAL, c, 1.0);
    }
}

