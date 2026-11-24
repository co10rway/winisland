//! --preview 系列：把胶囊/面板离屏渲染成 BMP（UI 开发核对用），渲染完即退出。
//! 该子模块只被 `run()` 的 CLI 分支与 `install_preview_app` 调用，与主消息循环解耦。

use super::*;

pub(crate) fn preview_app(demo: bool) -> App {
    let cfg = Config::load();
    crate::theme::init(&cfg.theme);
    let mut store = Store::load();
    let st = clock::local_time();
    store.reset_day_if_needed(&clock::today_str(&st));
    if demo {
        crate::modules::sched_ics::inject_demo(vec![
            crate::store::ScheduleItem { time: "09:30".into(), title: "早会 · 周会同步进度".into(), ..Default::default() },
            crate::store::ScheduleItem { time: "14:00".into(), title: "评审灵动岛面板交互稿".into(), ..Default::default() },
            crate::store::ScheduleItem { time: "19:30".into(), title: "健身 · 腿日".into(), ..Default::default() },
        ]);
        store.todos = vec![
            crate::store::TodoItem { title: "给设置页补面板轮播配置".into(), done: false },
            crate::store::TodoItem { title: "回复设计评审邮件".into(), done: false },
            crate::store::TodoItem { title: "把桌面截图归档".into(), done: true },
        ];
        store.files = vec![
            crate::store::FileItem { path: r"C:\Users\NINGMEI\Desktop\设计稿-v3.png".into(), name: "设计稿-v3.png".into(), size: 2_357_204, ts: 0 },
            crate::store::FileItem { path: r"C:\Users\NINGMEI\Desktop\需求文档.docx".into(), name: "需求文档.docx".into(), size: 48_120, ts: 0 },
        ];
        store.clips = vec![
            crate::store::ClipItem { text: "验证码 884231，5 分钟内有效，请勿泄露给他人。".into(), ts: 0 },
            crate::store::ClipItem { text: "git push origin feat/panel-carousel".into(), ts: 0 },
        ];
        store.water_cups = 3;
    }
    let sal = salary::SalaryCalc::new(&cfg.salary);
    let mut anims = Anims::new();
    anims.open.set(1.0);
    anims.panel.set(1.0);
    anims.content.set(1.0);
    anims.pill.set(0.0);
    anims.water.set(store.water_cups as f32 / cfg.health.water_goal_cups.max(1) as f32);
    App {
        hwnd: HWND::default(),
        canvas: None,
        glassy: None,
        cfg: cfg.clone(),
        store,
        sal,
        expanded: true,
        panel_open: true,
        pinned: std::env::args().any(|a| a == "--pinned"), // 离屏预览可看锁定态图标
        vis: Vis::Dock,
        pill_hover: false,
        hover_latch: false,
        occluded: false,
        occ_next: Instant::now(),
        leave_at: None,
        anims,
        px: 0,
        py: 0,
        page: 0,
        flash: None,
        pill_mode: 0,
        scroll: 0.0,
        scroll_max: 0.0,
        panel_scroll: 0.0,
        wheel_acc: 0.0,
        todo_del_arm: None,
        file_del_arm: None,
        schedule_del_arm: None,
        hits: Vec::new(),
        mouse: POINT::default(),
        hover: None,
        drag_hover: false,
        reminder: None,
        last_water_unix: 0,
        last_sed_unix: files::now_unix(),
        code_until: None,
        code_pending: false,
        last_mail_seq: 0,
        code_copied_at: None,
        peak_wpm: 0,
        rot_i: 0,
        rot_prev: 0,
        rot_next: Instant::now() + Duration::from_secs(5),
        icon_energy: 0.0,
        icon_phase: 0.0,
        icon_state: crate::modules::icon::ST_DEFAULT,
        icon_state_prev: crate::modules::icon::ST_DEFAULT,
        icon_blend: 1.0,
        icon_sound_hold: 0.0,
        icon_key_idx: 0,
        icon_key_age: 9.0,
        icon_key_seed: 0x9E37_79B9,
        dirty: false,
        last_save: Instant::now(),
        mail_shared: std::sync::Arc::new(std::sync::Mutex::new(mail::MailShared::default())),
        hidden: false,
        placed: (99_999, 99_999, 0, 0),
    }
}

