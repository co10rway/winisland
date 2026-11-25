//! 应用状态、窗口过程与消息循环

mod preview;
use preview::{preview_anim, preview_icons, preview_keys, preview_panel, preview_pill, preview_reminder, preview_shots, preview_wave};
pub(crate) use preview::dump_bmp;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::DataExchange::AddClipboardFormatListener;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{ShellExecuteW, DragAcceptFiles};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::HiDpi::*;
use windows::core::w;

use crate::anim::Anims;
use crate::config::{Config, data_dir};
use crate::render::{self, Canvas};
use crate::store::Store;
use crate::modules::{audio, clipboard, clock, files, icon, mail, occlusion, salary, tray, typing, workday};
use crate::ui::{self, HitId};

pub const TIMER_TICK: usize = 1; // 1 秒
/// 灵动岛距屏幕顶端的距离（物理像素）
const TOP_Y: i32 = 8;

/// 显示模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vis {
    /// 正常显示：没有窗口压住时是主面板胶囊，点开后是下拉面板
    Dock,
    /// 因被其它窗口遮挡而收成小黑条；鼠标移上去临时显示主面板胶囊
    Auto,
}


#[derive(Clone, Copy, PartialEq)]
pub enum ToastKind {
    Water,
    Sedentary,
    /// 日程临近
    Schedule,
    /// 盯盘助手：自选涨跌幅触发提醒
    Stock,
}

impl ToastKind {
    /// 确认按钮文案：喝水记一杯、久坐起来走走、日程知道就行
    pub fn ack_label(self) -> &'static str {
        match self {
            ToastKind::Water => "喝一杯",
            ToastKind::Sedentary => "起来走走",
            ToastKind::Schedule => "知道了",
            ToastKind::Stock => "知道了",
        }
    }
}

/// 提醒（久坐 / 喝水 / 日程临近）。弹出后强制展开面板并保持置顶，
/// 直到用户点确认按钮才消失，不存在自动过期。
pub struct Toast {
    pub kind: ToastKind,
    /// 日程提醒的正文（其它类型为空串）
    pub msg: String,
}

/// 面板内的一过性反馈文字（淡入 → 保持 → 淡出）
pub struct Flash {
    pub msg: String,
    pub start: Instant,
    pub end: Instant,
}

impl Flash {
    pub fn new(msg: impl Into<String>, ms: u64) -> Self {
        let now = Instant::now();
        Self { msg: msg.into(), start: now, end: now + Duration::from_millis(ms) }
    }

    pub fn alive(&self) -> bool {
        Instant::now() < self.end
    }

    /// 淡入 90ms、淡出 220ms，中间保持
    pub fn alpha(&self) -> f32 {
        let now = Instant::now();
        if now >= self.end {
            return 0.0;
        }
        let t = now.duration_since(self.start).as_secs_f32();
        let total = self.end.duration_since(self.start).as_secs_f32().max(0.05);
        let fin = (t / 0.09).min(1.0);
        let fout = ((total - t) / 0.22).min(1.0);
        fin.min(fout).clamp(0.0, 1.0)
    }
}

pub struct App {
    pub hwnd: HWND,
    pub canvas: Option<Canvas>,
    /// DirectComposition 毛玻璃链路（Mica/Acrylic）。None = 分层自绘路径。
    pub glassy: Option<crate::modules::glassy::Glassy>,
    pub cfg: Config,
    pub store: Store,
    pub sal: salary::SalaryCalc,

    pub expanded: bool,
    /// 用户是否点开了下拉面板（只有它能撑开大面板；默认只显示主面板胶囊）
    pub panel_open: bool,
    /// 面板是否被图钉锁定：锁定后忽略遮挡检测，面板始终展开
    pub pinned: bool,
    /// 显示模式：正常 / 遮挡收起
    pub vis: Vis,
    /// 收起态下鼠标悬停在小黑条上，显示主面板胶囊
    pub pill_hover: bool,
    /// 手动收起后必须先移开鼠标，才允许再次悬停展开
    pub hover_latch: bool,
    /// 面板区域当前是否被其它应用窗口压住
    pub occluded: bool,
    /// 下一次遮挡检测时间
    pub occ_next: Instant,
    /// 鼠标离开面板区域的时刻（用于延迟收回）
    pub leave_at: Option<Instant>,
    /// 全部过渡状态（展开、翻页、悬停、进度）
    pub anims: Anims,
    pub px: i32,
    pub py: i32,
    /// 展开面板当前轮播页（索引指向 ui::effective_pages）
    pub page: usize,
    /// 面板内的短暂反馈，如「已复制」
    pub flash: Option<Flash>,
    /// 胶囊内容模式（提醒/验证码插入时用于淡入）
    pub pill_mode: u32,

    pub scroll: f32,
    pub scroll_max: f32,
    /// 展开面板当前轮播页的内容滚动量（滚轮滚动，翻页时清零）
    pub panel_scroll: f32,
    /// 滚轮累积量（一格 120）
    pub wheel_acc: f32,
    /// 待办的「二次确认删除」待确认行下标（防止误删；None = 无待确认）
    pub todo_del_arm: Option<usize>,
    /// 文件的「二次确认删除」待确认行下标
    pub file_del_arm: Option<usize>,
    /// 手动日程的「二次确认删除」待确认下标（对应 store.local_schedules 的下标）
    pub schedule_del_arm: Option<usize>,
    pub hits: Vec<ui::Hit>,
    #[allow(dead_code)]
    pub mouse: POINT,
    /// 当前鼠标悬停的控件（用于翻页箭头等的高亮）
    pub hover: Option<ui::HitId>,
    /// 文件正拖到岛上（OLE DragEnter→DragLeave/Drop 之间），用于面板高亮
    pub drag_hover: bool,

    /// 待用户确认的提醒（久坐 / 喝水 / 日程）。存在期间胶囊常亮显示提醒内容：
    /// 不自动过期、不被遮挡收成小黑条、不被悬停/轮播顶掉，
    /// 直到用户点击胶囊（或提醒条按钮）才清除
    pub reminder: Option<Toast>,
    pub last_water_unix: i64,
    /// 上次"起来走走"确认的时间戳（Unix 秒）。与喝水一样按真实时间流逝判定，
    /// 连续打字或完全不动都会累计，不再依赖系统输入空闲时间。
    pub last_sed_unix: i64,
    pub code_until: Option<Instant>,
    /// 验证码尚未点击复制：一直展示，不自动消失（与喝水提醒一致，必须用户确认）
    pub code_pending: bool,
    /// 已处理的验证码序号：邮件线程每弹一封新验证码就 seq+1，主界面只认增量。
    pub last_mail_seq: u64,
    /// 验证码「复制」按钮被点击的时刻：胶囊上短暂显示「已复制」反馈
    pub code_copied_at: Option<Instant>,
    pub peak_wpm: u64,
    /// 胶囊右侧信息轮播：当前条目、上一条目（滑动中的旧内容）、下一次切换时间
    pub rot_i: usize,
    pub rot_prev: usize,
    pub rot_next: Instant,
    /// 胶囊图标：平滑后的综合能量（打字 + 声音）与持续累积的相位；
    /// 以及三态形态（默认/键盘/音乐）与切换进度（见 modules::icon）
    pub icon_energy: f32,
    pub icon_phase: f32,
    pub icon_state: u8,
    pub icon_state_prev: u8,
    pub icon_blend: f32,
    /// 声音形态保持计时（秒）：音量掉到 0 后仍维持音乐态一段时间，避免安静段闪烁
    pub icon_sound_hold: f32,
    /// 键盘态：当前被点亮的那颗按键（0..2）、距上次敲击的秒数、随机种子
    pub icon_key_idx: u8,
    pub icon_key_age: f32,
    pub icon_key_seed: u32,

    pub dirty: bool,
    pub last_save: Instant,
    pub mail_shared: mail::SharedMail,
    pub hidden: bool, // 托盘菜单"显示/隐藏"
    /// 上次提交给窗口的位置与尺寸（未变化时跳过 SetWindowPos，
    /// 否则每帧都会触发一次 DWM 重排与同步 WM_PAINT）
    pub placed: (i32, i32, i32, i32),
}

impl App {
    pub fn window_size(&self) -> (i32, i32) {
        let a = self.anims.open.v.clamp(0.0, 1.0);
        // 基础尺寸：胶囊 ↔ 面板
        let bw = ui::PILL_W as f32 + a * (ui::PANEL_W - ui::PILL_W) as f32;
        let bh = ui::PILL_H as f32 + a * (ui::PANEL_H - ui::PILL_H) as f32;
        // 小黑条：从当前尺寸继续收成迷你黑条（交叉过渡，无跳变）
        let m = self.anims.mini.v.clamp(0.0, 1.0);
        let w = bw + m * (ui::SLIVER_W as f32 - bw);
        let h = bh + m * (ui::SLIVER_H as f32 - bh);
        (render::si(w).max(1), render::si(h).max(1))
    }

    /// 面板是否应当展开：常驻面板恒为真，收起态只由悬停临时展开
    fn want_open(&self) -> bool {
        // 下拉面板只由「点胶囊 / 托盘」控制开关。提醒不再强行展开面板：
        // 提醒只是让胶囊常亮，用户想看详情自己点胶囊
        self.panel_open || self.pinned
    }

    /// 当前展示 & 临近提醒共用的日程：本地手动录入（当天）+ 外部 ICS/CalDAV 订阅。
    /// 本地带日期，只取日期==今天的条目的时间参与排序与提醒；
    /// 外部订阅本身只有今天的条目。
    pub fn merged_schedule(&self) -> Vec<crate::store::ScheduleItem> {
        if !self.cfg.modules.schedule {
            return Vec::new();
        }
        let today = clock::today_str(&clock::local_time());
        let mut v = Vec::new();
        // 本地手动录入的日程：只在「它的日期就是今天」时出现在面板与提醒
        for it in self.store.local_schedules.iter() {
            if it.date == today {
                v.push(crate::store::ScheduleItem {
                    time: it.time.clone(),
                    title: it.title.clone(),
                    remind_min: it.remind_min,
                });
            }
        }
        // 外部订阅（ICS / CalDAV）
        v.extend(crate::modules::sched_ics::today_items());
        v.sort_by(|a, b| a.time.cmp(&b.time));
        v.dedup_by(|a, b| a.time == b.time && a.title == b.title);
        v
    }

    /// 与 `merged_schedule` 相同的合并结果，但为每一行额外附带「它是否是本地手动条目，
    /// 若是则对应的 store.local_schedules 下标」。外部订阅条目对应 None。
    /// 排序 / 去重会把本地下标打乱，因此渲染删除按钮时必须用这里的下标，
    /// 而不能拿合并后的行序去删 local_schedules。
    pub fn merged_schedule_local(&self) -> Vec<(crate::store::ScheduleItem, Option<usize>)> {
        if !self.cfg.modules.schedule {
            return Vec::new();
        }
        let today = clock::today_str(&clock::local_time());
        let mut rows: Vec<(crate::store::ScheduleItem, usize)> = Vec::new();
        for (idx, it) in self.store.local_schedules.iter().enumerate() {
            if it.date == today {
                rows.push((
                    crate::store::ScheduleItem {
                        time: it.time.clone(),
                        title: it.title.clone(),
                        remind_min: it.remind_min,
                    },
                    idx,
                ));
            }
        }
        rows.extend(crate::modules::sched_ics::today_items().into_iter().map(|s| (s, usize::MAX)));
        // 仅以时间+标题代表「外包下标」的占位与本地行区分；MAX 表示外部条目
        rows.sort_by(|a, b| a.0.time.cmp(&b.0.time).then(a.1.cmp(&b.1)));
        let mut merged: Vec<(crate::store::ScheduleItem, Option<usize>)> = Vec::new();
        for (item, idx) in rows {
            if idx == usize::MAX {
                // 外部条目：与已并入的本地/外部行做去重，重复则跳过
                let dup = merged.iter().any(|(m, _)| m.time == item.time && m.title == item.title);
                if !dup {
                    merged.push((item, None));
                }
            } else {
                // 本地条目：与已并入行去重；若与外部重复，优先保留本地（带可删除下标）
                let dup = merged.iter().any(|(m, _)| m.time == item.time && m.title == item.title);
                if !dup {
                    merged.push((item, Some(idx)));
                }
            }
        }
        merged
    }

    /// 把 vis / hover 状态落到 expanded 上，并启动一次过渡
    fn apply_vis(&mut self) {
        let open = self.want_open();
        if open == self.expanded {
            return;
        }
        self.expanded = open;
        if open {
            // 展开时刷新一次本地数据（待办可能刚在文件里改过）
            self.store = Store::load();
            let n = ui::effective_pages(&self.cfg).len().max(1);
            self.page = self.page.min(n - 1);
        }
        self.anim_continue();
    }
}

// ---------------- 屏幕几何 ----------------

/// 横条（收起态）的屏幕矩形：固定屏幕顶部居中
fn pill_rect() -> (i32, i32, i32, i32) {
    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let w = render::si(ui::PILL_W as f32);
    ((sw - w) / 2, TOP_Y, w, render::si(ui::PILL_H as f32))
}

/// 面板（展开态）的屏幕矩形：与横条同宽同轴，向下展开
fn panel_rect() -> (i32, i32, i32, i32) {
    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let pw = render::si(ui::PILL_W as f32);
    let w = render::si(ui::PANEL_W as f32);
    let x = ((sw - pw) / 2 + pw / 2 - w / 2).clamp(0, (sw - w).max(0));
    (x, TOP_Y, w, render::si(ui::PANEL_H as f32))
}

/// 小黑条（遮挡收起态）的屏幕矩形：固定屏幕顶部居中
fn sliver_rect() -> (i32, i32, i32, i32) {
    let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let w = render::si(ui::SLIVER_W as f32);
    ((sw - w) / 2, TOP_Y, w, render::si(ui::SLIVER_H as f32))
}

/// 遮挡判定：主面板胶囊本身（430×58），左右各收 10px
fn pill_area() -> (i32, i32, i32, i32) {
    let (x, y, w, h) = pill_rect();
    let inset = render::si(10.0);
    (x + inset, y, (w - inset * 2).max(24), h)
}

/// 遮挡判定：下拉面板本身（430×254），左右各收 10px、底部收 8px
fn panel_area() -> (i32, i32, i32, i32) {
    let (x, y, w, h) = panel_rect();
    let ix = render::si(10.0);
    let iy = render::si(8.0);
    (x + ix, y, (w - ix * 2).max(24), (h - iy).max(24))
}

/// 当前该拿哪个形态去判距离：下拉面板开着就判下拉面板，
/// 只有主面板胶囊时判胶囊 —— 不能拿看不见的下拉区域去判，
/// 那样窗口离胶囊还很远就会被判成遮挡。
///
/// 注意：曾经在这里额外用一个 `occ_watch_panel` 标记，
/// 「因下拉面板被压而收起后，要等整个面板区域空出来才恢复」。
/// 这会造成死锁：只要有个窗口长期压着面板底部几十像素（比如聊天窗口），
/// 而胶囊区域明明空着，灵动岛也会永远停在小黑条上，再也恢复不了。
/// 面板收起后 `panel_open` 已经清零、不会自动重开，所以按当前形态判就够了，
/// 不存在「收起→弹开→又收起」的来回闪。
fn occ_area(app: &App) -> (i32, i32, i32, i32) {
    if app.expanded {
        panel_area()
    } else {
        pill_area()
    }
}