/// 把一帧画到 BMP 文件（32 位、自底向上）
fn save_frame(canvas: &mut Canvas, app: &App, w: i32, h: i32, path: &str) {
    let memdc = canvas.draw_to_mem(w, h, 0x131317, |cv, cw, ch| {
        let mut hits = Vec::new();
        ui::draw_frame(app, cv, cw, ch, &mut hits);
    });
    unsafe { dump_bmp(memdc, w, h, path); }
}

/// 把 Canvas 的内存 DC 导出成 32 位 BMP（所有离屏预览共用）
pub(crate) unsafe fn dump_bmp(memdc: windows::Win32::Graphics::Gdi::HDC, w: i32, h: i32, path: &str) {
    use std::ffi::c_void;
    use windows::Win32::Graphics::Gdi::*;

    let hdc_screen = GetDC(None);
    let mem2 = CreateCompatibleDC(Some(hdc_screen));
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(Some(mem2), &bi, DIB_RGB_COLORS, &mut bits, None, 0).expect("dib");
    let old = SelectObject(mem2, hbmp.into());
    let _ = BitBlt(mem2, 0, 0, w, h, Some(memdc), 0, 0, SRCCOPY);

    let row = (w * 4) as usize;
    let data_len = row * h as usize;
    let off: u32 = 14 + 40;
    let total = off + data_len as u32;
    let mut out = Vec::with_capacity(total as usize);
    let mut fh = [0u8; 14];
    fh[0] = b'B';
    fh[1] = b'M';
    fh[2..6].copy_from_slice(&total.to_le_bytes());
    fh[10..14].copy_from_slice(&off.to_le_bytes());
    let mut ih = [0u8; 40];
    ih[0..4].copy_from_slice(&40u32.to_le_bytes());
    ih[4..8].copy_from_slice(&w.to_le_bytes());
    ih[8..12].copy_from_slice(&h.to_le_bytes());
    ih[12..14].copy_from_slice(&1u16.to_le_bytes());
    ih[14..16].copy_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&fh);
    out.extend_from_slice(&ih);
    out.extend_from_slice(std::slice::from_raw_parts(bits as *const u8, data_len));

    let _ = std::fs::write(path, &out);
    println!("saved {path}");
    let _ = SelectObject(mem2, old);
    let _ = DeleteObject(hbmp.into());
    let _ = DeleteDC(mem2);
    ReleaseDC(None, hdc_screen);
}

/// --preview [输出目录] [--demo]：把每个面板页离屏渲染成 BMP（UI 开发核对用）
pub(crate) fn preview_shots() {
    let mut app = preview_app(std::env::args().nth(3).as_deref() == Some("--demo"));
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview: canvas 初始化失败: {e}");
            return;
        }
    };
    let (w, h) = (render::si(ui::PANEL_W as f32), render::si(ui::PANEL_H as f32));
    let pages = ui::effective_pages(&app.cfg);
    for (i, kind) in pages.iter().enumerate() {
        app.page = i;
        save_frame(&mut canvas, &app, w, h, &format!("{out_dir}/panel_{i}_{kind}.bmp"));
    }

    // 胶囊态（收起）：核对左侧图标 / 时间日期 / 右侧金额三段布局
    app.expanded = false;
    app.anims.open.set(0.0);
    app.anims.panel.set(0.0);
    app.anims.content.set(0.0);
    app.anims.pill.set(1.0);
    app.anims.pill_content.set(1.0);
    let (pw, ph) = (render::si(ui::PILL_W as f32), render::si(ui::PILL_H as f32));
    save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/pill.bmp"));
}