/// 鼠标是否落在该矩形内
fn cursor_in(x: i32, y: i32, w: i32, h: i32) -> bool {
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return false;
    }
    p.x >= x && p.x < x + w && p.y >= y && p.y < y + h
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub fn run() {
    unsafe {
        // DPI 感知
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // 渲染缩放用主显示器 DPI
        let hdc = windows::Win32::Graphics::Gdi::GetDC(None);
        let dpi = windows::Win32::Graphics::Gdi::GetDeviceCaps(Some(hdc), windows::Win32::Graphics::Gdi::LOGPIXELSX);
        windows::Win32::Graphics::Gdi::ReleaseDC(None, hdc);
        let _ = render::SCALE.set((dpi.max(96) as f32) / 96.0);

        // --preview：离屏渲染每个面板页到 BMP（UI 开发用，渲染完退出）
        if std::env::args().any(|a| a == "--preview") {
            preview_shots();
            return;
        }
        // --preview-panel：展开面板各页 + 剪贴板页滚动态（验证滚轮滚动）
        if std::env::args().any(|a| a == "--preview-panel") {
            preview_panel();
            return;
        }
        // --preview-anim：逐帧导出展开/翻页动画中间帧
        if std::env::args().any(|a| a == "--preview-anim") {
            preview_anim();
            return;
        }
        // --preview-wave：胶囊图标逐帧导图，核对律动
        if std::env::args().any(|a| a == "--preview-wave") {
            preview_wave();
            return;
        }
        // --preview-icons：图标 × 3 档能量的放大对照图
        if std::env::args().any(|a| a == "--preview-pill") {
            preview_pill();
            return;
        }
        if std::env::args().any(|a| a == "--preview-icons") {
            preview_icons();
            return;
        }
        // --preview-keys：键盘态「随机选键」核对图（一次只亮一颗）
        if std::env::args().any(|a| a == "--preview-keys") {
            preview_keys();
            return;
        }
        // --preview-reminder：三种提醒各导一张面板图（UI 开发用）
        if std::env::args().any(|a| a == "--preview-reminder") {
            preview_reminder();
            return;
        }
        // --preview-settings：设置页逐页离屏导图（UI 开发用）
        if std::env::args().any(|a| a == "--preview-settings") {
            crate::settings::preview_pages();
            return;
        }
        // --sync-holidays：只跑一次节假日同步，打印结果后退出（排查联网问题用）
        if std::env::args().any(|a| a == "--sync-holidays") {
            workday::init(&crate::config::Config::load().salary);
            loop {
                if workday::sync_state() != workday::SYNC_RUN && workday::sync_state() != workday::SYNC_IDLE {
                    println!("state={} note={}", workday::sync_state(), workday::sync_note());
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            println!("cache={}", crate::config::data_dir().join("holidays.json").display());
            return;
        }
        // --occ [x y w h]：打印一次遮挡判定结果（排查「明明没窗口却收起了」用）
        if std::env::args().any(|a| a == "--occ") {
            let a: Vec<i32> = std::env::args()
                .skip_while(|s| s != "--occ")
                .skip(1)
                .filter_map(|s| s.parse().ok())
                .collect();
            let (x, y, w, h) = if a.len() >= 4 {
                (a[0], a[1], a[2], a[3])
            } else {
                pill_area()
            };
            let txt = match occlusion::blocker(x, y, w, h) {
                Some(hit) => format!("covered=true\nblocker={}\nrect=({x},{y}) {w}x{h}\n", occlusion::describe(hit)),
                None => format!("covered=false\nrect=({x},{y}) {w}x{h}\n"),
            };
            let txt = format!(
                "{txt}appdata={}\nauto_collapse={}\n",
                std::env::var("APPDATA").unwrap_or_default(),
                crate::config::Config::load().auto_collapse
            );
            println!("{txt}");
            let _ = std::fs::write(std::env::temp_dir().join("winisland_occ.txt"), &txt);
            return;
        }

        let hinst = GetModuleHandleW(None).unwrap();

        let class_name = w!("WinIsland");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
            hIcon: crate::modules::tray::app_icon(),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let cfg = Config::load();
        crate::theme::init(&cfg.theme);
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let w = render::si(ui::PILL_W as f32);
        let h = render::si(ui::PILL_H as f32);
        // 胶囊固定在屏幕顶部居中（不可自定义、不可拖动）
        let x = (sw - w) / 2;
        let y = 8;

        let hwnd = CreateWindowExW(
            if cfg.glassy() {
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
            } else {
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
            },
            class_name,
            w!("WinIsland"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW");

        let mail_shared: mail::SharedMail = std::sync::Arc::new(std::sync::Mutex::new(mail::MailShared::default()));
        mail::spawn(mail_shared.clone());
        audio::spawn(); // 系统音量峰值采样（独立线程，只取 0..1 标量）
        workday::init(&cfg.salary); // 读本地节假日缓存，必要时后台联网同步
        crate::modules::sched_ics::init(); // 外部日历（ICS 订阅）缓存
        crate::modules::stock::spawn(); // 盯盘助手：后台拉取自选行情

        let st = clock::local_time();
        let mut store = Store::load();
        store.reset_day_if_needed(&clock::today_str(&st));
        let now_unix = files::now_unix();
        // 序号基线固定 0：邮件线程只在「启动后新到的验证码」时才 seq+1，
        // 启动前的旧邮件由邮件线程的时间基准挡住，不会走到这里。
        let last_mail_seq = 0u64;

        // 毛玻璃（Mica/Acrylic）：DirectComposition 链路。失败则回退到分层自绘，
        // 此时窗口需重新带上 WS_EX_LAYERED（创建时按配置去掉了它）。
        let mut glassy = None;
        if cfg.glassy() {
            match crate::modules::glassy::Glassy::enable(hwnd, w, h) {
                Ok(g) => {
                    diag_log("glassy: DirectComposition 毛玻璃已启用");
                    glassy = Some(g);
                }
                Err(e) => {
                    diag_log(&format!("glassy: 启用失败，回退分层自绘: {e}"));
                    ensure_layered_style(hwnd);
                }
            }
        }

        APP.with(|a| {
            *a.borrow_mut() = Some(App {
                hwnd,
                canvas: None,
                glassy,
                cfg: cfg.clone(),
                store,
                sal: salary::SalaryCalc::new(&cfg.salary),
                expanded: false,
                panel_open: false,
                pinned: false,
                vis: Vis::Dock,
                pill_hover: false,
                hover_latch: false,
                occluded: false,
                occ_next: Instant::now(),
                leave_at: None,
                anims: Anims::new(),
                px: x,
                py: y,
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
                mouse: POINT { x: 0, y: 0 },
                hover: None,
                drag_hover: false,
                reminder: None,
                last_water_unix: now_unix,
                last_sed_unix: now_unix,
                code_until: None,
                code_pending: false,
                last_mail_seq,
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
                mail_shared,
                hidden: false,
                placed: (99_999, 99_999, 0, 0),
            })
        });

        // OLE 拖放必须在注册线程调用 OleInitialize（它比 CoInitializeEx 多初始化
        // OLE 拖放/剪贴板所需的设施）。主线程此前从未初始化 COM，
        // 导致 RegisterDragDrop 静默失败、光标显示"禁止"、文件根本拖不进来。
        // 注意：audio.rs 的 CoInitializeEx(MTA) 在独立采样线程，与此不冲突。
        match windows::Win32::System::Ole::OleInitialize(None) {
            Ok(()) => diag_log("OleInitialize: OK"),
            Err(e) => diag_log(&format!("OleInitialize: 失败 {:?}", e)),
        }

        DragAcceptFiles(hwnd, false); // 我们使用 OLE 拖放
        if let Err(e) = ui::register_ole_drop(hwnd) {
            diag_log(&format!("RegisterDragDrop 失败: {:?}", e));
        } else {
            diag_log("RegisterDragDrop: OK");
        }

        // 构建自报：日志里必须能看出「跑的是哪个 exe、哪个版本、哪个进程」。
        // 排查拖放问题时多次出现「日志里没有任何新埋点」的情况，根因往往
        // 是运行的仍是旧 exe —— 这一行让二进制身份可以被直接核对。
        {
            let exe = std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            diag_log(&format!(
                "BOOT v{} pid={} exe={}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                exe
            ));
        }

        // 启动时先判一次遮挡：面板区域已被窗口压住就直接以小黑条出现，
        // 不做「先弹出来再缩回去」的闪动
        APP.with(|a| {
            if let Some(app) = a.borrow_mut().as_mut() {
                app.occ_next = Instant::now();
                occ_check(app);
                if app.occluded {
                    app.anims.mini.set(1.0);
                }
            }
        });

        // 键盘钩子
        let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0).expect("SetWindowsHookExW");
        HOOK_HWND.store(hwnd.0 as isize, std::sync::atomic::Ordering::Relaxed);

        AddClipboardFormatListener(hwnd).ok();

        tray::add(hwnd); // 托盘图标

        // 帧时钟不再依赖 WM_TIMER：Windows 默认定时器粒度 ~15.6ms 且 WM_TIMER 是
        // 最低优先级消息，实际帧率会在 30~45fps 之间抖动 —— 这才是"不够丝滑"的主因。
        // 改为消息循环里的 vsync 驱动（DwmFlush 与 DWM 合成对齐）。
        let _ = timeBeginPeriod(1);
        SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // 首帧
        redraw();

        // --settings：启动后直接打开设置窗口（便于做快捷方式 / 调试）
        if std::env::args().any(|a| a == "--settings") {
            crate::settings::open();
        }

        // 桌面分区：按配置创建毛玻璃分区窗口（壁纸之上、图标之下）
        crate::modules::zones::sync();

        let mut msg = MSG::default();
        // --probe：自动点开一次面板并采样真实帧间隔（性能诊断用）
        let probe = std::env::args().any(|a| a == "--probe");
        if probe {
            PROBE_T0.with(|c| c.set(Some(Instant::now())));
        }
        'run: loop {
            if probe {
                if !PROBE_STARTED.with(|c| c.get()) {
                    let due = PROBE_T0.with(|c| c.get().is_some_and(|t| t.elapsed() >= Duration::from_millis(250)));
                    if due {
                        PROBE_STARTED.with(|c| c.set(true));
                        PROBE_T0.with(|c| c.set(Some(Instant::now())));
                        click(&HitId::Pill);
                    }
                } else if PROBE_T0.with(|c| c.get().is_some_and(|t| t.elapsed() > Duration::from_millis(1200))) {
                    PROBE_FRAMES.with(|v| {
                        let v = v.borrow();
                        let mut sum = 0.0f32;
                        let mut mx = 0.0f32;
                        for ms in v.iter() {
                            sum += *ms;
                            mx = mx.max(*ms);
                        }
                        let avg = sum / v.len().max(1) as f32;
                        let txt: Vec<String> = v.iter().map(|m| format!("{:.1}", m)).collect();
                        let line = format!(
                            "frames={} avg={:.2}ms ({:.0}fps) max={:.1}ms\nseq {}\n",
                            v.len(),
                            avg,
                            1000.0 / avg.max(0.001),
                            mx,
                            txt.join(",")
                        );
                        let _ = std::fs::write(std::env::temp_dir().join("winisland_probe.txt"), line);
                    });
                    break 'run;
                }
            }
            if frame_wanted() {
                // 先把消息排空，动画期间点击/悬停不会被阻塞
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        break 'run;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                anim_step();
                // 阻塞到下一次垂直回扫，出帧节奏与显示器刷新率一致（不撕裂、不抖）。
                // 即使本帧被空闲节流跳过绘制，也要在这里对齐 vsync，
                // 否则会退化成不休眠的忙循环吃满 CPU。
                let t0 = Instant::now();
                let _ = DwmFlush();
                // 合成被禁用（远程桌面/全屏独占）时 DwmFlush 立即返回，防止空转
                if t0.elapsed() < Duration::from_millis(2) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            } else {
                if probe {
                    // 探针模式不能阻塞在 GetMessage 里，否则等不到触发时刻
                    if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                } else if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        let _ = timeEndPeriod(1);
        UnhookWindowsHookEx(hook).ok();
        save_now();
        let _ = crate::ui::revoke_ole_drop(hwnd);
    }
}

/// 是否要继续出帧。
/// 胶囊图标要「一直律动」、金额要按毫秒连续跳动，所以窗口可见期间帧循环常驻；
/// 由 `anim_step` 按活跃度节流（空闲 ~30fps，打字/声音/过渡期间跟随 vsync 全速），
/// 隐藏后彻底停帧，退回 GetMessageW 阻塞，不占 CPU。
fn frame_wanted() -> bool {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        match b.as_mut() {
            Some(app) => {
                // 先把界面状态同步成弹簧目标，否则 busy() 永远是 false
                sync_targets(app);
                !app.hidden
            }
            None => false,
        }
    })
}

pub fn save_now() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.store.save_preserving_mail();
            app.cfg.save();
            app.dirty = false;
        }
    });
}

/// 设置窗口点「同步」后调用：用最新配置重建运行时数据并刷新面板。
/// - SalaryCalc 是配置快照，月薪 / 作息改动必须重建才生效
/// - 工作日判定与节假日缓存按最新配置重新加载，并后台拉取一次官方数据
pub fn reload_runtime() {
    workday::init(&with_cfg(|c| c.salary.clone()));
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.sal = salary::SalaryCalc::new(&app.cfg.salary);
            app.dirty = false;
        }
    });
    redraw();
}

/// 设置页调整中转站上限：写入配置，并立即裁掉超出的旧条目（保持最新在前）。
/// 返回裁剪掉的条数，供界面提示。
pub fn set_filehub_cap(cap: usize) -> usize {
    let cap = cap.clamp(1, 200);
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return 0 };
        app.cfg.filehub_cap = cap;
        let before = app.store.files.len();
        if app.store.files.len() > cap {
            app.store.files.truncate(cap);
        }
        let removed = before.saturating_sub(app.store.files.len());
        if removed > 0 {
            app.dirty = true;
        }
        removed
    })
}

/// 供设置窗口读写配置（同线程调用）
pub fn with_cfg<R>(f: impl FnOnce(&mut Config) -> R) -> R {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = b.as_mut().expect("[winisland] app 未初始化");
        f(&mut app.cfg)
    })
}

/// 面板新增待办（由待办输入对话框回调）：写入 store 并保存、重绘。
/// 新待办追加到末尾，未完成优先排序由渲染端处理。
pub fn push_todo(title: &str) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        // 上限保护：待办无界增长会拖垮落盘与渲染，超限时丢弃最旧项
        const CAP: usize = 200;
        if app.store.todos.len() >= CAP {
            let drop = app.store.todos.len() + 1 - CAP;
            app.store.todos.drain(..drop);
        }
        app.store.todos.push(crate::store::TodoItem {
            title: title.to_string(),
            done: false,
        });
        app.dirty = true;
        app.save_now_quiet();
    });
}

/// 面板新增本地手动日程（由新增日程对话框回调）：写入 store 并保存、重绘。
pub fn push_schedule(date: &str, time: &str, title: &str, remind_min: u32) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        // 上限保护：本地日程无界增长会拖垮落盘与渲染，超限时丢弃最旧项
        const CAP: usize = 200;
        if app.store.local_schedules.len() >= CAP {
            let drop = app.store.local_schedules.len() + 1 - CAP;
            app.store.local_schedules.drain(..drop);
        }
        app.store.local_schedules.push(crate::store::LocalSchedule {
            date: date.to_string(),
            time: time.to_string(),
            title: title.to_string(),
            remind_min,
        });
        app.dirty = true;
        app.save_now_quiet();
    });
}


/// 面板删除待办（行尾 ×）在 click 中直接处理，这里保留为对外 API。
#[allow(dead_code)]
pub fn remove_todo(index: usize) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        if index < app.store.todos.len() {
            app.store.todos.remove(index);
            app.dirty = true;
            app.save_now_quiet();
        }
    });
}

impl App {
    /// 待办数据有变：立即落盘（不必等 2s 节流）+ 触痛重绘
    fn save_now_quiet(&mut self) {
        self.store.save_preserving_mail();
        self.last_save = Instant::now();
    }
}

/// 调试用：把离屏预览的 App 装进 APP（--preview-settings 需要读配置）
pub(crate) fn install_preview_app(demo: bool) {
    let app = preview::preview_app(demo);
    APP.with(|a| *a.borrow_mut() = Some(app));
}

// ---------------- 托盘动作 ----------------

/// 托盘左键：切换展开；若当前隐藏则先恢复显示
pub fn tray_click() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        if app.hidden {
            app.hidden = false;
            unsafe {
                let _ = ShowWindow(app.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        // 托盘左键：在「主面板胶囊 ↔ 下拉面板」之间切换
        if app.panel_open || app.pinned {
            app.panel_open = false;
            app.pinned = false;
        } else {
            app.panel_open = true;
            app.pill_hover = false;
            app.page = 0;
            app.anims.page.set(1.0);
            app.anims.page_from = 0;
            app.scroll = 0.0;
        }
        app.leave_at = None;
        app.apply_vis();
        drop(b);
        redraw();
    });
}

/// 切换胶囊模式（托盘右键菜单）。
/// `work` 工作模式：右侧轮播今日已赚 / 已工作 / 进度 / 距下班 / 打字。
/// `stock` 盯盘模式：左侧状态显示「盯盘中」，右侧轮播每只自选的股价与涨跌幅 + 今日盈亏。
///
/// 两种模式共用同一套胶囊布局，只换状态徽标与轮播内容，所以切换只需重置轮播索引、
/// 存盘并重绘；切入盯盘模式时顺带对自选补拉一次行情，切过去立刻有数。
pub fn set_mode(mode: &str) {
    let changed = APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return false };
        if app.cfg.mode == mode {
            return false;
        }
        app.cfg.mode = mode.to_string();
        // 选了盯盘模式却关着盯盘模块时自动开启：否则胶囊在播股价、面板却没有盯盘页，前后割裂
        if mode == "stock" && !app.cfg.modules.stock {
            app.cfg.modules.stock = true;
        }
        // 两种模式的轮播条目数不同（工作 5 条 / 盯盘 = 自选数 + 今日盈亏），
        // 索引归零，避免取模后落到不存在的条目上
        app.rot_i = 0;
        app.rot_prev = 0;
        app.rot_next = Instant::now() + Duration::from_millis(4200);
        app.anims.rot.set(1.0);
        true
    });
    if !changed {
        return;
    }
    if let Some(cfg) = APP.with(|a| a.borrow().as_ref().map(|x| x.cfg.clone())) {
        cfg.save();
    }
    // 切到盯盘模式：快照里没有的代码异步补拉一次（内部自带判重，不会重复发请求）
    if mode == "stock" {
        let snap = crate::modules::stock::snapshot();
        for it in crate::modules::stock::read_items() {
            if crate::modules::stock::quote_for(&snap, &it.code).is_none() {
                crate::modules::stock::request_lookup(&it.code);
            }
        }
    }
    redraw();
}

pub fn toggle_hidden() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        app.hidden = !app.hidden;
        unsafe {
            let _ = ShowWindow(app.hwnd, if app.hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
        if !app.hidden {
            drop(b);
            redraw();
        }
    });
}

pub fn quit() {
    save_now();
    // 桌面分区窗口随主线程消息循环一起销毁
    crate::modules::zones::shutdown();
    // 设置窗口可能开着：先同步藏掉（顺带保存当前页输入），退出后不残留窗口
    crate::settings::close_for_quit();
    // 模态弹窗（编辑自选 / 新增待办 / 新增日程）可能开着：主动关掉，避免退出后残留幽灵窗口
    crate::modules::watch_edit::close_for_quit();
    crate::modules::todo_add::close_for_quit();
    crate::modules::schedule_add::close_for_quit();
    // 看门狗兜底：正常收尾（销毁窗口 → 存盘 → 退出消息循环）应在 1-2 秒内结束。
    // 若主线程在收尾阶段挂死（曾出现设置窗口开着时退出后卡住），3 秒后强制结束进程，
    // 不在屏幕上留"未响应"的幽灵窗口。数据已在 save_now 落盘。
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        std::process::exit(0);
    });
    APP.with(|a| {
        let mut b = a.borrow_mut();
        if let Some(app) = b.as_mut() {
            unsafe {
                let _ = PostMessageW(Some(app.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    });
}

/// 键盘钩子唤醒消息循环用（钩子回调里不能碰 RefCell，这里是纯原子读写）
static HOOK_HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

thread_local! {
    static REDRAWING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };    /// 设置窗口打开期间挂起置顶，避免灵动岛盖住设置界面
    static TOPMOST_SUSPENDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 置顶兜底检查的节流时间点
    static TOPMOST_CHECK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    // ---- --probe 性能诊断用（正常运行时只是几个空 Cell）----
    static PROBE_T0: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static PROBE_STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PROBE_FRAMES: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

/// 供设置窗口调用：设置窗口显示时把灵动岛降为普通窗口，关闭后恢复置顶
pub fn set_island_topmost(on: bool) {
    TOPMOST_SUSPENDED.with(|c| c.set(!on));
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            unsafe {
                let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
                let _ = SetWindowPos(app.hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    });
}

/// 兜底：把置顶标志补回来。
///
/// WS_EX_TOPMOST 可能被系统或其它操作剥掉（设置窗最小化、explorer 重启、
/// 显示模式/DPI 切换、全屏应用获得焦点等）。一旦丢了，灵动岛连同收起态的
/// 小黑条都会被普通窗口压住，看上去就是「岛消失了」。这里每秒查一次，
/// 丢了就重新置顶；设置窗口打开期间（TOPMOST_SUSPENDED）有意不动。
/// 另外 redraw 里只有位置尺寸变化才会重新置顶，所以必须由这里兜底。
fn ensure_topmost(app: &App) {
    // 提醒期间就算设置窗活跃也不让位，否则全屏应用下会看不到提醒
    if app.hidden || (TOPMOST_SUSPENDED.with(|c| c.get()) && app.reminder.is_none()) {
        return;
    }
    let now = Instant::now();
    if let Some(t) = TOPMOST_CHECK.with(|c| c.get()) {
        if now < t {
            return;
        }
    }
    TOPMOST_CHECK.with(|c| c.set(Some(now + Duration::from_millis(1000))));
    unsafe {
        let ex = GetWindowLongW(app.hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOPMOST.0 == 0 && IsWindowVisible(app.hwnd).as_bool() {
            let _ = SetWindowPos(
                app.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// 诊断日志：追加到 %APPDATA%\WinIsland\drag.log（GUI 程序无控制台）。
/// 用于排查拖放问题，仅在拖放相关路径调用，平时不产生噪音。
pub fn diag_log(msg: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir().join("drag.log"))
        .map(|mut f| {
            use std::io::Write;
            let _ = writeln!(f, "[{}] {}", files::now_unix(), msg);
        });
}

/// 给窗口补上 WS_EX_LAYERED（毛玻璃启用失败/渲染失败回退时用）。
/// 毛玻璃路径的窗口在创建时去掉了该样式；回退到分层自绘前必须加回。
fn ensure_layered_style(hwnd: HWND) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex & (WS_EX_LAYERED.0 as isize) == 0 {
            let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | (WS_EX_LAYERED.0 as isize));
        }
    }
}

pub fn redraw() {
    // SetWindowPos 会同步派发 WM_PAINT，避免重入
    if REDRAWING.with(|f| f.get()) {
        return;
    }
    REDRAWING.with(|f| f.set(true));
    let _guard = Guard;
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            REDRAWING.with(|f| f.set(false));
        }
    }
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        let (w, h) = app.window_size();
        if app.hidden {
            return;
        }
        // 面板以胶囊水平中心为中轴；胶囊固定屏幕顶部居中
        let sw = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let pill_w = render::si(ui::PILL_W as f32);
        let pill_x = (sw - pill_w) / 2;
        app.px = (pill_x + pill_w / 2 - w / 2).clamp(0, (sw - w).max(0));
        // 位置尺寸没变就不碰窗口：SetWindowPos 每帧调用会拖慢合成，
        // 且会同步派发一次 WM_PAINT（触发额外的重入检查）
        let want = (app.px, app.py, w, h);
        if app.placed != want {
            unsafe {
                let suspended = TOPMOST_SUSPENDED.with(|c| c.get());
                let flags = if suspended { SWP_NOZORDER | SWP_NOACTIVATE } else { SWP_NOACTIVATE };
                let _ = SetWindowPos(app.hwnd, Some(HWND_TOPMOST), app.px, app.py, w, h, flags);
            }
            app.placed = want;
        }
        if app.canvas.is_none() {
            match Canvas::new() {
                Ok(c) => app.canvas = Some(c),
                Err(e) => {
                    eprintln!("[winisland] 渲染初始化失败: {e}");
                    return;
                }
            }
        }
        let mut canvas = match app.canvas.take() {
            Some(c) => c,
            None => return,
        };
        let glassy = app.glassy.take();
        let mut hits: Vec<ui::Hit> = Vec::new();
        let mut scroll_max = 0.0f32;
        let hwnd = app.hwnd;
        let mut fell_back = false;
        // 当前帧还存活、需要恢复进 App 的毛玻璃链路（此作用域内不可动 app 自身）
        let mut keep_glassy: Option<crate::modules::glassy::Glassy> = None;
        {
            let app_ref = &*app;
            match glassy {
                Some(mut g) => {
                    let r = canvas.present_glassy(&mut g, w, h, |cv, cw, ch| {
                        scroll_max = ui::draw_frame(app_ref, cv, cw, ch, &mut hits);
                    });
                    match r {
                        Ok(()) => keep_glassy = Some(g),
                        Err(e) => {
                            let dev_lost = e
                                .as_ref()
                                .downcast_ref::<windows::core::Error>()
                                .is_some_and(crate::modules::glassy::Glassy::is_device_lost);
                            if dev_lost {
                                // GPU 设备丢失：整条 D3D11/DComp 链已失效，重试无意义，
                                // 立即重建链路；成功则下一帧恢复毛玻璃，失败再回退分层。
                                diag_log(&format!("glassy: 设备丢失({e})，重建 DirectComposition 链路"));
                                match crate::modules::glassy::Glassy::enable(hwnd, w, h) {
                                    Ok(g2) => keep_glassy = Some(g2),
                                    Err(e2) => {
                                        diag_log(&format!("glassy: 重建失败，回退分层自绘: {e2}"));
                                        fell_back = true;
                                    }
                                }
                            } else {
                                diag_log(&format!("glassy: 渲染失败，回退分层自绘: {e}"));
                                fell_back = true;
                            }
                        }
                    }
                }
                None => {
                    canvas.present(hwnd, w, h, |cv, cw, ch| {
                        scroll_max = ui::draw_frame(app_ref, cv, cw, ch, &mut hits);
                    });
                }
            }
        }
        if fell_back {
            // 回退：补回分层样式后按普通分层自绘重画一次
            ensure_layered_style(hwnd);
            let app_ref = &*app;
            hits.clear();
            scroll_max = 0.0;
            canvas.present(hwnd, w, h, |cv, cw, ch| {
                scroll_max = ui::draw_frame(app_ref, cv, cw, ch, &mut hits);
            });
        }
        app.canvas = Some(canvas);
        app.glassy = keep_glassy;
        app.hits = hits;
        app.scroll_max = scroll_max;
        app.scroll = app.scroll.clamp(0.0, scroll_max);
    });
}