/// --preview-wave [输出目录]：模拟「3 帧空闲 → 按键 → 停手」的真实能量曲线，
/// 逐帧导出胶囊，用于核对响应是否够快（该立刻升、立刻降）
pub(crate) fn preview_wave() {
    let mut app = preview_app(false);
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-wave: canvas 初始化失败: {e}");
            return;
        }
    };
    app.expanded = false;
    app.anims.open.set(0.0);
    app.anims.panel.set(0.0);
    app.anims.content.set(0.0);
    app.anims.pill.set(1.0);
    app.anims.pill_content.set(1.0);
    let (pw, ph) = (render::si(ui::PILL_W as f32), render::si(ui::PILL_H as f32));
    let dt = 1.0 / 60.0;
    app.icon_energy = 0.0;
    app.icon_phase = 0.0;
    app.icon_state = crate::modules::icon::ST_DEFAULT;
    app.icon_state_prev = crate::modules::icon::ST_DEFAULT;
    app.icon_blend = 1.0;
    app.icon_sound_hold = 0.0;
    app.icon_key_idx = 0;
    app.icon_key_age = 9.0;
    app.icon_key_seed = 0x9E37_79B9;
    for i in 0..24u64 {
        // 模拟连续打字：间隔不均的几次敲击（第 3/5/8/11/15 帧），
        // 用来核对「每次只亮一颗、且亮的是不同键」。
        // 秒计数只在第 16、17 帧推进两次，模拟停手 2 秒后回到默认态
        //（真实运行每秒一次；预览里每帧都推进会把手感打乱）。
        if i == 16 || i == 17 {
            typing::tick_second();
        }
        if matches!(i, 3 | 5 | 8 | 11 | 15) {
            typing::bump_key();
        }
        icon::advance(&mut app, dt);
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/wave_{i:02}.bmp"));
    }
}

/// --preview-panel [输出目录]：展开面板各页 + 剪贴板页滚动态（验证滚轮滚动改造）
pub(crate) fn preview_panel() {
    let mut app = preview_app(true);
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-panel".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-panel: canvas 初始化失败: {e}");
            return;
        }
    };
    app.expanded = true;
    app.anims.open.set(1.0);
    app.anims.panel.set(1.0);
    app.anims.content.set(1.0);
    app.anims.pill.set(0.0);
    app.anims.pill_content.set(0.0);
    let (pw, ph) = (render::si(ui::PANEL_W as f32), render::si(ui::PANEL_H as f32));
    // 剪贴板塞满 9 条，制造超出可视区的长内容
    for k in 0..7 {
        app.store.clips.push(crate::store::ClipItem {
            text: format!("第 {} 条：长内容滚动验证——面板可视区只有约 5 行，其余靠滚轮滚动查看", k),
            ts: 0,
        });
    }
    let pages = ui::effective_pages(&app.cfg);
    for (i, kind) in pages.iter().enumerate() {
        app.page = i;
        app.panel_scroll = 0.0;
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/panel_p{i}_{kind}.bmp"));
    }
    // 剪贴板页滚动态：中部 / 底部（滚动条位置 + 完整列表）
    if let Some(i) = pages.iter().position(|&k| k == "clips") {
        app.page = i;
        let max = ui::panel_scroll_max(&app, "clips");
        app.panel_scroll = max / 2.0;
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/panel_clips_mid.bmp"));
        app.panel_scroll = max;
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/panel_clips_bottom.bmp"));
    }
}