unsafe extern "system" fn hook_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode >= 0 {
        let is_down = (wparam.0 as u32) == 0x0100 /*WM_KEYDOWN*/ || (wparam.0 as u32) == 0x0104 /*WM_SYSKEYDOWN*/;
        if is_down {
            // 忽略纯修饰键
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            let vk = kb.vkCode;
            let is_modifier = matches!(vk, 0x10 | 0x11 | 0x14 | 0xA0..=0xA5 | 0x5B | 0x5C);
            if !is_modifier {
                typing::bump_key();
                // 唤醒消息循环：让波形动画立刻开始律动，
                // 否则要等下一次 1 秒定时器才出帧
                let h = HOOK_HWND.load(std::sync::atomic::Ordering::Relaxed);
                if h != 0 {
                    let _ = PostMessageW(Some(HWND(h as *mut _)), WM_NULL, WPARAM(0), LPARAM(0));
                }
            }
        }
    }
    CallNextHookEx(None, ncode, wparam, lparam)
}

/// 外部（OLE 拖放回调，运行在主线程消息循环）推入文件。
/// 容量上限由设置里的 filehub_cap 决定；满额时拒绝新文件并提示，不淘汰已有条目。
pub fn push_file_external(path: String) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else {
            diag_log("PUSH_FILE APP=None");
            return;
        };
        if !app.cfg.modules.filehub {
            // 静默 return 会让用户以为「拖不进去」，必须给可见反馈
            diag_log("PUSH_FILE filehub 模块关闭");
            app.flash = Some(Flash::new("文件中转站未开启，请在设置中打开", 2200));
            drop(b);
            redraw();
            return;
        }
        let cap = app.cfg.filehub_cap;
        let result = files::try_push_file(&mut app.store.files, &path, cap);
        diag_log(&format!("PUSH_FILE path={} result={:?} files_now={}", path, result, app.store.files.len()));
        let msg = match result {
            files::PushResult::Added => Some("已暂存到中转站"),
            files::PushResult::Refreshed => Some("已更新（原已暂存）"),
            files::PushResult::Full => Some("中转站已满，请先移除或调大上限"),
        };
        if result != files::PushResult::Full {
            app.dirty = true;
        }
        app.flash = Some(Flash::new(msg.unwrap_or(""), 1600));
        drop(b);
        redraw();
    });
}

/// OLE 拖动文件进入/离开岛屿：设置高亮态，并在进入时自动展开面板、
/// 展开面板并切到「文件中转站」页。
/// 分区内图标拖到主面板暂存文件后调用，让用户立刻看到暂存位置。
/// 不改动 drag_hover 标志（那是 OLE 拖放专用的状态）。
pub fn focus_filehub() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        if !app.cfg.modules.filehub {
            return;
        }
        let pages = ui::effective_pages(&app.cfg);
        match pages.iter().position(|p| *p == "files") {
            Some(idx) => {
                if idx != app.page {
                    go_page(app, idx);
                }
            }
            None => {
                // 模块开着但「文件中转站」页被关掉：文件仍会入站，
                // 但用户看不到页面 —— 明确提示去哪儿打开，避免以为没成功。
                app.flash = Some(Flash::new("「文件中转站」页已关闭，请在设置中开启", 2600));
            }
        }
        app.panel_open = true;
        app.apply_vis();
        drop(b);
        redraw();
    });
}

/// OLE 拖动文件进入/离开岛屿：设置高亮态，并在进入时自动展开面板、
/// 切到「文件中转站」页，方便用户看到暂存位置。
/// enter=true 为 DragEnter/DragOver，false 为 DragLeave/Drop。
pub fn set_drag_hover(enter: bool) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let Some(app) = b.as_mut() else { return };
        if !app.cfg.modules.filehub {
            return;
        }
        if enter {
            if app.drag_hover {
                return; // 已在拖动态，避免重复触发展开
            }
            app.drag_hover = true;
            // 自动展开面板并跳到文件中转站页
            let pages = ui::effective_pages(&app.cfg);
            if let Some(idx) = pages.iter().position(|p| *p == "files") {
                if idx != app.page {
                    go_page(app, idx);
                }
            }
            app.panel_open = true;
            app.apply_vis();
        } else {
            if !app.drag_hover {
                return;
            }
            app.drag_hover = false;
        }
        drop(b);
        redraw();
    });
}

/// 弹出提醒：胶囊常亮显示提醒内容并保持置顶，直到用户点击。
///
/// 要点（用户明确要求）：
/// - 只让**胶囊**（430×58）常亮，不展开成下拉面板；
/// - 不管当前是不是被别的窗口压住（含全屏应用）都要可见：清掉遮挡收起态、
///   解除隐藏、补一次 HWND_TOPMOST，并临时无视设置窗的置顶让位；
/// - 保持到用户点击为止，不做超时自动消失；
/// - 提醒期间遮挡检测不再把它收成小黑条、悬停也不自动展开面板
///   （见 occ_check / hover_tick）。
fn raise_reminder(app: &mut App, kind: ToastKind, msg: String) {
    if app.reminder.is_some() {
        return;
    }
    app.reminder = Some(Toast { kind, msg });

    if app.hidden {
        app.hidden = false;
        unsafe {
            let _ = ShowWindow(app.hwnd, SW_SHOWNOACTIVATE);
        }
    }
    // 不主动展开、也不强制收起：面板开着时提醒显示在面板头部提醒条，
    // 收起时显示在胶囊上，两处都常亮
    app.occluded = false;
    app.vis = Vis::Dock;
    app.pill_hover = false;
    app.leave_at = None;
    app.hover_latch = false;

    app.apply_vis();
    // 全屏应用会覆盖普通窗口，置顶必须重新压一次。
    // 注意：这里不能调 set_island_topmost()——它会再 APP.borrow_mut()，
    // 而调用方（tick_second / 预览）已持有可变借用，RefCell 会直接 panic
    // 而 release 是 panic=abort，表现就是秒退。直接操作窗口。
    TOPMOST_SUSPENDED.with(|c| c.set(false));
    unsafe {
        let _ = SetWindowPos(
            app.hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    app.anim_continue();
}

/// 用户点击胶囊 / 提醒条按钮：清掉提醒并做完对应的记账，然后恢复正常轮播
/// 当前久坐秒数：与喝水一样按时间戳间隔计算，只看真实时间流逝，
/// 连续打字或完全不输入都会照常累计。
pub fn now_sed_secs(last_sed_unix: i64) -> u32 {
    files::now_unix().saturating_sub(last_sed_unix).max(0) as u32
}

/// 久坐提醒文案：直接显示用户设置的久坐阈值，而不是不断上涨的累计时长。
/// 这样提醒里的分钟数和设置页「久坐阈值」始终对得上。
pub fn sed_reminder_msg(sedentary_minutes: u32) -> String {
    format!("久坐 {} 分钟到了 · 该起来走走了", sedentary_minutes)
}

fn ack_reminder(app: &mut App) {
    let Some(r) = app.reminder.take() else { return };
    match r.kind {
        ToastKind::Water => {
            app.store.water_cups += 1;
            app.last_water_unix = files::now_unix();
            app.dirty = true;
        }
        ToastKind::Sedentary => {
            app.last_sed_unix = files::now_unix();
        }
        ToastKind::Schedule => {}
        ToastKind::Stock => {}
    }
    app.leave_at = None;
    app.apply_vis();
}

fn tick_second() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        // 面板已收起时，待办/文件/手动日程的「二次确认删除」态一并失效，避免下次打开误删
        if !app.panel_open {
            app.todo_del_arm = None;
            app.file_del_arm = None;
            app.schedule_del_arm = None;
        }
        // 打字统计入库
        let st = clock::local_time();
        let today = clock::today_str(&st);
        app.store.reset_day_if_needed(&today);
        let add = typing::KEYS_TOTAL.swap(0, std::sync::atomic::Ordering::Relaxed);
        app.store.typing_keys += add;
        typing::tick_second();
        if add > 0 {
            app.dirty = true;
        }

        // 健康提醒
        if app.cfg.modules.health {
            let now_unix = files::now_unix();
            if app.reminder.is_none() {
                if (now_unix - app.last_sed_unix) >= (app.cfg.health.sedentary_minutes as i64) * 60 {
                    raise_reminder(app, ToastKind::Sedentary, String::new());
                } else if (now_unix - app.last_water_unix) >= (app.cfg.health.water_interval_minutes as i64) * 60 {
                    // 先记下这次提醒的时间，避免用户点了确认又立刻重新弹
                    app.last_water_unix = now_unix;
                    raise_reminder(app, ToastKind::Water, String::new());
                }
            }
            // 喝水跨天重置在 reset_day_if_needed
            app.dirty = true;
        }

        // 日程临近 → 胶囊常亮提醒（同一条日程当天只提醒一次）
        if app.cfg.modules.schedule && app.reminder.is_none() {
            let now_min = clock::secs_of_day(&st) / 60;
            let mut due: Option<(String, String)> = None;
            for it in app.merged_schedule().iter() {
                let Some(m) = clock::parse_hm(&it.time) else { continue };
                // 每条日程可配置自己的提前量（remind_min：0 = 回落全局 schedule_lead_minutes）
                let item_lead = if it.remind_min > 0 { it.remind_min as i64 } else { app.cfg.schedule_lead_minutes as i64 };
                if item_lead <= 0 {
                    continue;
                }
                let diff = m - now_min;
                if (0..=item_lead).contains(&diff) {
                    let key = format!("{}|{}|{}", today, it.time, it.title);
                    if !app.store.sched_seen(&key) {
                        due = Some((key, format!("{} · {}", it.time, it.title)));
                        break;
                    }
                }
            }
            if let Some((key, msg)) = due {
                // 先记账再弹：多条日程同时在提醒窗口内时，避免 A→B→A 来回弹
                app.store.sched_mark(&key);
                raise_reminder(app, ToastKind::Schedule, msg);
                app.dirty = true;
            }
        }

        // 外部日历（ICS 订阅 / CalDAV）：到刷新间隔就后台拉取；后台完成则触发一次重绘
        if app.cfg.modules.schedule {
            crate::modules::sched_ics::auto_tick(&app.cfg.schedule_sub);
            if crate::modules::sched_ics::take_dirty() {
                app.dirty = true;
            }
        }

        // 邮箱验证码 -> 弹出胶囊（不点复制常驻，点了才 5 秒消失）
        if app.cfg.modules.mailcode {
            // 先读到局部变量，锁立即释放，避免在锁内重复加锁 / 持锁改 App
            let pending = app
                .mail_shared
                .lock()
                .ok()
                .and_then(|s| {
                    if s.seq > app.last_mail_seq {
                        s.latest.clone().map(|mc| (s.seq, mc))
                    } else {
                        None
                    }
                });
            if let Some((seq, _mc)) = pending {
                app.last_mail_seq = seq;
                // 不点复制就一直展示；只有点复制才启动 5 秒倒计时。
                app.code_pending = true;
                app.code_until = None;
                app.code_copied_at = None;
                // 同步给邮件线程写进状态栏
                if let Ok(mut shared) = app.mail_shared.lock() {
                    shared.ui_seq = seq;
                }
            }
        }

        // 盯盘助手：行情刷新 → 重绘当前页；某只涨跌幅超过【它自己设置的】阈值 → 弹一次提醒（当天不重复）
        if app.cfg.modules.stock && crate::modules::stock::take_dirty() {
            app.dirty = true;
            if app.reminder.is_none() {
                if let Some(msg) = crate::modules::stock::next_alert() {
                    raise_reminder(app, ToastKind::Stock, msg);
                }
            }
        }

        // 盯盘页自愈兜底：只要面板停在盯盘页，就把快照里尚缺的自选代码主动补拉。
        // 这样数据不再依赖「启动时机 / 是否交易时段 / 主轮询是否已覆盖」——
        // 之前收盘后（off_hours=false）主轮询不拉，必须点编辑触发按需查询才出数据，
        // 正是缺了这层保证。request_lookup 自带快照判重 + 单代码去重，重复调用不会多发请求。
        if app.cfg.modules.stock && app.panel_open {
            let pages = ui::effective_pages(&app.cfg);
            let idx = app.page.min(pages.len().saturating_sub(1));
            if pages.get(idx).copied() == Some("watch") {
                for it in crate::modules::stock::read_items() {
                    crate::modules::stock::request_lookup(&it.code);
                }
            }
        }
        // 提醒不做自动过期：必须用户点确认按钮才消失（见 click / HitId::ReminderAck）
        // 定时保存
        if app.dirty && app.last_save.elapsed() > Duration::from_secs(2) {
            app.store.save_preserving_mail();
            app.dirty = false;
            app.last_save = Instant::now();
        }

        // 定时重绘（薪资跳动 / 打字峰值 / 波形动画）
        app.peak_wpm = app.peak_wpm.max(typing::wpm_now());
        drop(b);
        redraw();
    });
}

fn on_clipboard_update() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        if !app.cfg.modules.clipboard {
            return;
        }
        if let Some(text) = clipboard::get_text(app.hwnd) {
            let text = text.trim().to_string();
            if text.is_empty() || text.len() > 4096 {
                return;
            }
            if app.store.clips.first().map(|c| c.text == text).unwrap_or(false) {
                return;
            }
            let cap = app.cfg.clipboard_history.clamp(5, 50);
            app.store.clips.retain(|c| c.text != text);
            app.store.clips.insert(0, crate::store::ClipItem { text, ts: files::now_unix() });
            app.store.clips.truncate(cap);
            app.dirty = true;
            drop(b);
            redraw();
        }
    });
}

fn click(h: &HitId) {
    // 「新增待办」在 APP 借用之外处理：模态对话框的消息循环会再次触达主窗口
    // 事件，若此时还持有 APP 借用会重入冲突。
    if let HitId::TodoAdd = h {
        let res = crate::modules::todo_add::prompt();
        if let crate::modules::todo_add::Result2::Confirm(txt) = &res {
            push_todo(txt);
            crate::app::redraw();
        }
        return;
    }

    // 「新增日程」同样在 APP 借用之外处理（模态对话框，会再次触达主窗口事件）
    if let HitId::ScheduleAdd = h {
        match crate::modules::schedule_add::prompt() {
            crate::modules::schedule_add::Result3::Confirm(content, date, time, remind_min) => {
                push_schedule(&date, &time, &content, remind_min);
                crate::app::redraw();
            }
            crate::modules::schedule_add::Result3::Cancel => {}
        }
        return;
    }

    // 「编辑自选持仓」同样在 APP 借用之外处理：模态对话框的消息循环会再次触达
    // 主窗口事件，若此刻还持有 APP 借用会重入冲突。
    if let HitId::WatchEdit = h {
        if let Some(items) = crate::modules::watch_edit::edit() {
            crate::modules::stock::save_items(&items);
            crate::modules::stock::reset_alerts();
            crate::app::redraw();
        }
        return;
    }
    let mut open_file: Option<String> = None;
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        // 除「继续点同一个删除按钮」外，任何点击都取消二次确认态（防误删残留）
        let keep_todo_arm = matches!(h, HitId::TodoDel(i) if app.todo_del_arm == Some(*i));
        if !keep_todo_arm {
            app.todo_del_arm = None;
        }
        let keep_file_arm = matches!(h, HitId::FileDel(i) if app.file_del_arm == Some(*i));
        if !keep_file_arm {
            app.file_del_arm = None;
        }
        let keep_sched_arm = matches!(h, HitId::ScheduleDel(i) if app.schedule_del_arm == Some(*i));
        if !keep_sched_arm {
            app.schedule_del_arm = None;
        }
        let n = ui::effective_pages(&app.cfg).len().max(1);
        match h {
            HitId::Pill => {
                // 有提醒时：点胶囊 = 确认提醒，只让它消失，不展开面板
                // （用户要求「点击后就不再显示」，点一下就该回到正常轮播）
                if app.reminder.is_some() {
                    ack_reminder(app);
                } else if app.panel_open && !app.pinned {
                    app.panel_open = false;
                } else {
                    app.panel_open = true;
                    app.page = 0;
                    app.anims.page.set(1.0);
                    app.anims.page_from = 0;
                    app.scroll = 0.0;
                    app.pill_hover = false;
                }
                app.hover_latch = false;
                app.leave_at = None;
                app.apply_vis();
            }
            // 底部分页圆点：点哪个跳哪个（点当前页无动作）
            HitId::PageDot(p) => {
                if *p < n {
                    go_page(app, *p);
                }
            }
            HitId::Collapse => {
                // 收起下拉面板，回到主面板胶囊（被遮挡时自然落回小黑条）
                app.panel_open = false;
                app.pinned = false;
                app.leave_at = None;
                app.todo_del_arm = None;
                app.schedule_del_arm = None;
                app.apply_vis();
            }
            HitId::Pin => {
                // 图钉：锁定当前面板。锁定后 occ_check 不再收起；再点一次解除。
                app.pinned = !app.pinned;
                if app.pinned {
                    app.panel_open = true;
                    app.occluded = false;
                    app.vis = Vis::Dock;
                    app.pill_hover = false;
                    app.leave_at = None;
                } else {
                    // 解除后立刻重判一次遮挡，避免残留锁定态
                    app.occ_next = Instant::now();
                }
                app.flash = Some(Flash::new(
                    if app.pinned { "已钉住面板 · 遮挡时不收起" } else { "已取消钉住" },
                    1600,
                ));
                app.apply_vis();
            }
            HitId::TodoToggle(i) => {
                if let Some(t) = app.store.todos.get_mut(*i) {
                    t.done = !t.done;
                    app.dirty = true;
                }
            }
            // 已在 click 开头单独处理（模态对话框需在 APP 借用之外运行）
            HitId::TodoAdd => {} 
            // 同上：新增日程的模态对话框也在此借用之外处理
            HitId::ScheduleAdd => {}
            // 同上：编辑自选持仓的模态对话框也在此借用之外处理
            HitId::WatchEdit => {}
            HitId::TodoDel(i) => {
                // 二次确认删除：第一次点 → 该行进入「确认删除」态；再点同一行才真删。
                // 防止悬停 × 误触直接丢数据。
                if app.todo_del_arm == Some(*i) {
                    if *i < app.store.todos.len() {
                        app.store.todos.remove(*i);
                        app.save_now_quiet();
                    }
                    app.todo_del_arm = None;
                } else {
                    app.todo_del_arm = Some(*i);
                }
            }
            HitId::ScheduleDel(i) => {
                // 手动日程删除（仅作用域于本地手动录入项，下标对齐 store.local_schedules）。
                // 与待办/文件一致的二次确认：第一次点进入确认态，再点同一项才真删。
                if app.schedule_del_arm == Some(*i) {
                    if *i < app.store.local_schedules.len() {
                        app.store.local_schedules.remove(*i);
                        app.save_now_quiet();
                        app.flash = Some(Flash::new("已删除该日程", 1500));
                    }
                    app.schedule_del_arm = None;
                } else {
                    app.schedule_del_arm = Some(*i);
                }
            }
            HitId::FileOpen(i) => {
                if let Some(f) = app.store.files.get(*i) {
                    open_file = Some(f.path.clone());
                }
            }
            HitId::FileDel(i) => {
                // 二次确认删除：第一次点 → 进入确认态；再点同一行才真删。
                if app.file_del_arm == Some(*i) {
                    if *i < app.store.files.len() {
                        app.store.files.remove(*i);
                        app.save_now_quiet();
                        app.flash = Some(Flash::new("已从中转站移除", 1500));
                    }
                    app.file_del_arm = None;
                } else {
                    app.file_del_arm = Some(*i);
                }
            }
            HitId::ClipCopy(i) => {
                if let Some(text) = app.store.clips.get(*i).map(|c| c.text.clone()) {
                    clipboard::set_text(app.hwnd, &text);
                    app.flash = Some(Flash::new("已复制到剪贴板", 1500));
                }
            }
            HitId::WaterAdd => {
                app.store.water_cups += 1;
                app.last_water_unix = files::now_unix();
                app.reminder = None;
                app.dirty = true;
                app.flash = Some(Flash::new("已记一杯", 1500));
            }
            HitId::WaterOk => {
                app.store.water_cups += 1;
                app.last_water_unix = files::now_unix();
                app.reminder = None;
                app.dirty = true;
            }
            HitId::SedWalk => {
                app.last_sed_unix = files::now_unix();
                app.reminder = None;
            }
            // 面板顶部提醒条的确认按钮：唯一能让提醒消失的入口
            HitId::ReminderAck => ack_reminder(app),
            HitId::MailCopy => {
                if let Some(mc) = app.mail_shared.lock().ok().and_then(|s| s.latest.clone()) {
                    clipboard::set_text(app.hwnd, &mc.code);
                    // 点了复制才开始 5 秒倒计时；这 5 秒里继续显示「已复制」反馈。
                    // 不点则 code_pending 一直为 true，胶囊常驻（同喝水提醒）。
                    let now = Instant::now();
                    app.code_pending = false;
                    app.code_copied_at = Some(now);
                    app.code_until = Some(now + Duration::from_secs(5));
                }
            }
        }
        drop(b);
        redraw();
    });
    if let Some(p) = open_file {
        open_file_direct(&p);
    }
}