/// --preview-reminder [输出目录]：久坐 / 喝水 / 日程三种提醒在**胶囊**上的样子
/// （再附一张面板开着时的顶部提醒条）
pub(crate) fn preview_reminder() {
    let mut app = preview_app(false);
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-reminder".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-reminder: canvas 初始化失败: {e}");
            return;
        }
    };
    app.expanded = false;
    app.panel_open = false;
    app.anims.open.set(0.0);
    app.anims.panel.set(0.0);
    app.anims.content.set(0.0);
    app.anims.pill.set(1.0);
    app.anims.pill_content.set(1.0);
    let (pw, ph) = (render::si(ui::PILL_W as f32), render::si(ui::PILL_H as f32));
    let cases = [
        (ToastKind::Water, String::new(), "water"),
        (ToastKind::Sedentary, String::new(), "sitting"),
        (ToastKind::Schedule, "14:30 · 产品评审会".to_string(), "schedule"),
    ];
    for (kind, msg, name) in cases.iter() {
        app.reminder = Some(Toast { kind: *kind, msg: msg.clone() });
        app.pill_mode = ui::pill_mode_key(&app);
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/reminder_{name}.bmp"));
    }
    // 面板开着时：提醒落在头部提醒条上
    app.expanded = true;
    app.panel_open = true;
    app.anims.open.set(1.0);
    app.anims.panel.set(1.0);
    app.anims.content.set(1.0);
    app.anims.pill.set(0.0);
    let pages = ui::effective_pages(&app.cfg);
    app.page = pages.iter().position(|p| *p == "health").unwrap_or(0);
    app.reminder = Some(Toast { kind: ToastKind::Water, msg: String::new() });
    let (pw, ph) = (render::si(ui::PANEL_W as f32), render::si(ui::PANEL_H as f32));
    save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/reminder_panel_bar.bmp"));
}

/// --preview-pill [输出目录]：胶囊右侧轮播的 5 个条目 + 2 个中途滑动帧
pub(crate) fn preview_pill() {
    let mut app = preview_app(false);
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-pill".to_string());
    // --festival=圣诞节：离屏预览时注入测试节日名，验证日期下方小字展示（正式运行无此参数）
    if let Some(arg) = std::env::args().find(|a| a.starts_with("--festival=")) {
        let name = arg["--festival=".len()..].to_string();
        crate::modules::workday::set_preview_festival(if name.is_empty() { None } else { Some(name) });
    }
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-pill: canvas 初始化失败: {e}");
            return;
        }
    };
    app.expanded = false;
    app.anims.open.set(0.0);
    app.anims.panel.set(0.0);
    app.anims.content.set(0.0);
    app.anims.pill.set(1.0);
    app.anims.pill_content.set(1.0);
    let (pw, ph) = (render::si(ui::PILL_W as f32), render::si(ui::PILL_H as f32));
    for i in 0..ui::pill_count(&app).max(1) {
        app.rot_i = i;
        app.rot_prev = i;
        app.anims.rot.set(1.0);
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/pill_item{i}.bmp"));
    }
    // 中途帧：0 → 1 的滑动过程
    app.rot_prev = 0;
    app.rot_i = 1;
    for (k, p) in [0.25f32, 0.55, 0.85].iter().enumerate() {
        app.anims.rot.set(*p);
        save_frame(&mut canvas, &app, pw, ph, &format!("{out_dir}/pill_slide{k}.bmp"));
    }
}

/// --preview-icons [输出目录]：三态图标对照图
/// 上排 = 大尺寸完整底板（70×70），下排 = 胶囊内真实小尺寸（32×24，徽章模式）
pub(crate) fn preview_icons() {
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-icons".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-icons: canvas 初始化失败: {e}");
            return;
        }
    };
    let (w, h) = (render::si(270.0), render::si(180.0));
    let energies = [0.05f32, 0.45, 0.95];
    let phases = [0.6f32, 1.9, 3.2];
    for (id, _) in crate::modules::icon::ICON_KINDS.iter() {
        let memdc = canvas.draw_to_mem(w, h, 0x17171C, |cv, cw, ch| {
            for k in 0..3 {
                let cx = cw * (k as f32 + 0.5) / 3.0;
                // 三列分别展示三态：默认（低能量）/ 键盘（中能量）/ 音乐（高能量）
                let st = match k {
                    0 => crate::modules::icon::ST_DEFAULT,
                    1 => crate::modules::icon::ST_TYPING,
                    _ => crate::modules::icon::ST_MUSIC,
                };
                let sig = crate::modules::icon::Signal {
                    energy: energies[k],
                    phase: phases[k],
                    state: st,
                    prev: st,
                    blend: 1.0,
                    key_idx: 1,
                    key_age: 0.05,
                };
                crate::modules::icon::draw_icon(cv, id, cx, ch * 0.33, render::s(70.0), render::s(70.0), sig, 1.0);
                // 下排：与胶囊内一致的槽位尺寸，用来判断真实小尺寸下的清晰度
                crate::modules::icon::draw_icon(cv, id, cx, ch * 0.82, render::s(34.0), render::s(26.0), sig, 1.0);
            }
        });
        unsafe { dump_bmp(memdc, w, h, &format!("{out_dir}/icon_{id}.bmp")); }
    }
}

/// --preview-keys [输出目录]：键盘态「随机选键」核对图
/// 上排：第 1/2/3 颗键分别被点亮（70×70 底板）
/// 下排：胶囊内真实槽位 34×26，键已衰减 0.12s，确认只剩一颗在亮
pub(crate) fn preview_keys() {
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-keys".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-keys: canvas 初始化失败: {e}");
            return;
        }
    };
    let (w, h) = (render::si(270.0), render::si(150.0));
    for (id, _) in crate::modules::icon::ICON_KINDS.iter() {
        let memdc = canvas.draw_to_mem(w, h, 0x17171C, |cv, cw, ch| {
            for k in 0..3u8 {
                let cx = cw * (k as f32 + 0.5) / 3.0;
                let sig = crate::modules::icon::Signal {
                    energy: 0.85,
                    phase: 2.1,
                    state: crate::modules::icon::ST_TYPING,
                    prev: crate::modules::icon::ST_TYPING,
                    blend: 1.0,
                    key_idx: k,
                    key_age: 0.0,
                };
                crate::modules::icon::draw_icon(cv, id, cx, ch * 0.28, render::s(70.0), render::s(70.0), sig, 1.0);
                let aged = crate::modules::icon::Signal { key_age: 0.12, ..sig };
                crate::modules::icon::draw_icon(cv, id, cx, ch * 0.74, render::s(34.0), render::s(26.0), aged, 1.0);
            }
        });
        unsafe { dump_bmp(memdc, w, h, &format!("{out_dir}/keys_{id}.bmp")); }
    }
}

/// --preview-anim [输出目录]：逐帧导出展开动画与翻页动画，用于核对中间帧
pub(crate) fn preview_anim() {
    let mut app = preview_app(true);
    let out_dir = std::env::args().nth(2).unwrap_or_else(|| "preview-anim".to_string());
    let _ = std::fs::create_dir_all(&out_dir);
    let mut canvas = match Canvas::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[winisland] preview-anim: canvas 初始化失败: {e}");
            return;
        }
    };
    let dt = 1.0 / 60.0;

    // 1) 展开：胶囊 → 面板
    app.expanded = false;
    app.anims.open.set(0.0);
    app.anims.panel.set(0.0);
    app.anims.content.set(0.0);
    app.anims.pill.set(1.0);
    app.anims.page.set(1.0);
    app.expanded = true;
    for f in 0..26 {
        let (w, h) = app.window_size();
        save_frame(&mut canvas, &app, w, h, &format!("{out_dir}/open_{f:02}.bmp"));
        step_anims(&mut app, dt);
    }

    // 2) 翻页：概览 → 日程
    let from = app.page;
    let n = ui::effective_pages(&app.cfg).len();
    go_page(&mut app, (from + 1).min(n.saturating_sub(1)));
    for f in 0..18 {
        let (w, h) = app.window_size();
        save_frame(&mut canvas, &app, w, h, &format!("{out_dir}/page_{f:02}.bmp"));
        step_anims(&mut app, dt);
    }
}