/// 供设置窗口调用：打开配置文件
/// 供设置窗口调用：打开日程/待办数据文件（data.json）
pub fn open_data_file() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.store.save_preserving_mail();
            open_in_notepad(&crate::config::data_path().to_string_lossy());
        }
    });
}


pub fn open_config_file() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.cfg.save();
            open_in_notepad(&crate::config::config_path().to_string_lossy());
        }
    });
    // 用户可能在记事本里直接手改 config.json：清空内存快照缓存，
    // 下次 Config::load()（含后台线程）会重新读盘，让手改立即生效。
    crate::config::Config::invalidate();
}

/// 供设置窗口调用：打开数据目录
pub fn open_data_dir() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.store.save_preserving_mail();
        }
    });
    open_path(&data_dir().to_string_lossy());
}

fn open_in_notepad(path: &str) {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("notepad.exe"),
            &windows::core::HSTRING::from(path),
            None,
            SW_SHOWNORMAL,
        );
    }
}

fn open_path(path: &str) {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &windows::core::HSTRING::from(path),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// 用系统默认关联程序直接打开文件（等价双击）。
/// lpOperation 传 None 让 Shell 选用默认动词，覆盖各类文件；
/// 若原文件已被移动/删除，则退回打开其父目录，避免无响应。
fn open_file_direct(path: &str) {
    let p = std::path::Path::new(path);
    if p.exists() {
        unsafe {
            ShellExecuteW(
                None,
                None,
                &windows::core::HSTRING::from(path),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    } else if let Some(dir) = p.parent() {
        open_path(&dir.to_string_lossy());
    }
}

impl App {
    /// 供点击/托盘等入口触发一帧动画（帧时钟常驻，这里只保证从当前状态继续推进）
    fn anim_continue(&mut self) {
        self.anims.last = Instant::now();
    }
}

/// 切换面板轮播页：带方向地启动滑动过渡
fn go_page(app: &mut App, new: usize) {
    let n = ui::effective_pages(&app.cfg).len().max(1);
    let new = new.min(n - 1);
    if new == app.page {
        return;
    }
    let dir = if new > app.page { 1.0 } else { -1.0 };
    app.anims.page_to(app.page, dir);
    app.page = new;
    app.panel_scroll = 0.0;
    // 翻页后行下标可能指向别的条目，清掉未完成的删除确认
    app.todo_del_arm = None;
    app.file_del_arm = None;
    app.schedule_del_arm = None;
}

/// 把界面状态同步到各弹簧的目标值。
/// 必须独立于 step_anims：主循环要先用 busy() 判断"要不要出帧"，
/// 而 busy() 只有在目标已经设定后才可能为 true。
fn sync_targets(app: &mut App) {
    let target = if app.expanded { 1.0 } else { 0.0 };
    app.anims.open.target = target;
    app.anims.panel.target = target;
    app.anims.content.target = target;
    app.anims.pill.target = 1.0 - target;
    // 收起后清零页内滚动，下次展开从头看
    if !app.expanded {
        app.panel_scroll = 0.0;
    }

    // 小黑条：被遮挡 + 没有悬停 + 下拉面板没开 → 收成迷你黑条；
    // 其余显示主面板胶囊（默认态就是它）
    app.anims.mini.target =
        if !app.expanded && app.occluded && !app.pill_hover { 1.0 } else { 0.0 };

    // 饮水进度平滑填充
    let goal = app.cfg.health.water_goal_cups.max(1);
    app.anims.water.target = app.store.water_cups as f32 / goal as f32;

    // 胶囊内容模式切换（提醒/验证码插入）：淡入，避免文字硬替换
    let mk = ui::pill_mode_key(app);
    if mk != app.pill_mode {
        app.pill_mode = mk;
        app.anims.pill_content.v = 0.0;
        app.anims.pill_content.target = 1.0;
        app.anims.pill_content.kick(9.0);
    }
}

/// 推进所有弹簧 dt 秒（与窗口/消息循环无关，便于离线逐帧核对）
fn step_anims(app: &mut App, dt: f32) {
    sync_targets(app);

    // 展开 / 收起：窗口高度、面板背景、内容淡入淡出各自用不同刚度，
    // 形成"外框先动、内容稍后跟上"的层次感
    app.anims.open.step(dt);
    app.anims.panel.step(dt);
    app.anims.content.step(dt);
    app.anims.pill.step(dt);
    app.anims.pill_content.step(dt);
    app.anims.page.step(dt);
    app.anims.water.step(dt);
    app.anims.rot.step(dt);
    app.anims.mini.step(dt);

    // 胶囊右侧信息轮播：收起态每 4.2 秒上滑切换一条；展开时暂停并重置计时。
    // 条目数随模式变化（工作 5 条 / 盯盘 = 自选数 + 今日盈亏），取模用 pill_count；
    // 盯盘模式与薪资模块无关，只要在盯盘模式就继续轮播。
    let rotating = !app.expanded && (app.cfg.stock_mode() || app.cfg.modules.salary);
    if rotating {
        if Instant::now() >= app.rot_next {
            let n = ui::pill_count(app).max(1);
            app.rot_prev = app.rot_i % n;
            app.rot_i = (app.rot_i + 1) % n;
            app.rot_next = Instant::now() + Duration::from_millis(4200);
            app.anims.rot_to();
        }
    } else {
        app.rot_next = Instant::now() + Duration::from_millis(4200);
    }

    // 悬停渐变
    let hv = app.hover.clone();
    app.anims.hover_step(hv.as_ref(), dt);
}

/// 外部几何变动（桌面分区被拖动 / 缩放 / 显隐）后调用：让下一次 `occ_check`
/// 立刻重判，而不是等最多 350ms 的节流窗口过去 —— 分区挪到灵动岛跟前时，
/// 收起要跟手。
pub fn occ_poke() {
    APP.with(|a| {
        if let Some(app) = a.borrow_mut().as_mut() {
            app.occ_next = Instant::now();
        }
    });
}

/// 遮挡检测：面板区域被别的应用窗口压住就收成横条，遮挡消失再恢复面板。
/// 每 350ms 判一次（枚举窗口只取矩形，开销很小）。
fn occ_check(app: &mut App) {
    let now = Instant::now();
    if now < app.occ_next {
        return;
    }
    app.occ_next = now + Duration::from_millis(350);
    if app.hidden {
        return;
    }
    // 图钉锁定：面板固定展开，遮挡检测完全跳过（也不会被小黑条替代）
    if app.pinned {
        if !app.panel_open || app.occluded || app.vis != Vis::Dock || !app.expanded {
            app.panel_open = true;
            app.occluded = false;
            app.vis = Vis::Dock;
            app.pill_hover = false;
            app.leave_at = None;
            app.apply_vis();
        }
        return;
    }
    // 提醒进行中：无论被什么窗口（含全屏应用）盖住，都不收成小黑条，
    // 让胶囊一直亮着提醒
    if app.reminder.is_some() {
        if app.occluded || app.vis == Vis::Auto {
            app.occluded = false;
            app.vis = Vis::Dock;
            app.pill_hover = false;
            app.apply_vis();
        }
        return;
    }
    if !app.cfg.auto_collapse {
        // 用户在设置里关掉「遮挡时自动收起」→ 从不判定遮挡
        if app.occluded || app.vis == Vis::Auto {
            app.occluded = false;
            app.vis = Vis::Dock;
            app.pill_hover = false;
            app.apply_vis();
        }
        return;
    }
    // 判定对象跟随当前形态：下拉面板开着就判下拉面板，只露胶囊时判胶囊
    let (x, y, w, h) = occ_area(app);
    // 迟滞：已收起时用更小的重叠阈值判断「是否解除」，
    // 避免分区/窗口停在阈值边缘反复收起展开
    let min_overlap = if app.occluded {
        occlusion::RELEASE_OVERLAP
    } else {
        occlusion::MIN_OVERLAP
    };
    let hit = occlusion::blocker_with(x, y, w, h, min_overlap);
    let covered = hit.is_some();
    if covered == app.occluded {
        return;
    }
    // 只在状态翻转时记一笔，便于核对「到底被谁压住了」（含桌面分区）
    match hit {
        Some(h) => diag_log(&format!("OCC on by {}", occlusion::describe(h))),
        None => diag_log("OCC off"),
    }
    app.occluded = covered;
    if covered {
        // 鼠标正停在刚点开的面板上 → 先留着，移开后由 hover_tick 收回
        let holding = app.panel_open && cursor_in(x, y, w, h);
        if !holding {
            app.panel_open = false;
        }
        app.vis = Vis::Auto;
        app.pill_hover = !holding && cursor_in(x, y, w, h);
        app.leave_at = None;
    } else {
        // 遮挡解除 → 回到主面板胶囊（下拉面板不自动重开，需要点击）
        app.vis = Vis::Dock;
        app.pill_hover = false;
        app.leave_at = None;
    }
    app.apply_vis();
}

/// 收起态的悬停：鼠标移到小黑条上展开成主面板胶囊，
/// 移开约 180ms 收回小黑条。悬停只到胶囊这一级，不再自动打开大面板
/// （大面板由点击胶囊触发）。用轮询而不是 WM_MOUSELEAVE：
/// 面板展开会改变窗口尺寸，系统可能在尺寸变化时误发离开消息，轮询判定更稳。
fn hover_tick(app: &mut App) {
    if app.hidden {
        return;
    }
    // 提醒期间不做悬停切换：胶囊要一直亮着提醒，也不因悬停自动展开面板
    if app.reminder.is_some() {
        return;
    }
    // 没被遮挡：正常形态（胶囊或下拉面板）不需要悬停切换
    if !app.occluded {
        app.pill_hover = false;
        app.hover_latch = false;
        app.leave_at = None;
        return;
    }
    // 被遮挡时点开的下拉面板：只作临时查看，鼠标离开 180ms 收回
    if app.expanded {
        let (x, y, w, h) = panel_rect();
        if cursor_in(x, y, w, h) {
            app.leave_at = None;
        } else {
            let since = *app.leave_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(180) {
                app.panel_open = false;
                app.leave_at = None;
                app.apply_vis();
            }
        }
        return;
    }
    // 收起态：小黑条 ↔ 主面板胶囊（悬停区域随当前形态变化）
    let (x, y, w, h) = if app.pill_hover { pill_rect() } else { sliver_rect() };
    if cursor_in(x, y, w, h) {
        app.leave_at = None;
        // 手动收起后鼠标还停在原位，必须先移开，否则会「刚收起就弹开」
        if app.hover_latch {
            return;
        }
        if !app.pill_hover {
            app.pill_hover = true;
            app.anim_continue();
        }
    } else {
        app.hover_latch = false;
        if app.pill_hover {
            let since = *app.leave_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(180) {
                app.pill_hover = false;
                app.leave_at = None;
                app.anim_continue();
            }
        }
    }
}

/// 推进一帧：图标能量/相位每帧连续推进，界面按需重绘。
/// 空闲节流到 ~30fps（缓慢呼吸已足够顺滑），打字 / 声音 / 过渡期间跟随 vsync 全速。
fn anim_step() {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        let app = match b.as_mut() {
            Some(x) => x,
            None => return,
        };
        let now = Instant::now();
        let raw = (now - app.anims.last).as_secs_f32();
        // 长时间阻塞后的首帧只建立时间基准，不按真实间隔跳步
        let dt = if raw > 0.06 { 1.0 / 60.0 } else { raw.clamp(0.001, 0.05) };

        // 能量与相位每帧都推进（含被节流跳过的帧），保证动画不断档
        icon::advance(app, dt);

        // 每一帧都判一次「要不要收起 / 要不要展开」：轮询鼠标位置，
        // 遮挡检查按 350ms 节流。必须在下面的空闲提前返回之前执行，
        // 否则鼠标停在横条上时永远等不到展开。
        occ_check(app);
        hover_tick(app);
        sync_targets(app);
        ensure_topmost(app);

        let active = app.anims.busy()
            || app.flash.as_ref().is_some_and(|f| f.alive())
            || app.code_pending
            || app.code_until.is_some_and(|t| now < t)
            // 「已复制」反馈按 5 秒展示（ui.rs 同口径）。这里多留 200ms，
            // 保证 5 秒到期后至少还有一帧重绘，把胶囊真正从画面上清掉；
            // 旧值 1500ms 会在该重绘的那一刻提前停止重绘，导致通知残留在屏幕上。
            || app.code_copied_at.is_some_and(|t| now < t + Duration::from_millis(5200))
            || icon::active(app);
        if !active && raw < 0.033 {
            return; // 本帧只推进能量/相位，不提交重绘
        }
        app.anims.last = now;
        step_anims(app, dt);
        drop(b);
        if PROBE_T0.with(|c| c.get().is_some()) {
            let ms = raw * 1000.0;
            PROBE_FRAMES.with(|v| {
                let mut v = v.borrow_mut();
                if v.len() < 400 {
                    v.push(ms);
                }
            });
        }
        redraw();
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Explorer 重启时系统广播 "TaskbarCreated"，托盘图标会被清空；
    // 注册该消息并在收到时重新添加托盘图标，避免资源管理器重启后托盘丢失。
    static TASKBAR_MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    if msg == *TASKBAR_MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }) {
        tray::add(hwnd);
        return LRESULT(0);
    }
    match msg {
        WM_TIMER => {
            if wparam.0 == TIMER_TICK {
                tick_second();
            }
            LRESULT(0)
        }
        WM_PAINT => {
            // 动画期间由帧循环统一出帧；这里再画一次会和帧循环重复（导致偶发的
            // "一帧里画两次"）。非动画状态才按需重绘。
            if !frame_wanted() {
                redraw();
            }
            let _ = ValidateRect(Some(hwnd), None);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let changed = APP.with(|a| {
                let mut b = a.borrow_mut();
                let Some(app) = b.as_mut() else { return false };
                let hit = app
                    .hits
                    .iter()
                    .rev()
                    .find(|h| x >= h.rect.left && x <= h.rect.right && y >= h.rect.top && y <= h.rect.bottom)
                    .map(|h| h.id.clone());
                if app.hover != hit {
                    app.hover = hit;
                    true
                } else {
                    false
                }
            });
            if changed {
                redraw();
            }
            // 登记离开通知，鼠标移出窗口时清掉悬停高亮
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            APP.with(|a| {
                let mut b = a.borrow_mut();
                if let Some(app) = b.as_mut() {
                    if app.hover.is_some() {
                        app.hover = None;
                        drop(b);
                        redraw();
                    }
                }
            });
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            APP.with(|a| {
                let mut b = a.borrow_mut();
                if let Some(app) = b.as_mut() {
                    let x = (lparam.0 & 0xFFFF) as i16 as i32;
                    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                    let hit = app.hits.iter().rev().find(|h| {
                        x >= h.rect.left && x <= h.rect.right && y >= h.rect.top && y <= h.rect.bottom
                    });
                    if let Some(h) = hit {
                        let id = h.id.clone();
                        drop(b);
                        click(&id);
                    }
                }
            });
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as i16) as f32;
            APP.with(|a| {
                let mut b = a.borrow_mut();
                let Some(app) = b.as_mut() else { return };
                if !app.expanded {
                    return;
                }
                // 滚轮只滚动当前页的长内容，不再切换轮播页（翻页走底部箭头）
                let pages = ui::effective_pages(&app.cfg);
                let idx = app.page.min(pages.len().saturating_sub(1));
                let kind = pages.get(idx).copied().unwrap_or("overview");
                let max = ui::panel_scroll_max(app, kind);
                if max <= 0.0 {
                    app.wheel_acc = 0.0;
                    return;
                }
                // 累积滚轮量：一格滚动约一行半，触控板细碎事件平滑累积
                app.wheel_acc += delta;
                let steps = (app.wheel_acc / 120.0) as i32;
                if steps == 0 {
                    return;
                }
                app.wheel_acc -= steps as f32 * 120.0;
                let old = app.panel_scroll;
                app.panel_scroll =
                    (app.panel_scroll - steps as f32 * crate::render::s(40.0)).clamp(0.0, max);
                if (app.panel_scroll - old).abs() > 0.01 {
                    app.anim_continue();
                    drop(b);
                    redraw();
                }
            });
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            on_clipboard_update();
            LRESULT(0)
        }
        // OLE 拖放回调通过消息把 UI 操作切回主线程（回调里严禁同步重绘）
        ui::WM_DRAG_ENTER => {
            set_drag_hover(true);
            LRESULT(0)
        }
        ui::WM_DRAG_LEAVE => {
            set_drag_hover(false);
            LRESULT(0)
        }
        ui::WM_DROP_FILES => {
            // lParam 是在 OLE 回调 / 分区图标钩子里 Box 出来的路径列表堆指针，
            // 这里接管并释放。两个来源共用同一条入站通道：
            // - 资源管理器 OLE 拖放：DragEnter 已展开面板
            // - 分区图标拖动：没有 DragEnter，需要这里显式展开并切到中转站页
            let paths = lparam.0 as *mut Vec<String>;
            if !paths.is_null() {
                let list = unsafe { Box::from_raw(paths) };
                diag_log(&format!("DROP_MSG n={}", list.len()));
                let mut any = false;
                for path in list.iter() {
                    push_file_external(path.clone());
                    any = true;
                }
                if any {
                    focus_filehub();
                }
            }
            set_drag_hover(false);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // 窗口类 hCursor 为 NULL，必须自行设置，否则悬停时残留系统"加载中"转圈
            let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            SetCursor(Some(arrow));
            LRESULT(1)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_DESTROY => {
            // 不管从哪条路径退出（托盘 / 系统关机 / 任务管理器优雅关闭），
            // 都把设置窗口同步藏掉，避免残留
            crate::settings::close_for_quit();
            // 模态弹窗同样主动关闭（这条路径覆盖系统关机等非托盘退出）
            crate::modules::watch_edit::close_for_quit();
            crate::modules::todo_add::close_for_quit();
            crate::modules::schedule_add::close_for_quit();
            tray::remove(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        tray::TRAY_MSG => {
            tray::on_msg(hwnd, lparam);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
