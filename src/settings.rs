//! 设置窗口 v2 —— 全部自绘（Direct2D + DirectWrite）
//!
//! 视觉语言与灵动岛本体统一：深色底 + 蓝色强调色 + 8pt 栅格 + 12px 圆角。
//! 结构：自绘标题栏（可拖动）/ 左侧分组导航 / 右侧「页标题 + 卡片」/ 底部状态栏。
//!
//! 交互约定
//! - 开关、按钮、导航：自绘 + 命中区，点击立刻生效并存盘
//! - 文本框：原生 EDIT（保证输入法 / 剪贴板 / 光标可用），外框由 D2D 绘制，失焦即保存
//! - 所有配置改动即时写入 config.json，并在状态栏给出「已保存」反馈
//!
//! ── 设计令牌 ──────────────────────────────────────────────
//! 色彩  bg #0F0F13 / 侧栏 #131318 / 卡片 #16161B / 输入 #1E1E25
//!       text #F0F0F5 · 次级 #9C9CA7 · 弱化 #6B6B77
//!       accent #4E8FFF（与灵动岛一致）
//! 字号  19 页标题 / 13 行标题 / 12 按钮 / 10.5 说明
//! 间距  4 / 8 / 12 / 16 / 24 / 32，行高 56，卡片内边距 7

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect,
    ScreenToClient, SetBkColor, SetBkMode, SetTextColor, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
    DEFAULT_CHARSET, DEFAULT_PITCH, FW_NORMAL, HBRUSH, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::render::{s, si, Canvas, W_MED, W_NORMAL, W_SEMI};

// ───────────────────────── 尺寸 ─────────────────────────
const WIN_W: f32 = 900.0;
const WIN_H: f32 = 720.0;
const TITLE_H: f32 = 56.0;
const SIDE_W: f32 = 200.0;
const FOOT_H: f32 = 64.0;
const PAD: f32 = 32.0;

const ROW_H: f32 = 56.0;
const CARD_PAD: f32 = 7.0;
const CARD_R: f32 = 12.0;
const CAT_H: f32 = 30.0;
const NOTE_H: f32 = 32.0;
const HERO_H: f32 = 104.0;
const INPUT_H: f32 = 34.0;
const BTN_H: f32 = 32.0;
const NAV_H: f32 = 34.0;
const WIN_BTN: f32 = 30.0;

// ───────────────────────── 色彩 ─────────────────────────
const BG: u32 = 0x0F0F13;
const SIDE_BG: u32 = 0x131318;
const FOOT_BG: u32 = 0x111116;
const HAIR: u32 = 0x1E1E25;
const CARD: u32 = 0x16161B;
const CARD_BD: u32 = 0x272731;
const INPUT_BG: u32 = 0x1E1E25;
const INPUT_BD: u32 = 0x343440;
const TXT: u32 = 0xF0F0F5;
const TXT2: u32 = 0x9C9CA7;
const TXT3: u32 = 0x84848F;
const GREEN: u32 = 0x4ADE80;
const AMBER: u32 = 0xE8B33A;
const DANGER: u32 = 0xE5484D;
const NAV_ON: u32 = 0x1E1E27;

// ───────────────────────── 导航表 ─────────────────────────
// (页码, 分组, 导航文字) —— 页码与配置页一一对应，不随展示顺序变化
const GROUPS: [&str; 4] = ["外观", "效率", "关怀", "其他"];
const NAV: [(usize, usize, &str); 12] = [
    (0, 0, "通用"),
    (11, 0, "面板页面"),
    (2, 1, "打工鼓励器"),
    (3, 1, "日程安排"),
    (4, 1, "待办清单"),
    (5, 1, "文件中转站"),
    (6, 1, "剪贴板中转"),
    (7, 1, "桌面分区"),
    (9, 1, "灵动图标"),
    (1, 2, "健康提醒"),
    (8, 2, "邮箱验证码"),
    (10, 3, "关于"),
];

const ID_TIMER_STATUS: usize = 1;
const ID_TIMER_ICS: usize = 3;
/// 邮箱「测试连接」进行中：每 500ms 重绘一次状态行，拿到结果就停
const ID_TIMER_MAIL: usize = 4;
const EM_SETMARGINS: u32 = 0x00D3;
const EC_LEFTMARGIN: usize = 1;
const EC_RIGHTMARGIN: usize = 2;
const EM_SETCUEBANNER: u32 = 0x1501;
const EN_SETFOCUS_CODE: u32 = 0x0100;
const EN_KILLFOCUS_CODE: u32 = 0x0200;

// ───────────────────────── 控件模型 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind {
    Toggle,
    Field,
    Action,
    Caption,
    Note,
    Hero,
    /// 面板轮播页：开关 + ↑↓ 排序
    PageRow,
    /// 主题色：一排可点色块
    Theme,
    /// 灵动图标：一排带实时预览的可点图标卡片
    IconPick,
    /// 声音响应开关（不对应某个功能模块，单独命中）
    Sound,
    /// 工作日：一排「一二三四五六日」可点方块
    Weekday,
    /// 时间选择器：点击弹出时/分选择面板（不创建原生 EDIT）
    Time,
    /// 动态状态文字（目前用于邮箱验证码监听状态）
    MailStatus,
}

struct Row {
    kind: RowKind,
    slot: i32,
    label: &'static str,
    desc: &'static str,
    suffix: &'static str,
    placeholder: &'static str,
    fw: f32,
    int: bool,
    pwd: bool,
    /// (文字, slot, 主按钮?, 宽度)
    btns: Vec<(&'static str, i32, bool, f32)>,
}

impl Row {
    fn toggle(slot: i32, label: &'static str, desc: &'static str) -> Row {
        Row { kind: RowKind::Toggle, slot, label, desc, suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    fn field(slot: i32, label: &'static str, desc: &'static str, fw: f32) -> Row {
        Row { kind: RowKind::Field, slot, label, desc, suffix: "", placeholder: "", fw, int: false, pwd: false, btns: Vec::new() }
    }
    fn time(slot: i32, label: &'static str, desc: &'static str) -> Row {
        Row { kind: RowKind::Time, slot, label, desc, suffix: "", placeholder: "", fw: 100.0, int: false, pwd: false, btns: Vec::new() }
    }
    fn int(mut self) -> Row {
        self.int = true;
        self
    }
    fn pwd(mut self) -> Row {
        self.pwd = true;
        self
    }
    fn suffix(mut self, t: &'static str) -> Row {
        self.suffix = t;
        self
    }
    fn ph(mut self, t: &'static str) -> Row {
        self.placeholder = t;
        self
    }
    fn action(slot: i32, label: &'static str, desc: &'static str, btns: Vec<(&'static str, i32, bool, f32)>) -> Row {
        Row { kind: RowKind::Action, slot, label, desc, suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns }
    }
    fn caption(text: &'static str) -> Row {
        Row { kind: RowKind::Caption, slot: 0, label: text, desc: "", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 日程来源分组标题：slot = 10/20/30，右侧实时显示启用状态徽标
    fn cal_caption(text: &'static str, slot: i32) -> Row {
        Row { kind: RowKind::Caption, slot, label: text, desc: "", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    fn note(text: &'static str) -> Row {
        Row { kind: RowKind::Note, slot: 0, label: text, desc: "", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 邮箱监听状态行：文字来自后台线程，每次重绘都取最新
    fn mail_status() -> Row {
        Row { kind: RowKind::MailStatus, slot: 0, label: "", desc: "", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    fn hero() -> Row {
        Row { kind: RowKind::Hero, slot: 0, label: "", desc: "", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 主题色选择行：右侧一排色块
    fn theme() -> Row {
        Row { kind: RowKind::Theme, slot: 0, label: "主题色", desc: "胶囊与面板的强调色", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 图标样式选择行
    fn icon_pick() -> Row {
        Row { kind: RowKind::IconPick, slot: 0, label: "图标样式", desc: "点击切换，所有形态都随打字与声音律动", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 声音响应开关行
    fn sound() -> Row {
        Row { kind: RowKind::Sound, slot: 0, label: "响应系统声音", desc: "播放音乐 / 视频时图标跳动更明显", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 工作日选择行：哪天算上班
    fn weekday() -> Row {
        Row { kind: RowKind::Weekday, slot: 0, label: "工作日", desc: "点选需要上班的星期；未自定义时按法定日历判定", suffix: "", placeholder: "", fw: 0.0, int: false, pwd: false, btns: Vec::new() }
    }
    /// 面板轮播页行：slot = 在 panel.pages 中的下标；btns = [↑, ↓]
    fn page_row(idx: usize, label: &'static str) -> Row {
        let slot = idx as i32;
        Row {
            kind: RowKind::PageRow,
            slot,
            label,
            desc: "",
            suffix: "",
            placeholder: "",
            fw: 0.0,
            int: false,
            pwd: false,
            btns: vec![("↑", idx as i32 * 2, false, 26.0), ("↓", idx as i32 * 2 + 1, false, 26.0)],
        }
    }
}

struct Page {
    title: &'static str,
    desc: &'static str,
    rows: Vec<Row>,
}

struct Placed {
    row: usize,
    y: f32, // 逻辑坐标（未乘 DPI 缩放）
    h: f32,
}

struct Layout {
    placed: Vec<Placed>,
    cards: Vec<(f32, f32)>, // (y, h) 逻辑坐标
}

fn layout(page: &Page) -> Layout {
    let mut y = 146.0f32;
    let mut placed = Vec::new();
    let mut cards = Vec::new();
    let mut i = 0usize;
    let rows = &page.rows;
    while i < rows.len() {
        match rows[i].kind {
            RowKind::Caption => {
                placed.push(Placed { row: i, y, h: CAT_H });
                y += CAT_H;
                i += 1;
            }
            RowKind::Note => {
                placed.push(Placed { row: i, y, h: NOTE_H });
                y += NOTE_H;
                i += 1;
            }
            RowKind::MailStatus => {
                placed.push(Placed { row: i, y, h: NOTE_H });
                y += NOTE_H;
                i += 1;
            }
            RowKind::Hero => {
                placed.push(Placed { row: i, y, h: HERO_H });
                y += HERO_H + 16.0;
                i += 1;
            }
            _ => {
                let start = i;
                while i < rows.len()
                    && matches!(
                        rows[i].kind,
                        RowKind::Toggle
                            | RowKind::Field
                            | RowKind::Time
                            | RowKind::Action
                            | RowKind::PageRow
                            | RowKind::Theme
                            | RowKind::IconPick
                            | RowKind::Sound
                            | RowKind::Weekday
                    )
                {
                    i += 1;
                }
                let n = i - start;
                let ch = CARD_PAD * 2.0 + n as f32 * ROW_H;
                cards.push((y, ch));
                for k in 0..n {
                    placed.push(Placed { row: start + k, y: y + CARD_PAD + k as f32 * ROW_H, h: ROW_H });
                }
                y += ch + 14.0;
            }
        }
    }
    Layout { placed, cards }
}

fn pages() -> Vec<Page> {
    vec![
        // 0 通用
        Page {
            title: "通用",
            desc: "启动方式与外观",
            rows: vec![
                Row::caption("启动"),
                Row::toggle(0, "开机自启", "登录 Windows 后静默启动灵动岛"),
                Row::caption("显示"),
                Row::toggle(1, "被遮挡时自动收起", "面板区域被其它窗口压住时收成顶部横条，鼠标移上去再展开，移开自动收回"),
                Row::caption("外观"),
                Row::theme(),
                Row::note("灵动岛固定在屏幕顶部居中：没有窗口压住面板区域时显示面板，被压住时收成横条。金额符号固定为 ¥。"),
            ],
        },
        // 1 健康提醒
        Page {
            title: "健康提醒",
            desc: "久坐与喝水提醒，到点切换胶囊提醒态",
            rows: vec![
                Row::toggle(0, "启用健康提醒", "到点后胶囊变为提醒条，点「喝好了 / 起来走走」确认"),
                Row::field(1, "久坐阈值", "连续无输入达到该时长即提醒", 108.0).suffix("分钟").int(),
                Row::field(2, "喝水间隔", "两次提醒之间的最短间隔", 108.0).suffix("分钟").int(),
                Row::field(3, "每日目标", "面板中的喝水进度按此计算", 108.0).suffix("杯").int(),
                Row::note("久坐时长取自系统真实输入空闲时间，不记录任何操作内容。"),
            ],
        },
        // 2 打工鼓励器
        Page {
            title: "打工鼓励器",
            desc: "按月薪折算到每秒，胶囊轮播今天已赚 / 工作进度 / 距下班",
            rows: vec![
                Row::toggle(0, "启用打工鼓励器", "关闭后胶囊右侧改为显示打字速度"),
                Row::field(1, "月薪", "用于折算每秒收入", 128.0).suffix("元"),
                Row::field(2, "每日工时", "", 100.0).suffix("小时"),
                Row::time(4, "上午上班", "点击选择上下班时刻"),
                Row::time(6, "午休开始", "去吃饭的时间"),
                Row::time(7, "下午上班", "午休结束时间"),
                Row::time(5, "下班时间", "点击选择下班时刻"),
                Row::weekday(),
                Row::note("上班状态 = 勾选的星期 + 官方节假日 / 调休；金额只在上下班区间内增长，午休不计。节假日联网自动同步，同步失败时不展示。"),
            ],
        },
        // 3 日程安排
        Page {
            title: "日程安排",
            desc: "面板「日程安排」页中的定时事项",
            rows: vec![
                Row::toggle(0, "启用日程安排", "在面板里显示今天的外部日程"),
                Row::field(2, "临近提醒", "日程开始前多少分钟在胶囊上提示，0 = 不提醒", 108.0).suffix("分钟").int(),
                Row::field(4, "自动刷新", "每隔多少分钟同步一次，最小 10", 100.0).suffix("分钟").int(),
                Row::action(5, "同步", "立即同步所有已启用的来源，合并去重后展示", vec![("立即同步", 5, true, 112.0)]),
                Row::note("三种来源可同时启用、各自独立配置，均通过 CalDAV 同步（三家都不提供 ICS 订阅地址）；密码以明文存在本机 config.json，不上传。"),

                Row::cal_caption("飞书日历", 10),
                Row::toggle(10, "启用飞书", "只读同步「我的日历」"),
                Row::field(12, "服务器", "桌面端 设置 → 日历 → CalDAV 同步 → 生成，可改", 200.0).ph("caldav.feishu.cn"),
                Row::field(13, "账号", "上面生成的 CalDAV 用户名", 200.0),
                Row::field(14, "密码", "上面生成的 CalDAV 密码", 200.0).pwd(),

                Row::cal_caption("钉钉日历", 20),
                Row::toggle(20, "启用钉钉", "只读同步钉钉日历（CalDAV）"),
                Row::field(22, "服务器", "钉钉 CalDAV 服务器，可改", 200.0).ph("calendar.dingtalk.com"),
                Row::field(23, "账号", "CalDAV 用户名", 200.0),
                Row::field(24, "密码", "CalDAV 密码", 200.0).pwd(),

                Row::cal_caption("企业微信日历", 30),
                Row::toggle(30, "启用企业微信", "手机端 工作台 → 日程 → 右上角「…」→ 日程设置 → 同步至其他日历"),
                Row::field(31, "服务器", "填页面给的服务器即可（CalDAV 自动走 caldav 子域）", 200.0).ph("wecom.work"),
                Row::field(32, "账号", "页面给的账号，形如 you@corp.wecom.work", 220.0),
                Row::field(33, "密码", "该页面每打开一次就换新密码，换了要回来重填", 220.0).pwd(),
            ],
        },
        // 4 待办清单
        Page {
            title: "待办清单",
            desc: "面板中的待办，可直接点击打钩",
            rows: vec![
                Row::toggle(0, "启用待办清单", "显示未完成项与完成进度"),
                Row::action(1, "待办数据", "保存在本地 data.json，每行一条事项", vec![("打开待办文件", 1, false, 132.0)]),
                Row::note("勾选状态会实时写回本地文件。"),
            ],
        },
        // 5 文件中转站
        Page {
            title: "文件中转站",
            desc: "拖文件到岛上自动展开暂存，数量上限可自定义",
            rows: vec![
                Row::toggle(0, "启用文件中转站", "拖入文件时自动展开并暂存"),
                Row::field(1, "暂存数量上限", "最多暂存的文件数；满额后拒绝新增", 44.0).int().suffix("个").ph("20"),
                Row::note("只记录路径不复制内容，不占内存；单击条目用默认程序直接打开文件，悬停可删除。上限调小后会自动移除超出的旧条目。"),
            ],
        },
        // 6 剪贴板中转
        Page {
            title: "剪贴板中转",
            desc: "自动记录最近复制的文本，点击即可复制回去",
            rows: vec![
                Row::toggle(0, "启用剪贴板中转", "监听系统剪贴板并记录文本"),
                Row::field(1, "保留条数", "超出后丢弃最旧的记录", 108.0).suffix("条").int(),
                Row::note("仅在本地记录，不上传任何内容。"),
            ],
        },
        // 7 桌面分区
        Page {
            title: "桌面分区",
            desc: "在桌面上划出毛玻璃围栏，桌面图标按围栏分组收纳；图标拖进围栏即归类，不搬动文件内容",
            rows: vec![
                Row::toggle(0, "启用桌面分区", "开启后桌面图标由毛玻璃围栏接管显示（首次自动按文件类型分组）"),
                Row::toggle(1, "开机自启分区", "随系统启动自动显示围栏"),
                Row::note("围栏是一块半透明毛玻璃面板：把文件拖进围栏即归入该组、双击图标打开、拖标题栏移动、拖四边四角缩放、点右上角箭头折叠、右键可改名 / 换色 / 删除。位置与大小自动保存。"),
                Row::note("桌面文件有任何增删改名都会自动同步并归入对应围栏，无需手动刷新。在围栏上点右键可新建围栏、隐藏 / 显示全部围栏。"),
            ],
        },
        // 8 邮箱验证码
        Page {
            title: "邮箱验证码",
            desc: "IMAP 本地解析验证码，正文不留存不上传",
            rows: vec![
                Row::toggle(0, "启用邮箱验证码", "需要先填好下面的 IMAP 参数"),
                Row::field(1, "IMAP 服务器", "", 220.0).ph("imap.qq.com"),
                Row::field(2, "端口", "", 100.0).int(),
                Row::field(3, "账号", "", 220.0).ph("you@example.com"),
                Row::field(4, "密码 / 授权码", "QQ 邮箱请使用授权码而非登录密码", 220.0).pwd(),
                Row::field(5, "文件夹", "", 160.0).ph("INBOX"),
                Row::field(6, "轮询间隔", "最小 10 秒", 108.0).suffix("秒").int(),
                Row::action(7, "应用配置", "保存后立即生效，无需重启", vec![("保存配置", 7, true, 110.0), ("测试连接", 8, false, 96.0)]),
                Row::mail_status(),
            ],
        },
        // 9 灵动图标
        Page {
            title: "灵动图标",
            desc: "胶囊最左侧图标持续律动，打字或播放声音时跳动更明显",
            rows: vec![
                Row::toggle(0, "响应打字", "按最近几秒的按键节奏加大跳动幅度"),
                Row::icon_pick(),
                Row::sound(),
                Row::note("只统计按键次数与系统输出音量峰值，不记录键值、窗口标题、音频内容，也不上传。"),
            ],
        },
        // 10 关于
        Page {
            title: "关于 WinIsland",
            desc: "单一原生 exe · 零运行时依赖 · 零收集零上传",
            rows: vec![
                Row::hero(),
                Row::action(0, "配置文件", "%APPDATA%\\WinIsland\\config.json", vec![("打开配置文件", 0, false, 132.0)]),
                Row::action(1, "数据目录", "日程、待办、剪贴板、打字统计都在这里", vec![("打开数据目录", 1, false, 132.0)]),
                Row::note("源码开源，全程可审计。任何功能都不发起数据上传。"),
            ],
        },
        // 11 面板页面
        panel_page(),
    ]
}

/// 面板轮播页配置：列出 PANEL_KINDS 中的每一页，可排序、可关
fn panel_page() -> Page {
    let mut rows = vec![Row::caption("轮播顺序 · 点击灵动岛可翻页")];
    for (i, (_, name)) in crate::config::PANEL_KINDS.iter().enumerate() {
        rows.push(Row::page_row(i, name));
    }
    rows.push(Row::note("关闭的页面不参与轮播；模块本身关闭时该行会置灰、开关不可点，需先到对应模块页开启。"));
    Page {
        title: "面板页面",
        desc: "决定点击灵动岛后能翻到哪几页、按什么顺序",
        rows,
    }
}

// ───────────────────────── 命中区 / 状态 ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum HitId {
    None,
    Nav(usize),
    Toggle(usize, i32),
    Btn(usize, i32),
    Theme(usize),
    Icon(usize),
    /// 工作日（0=周一 … 6=周日）
    Weekday(usize),
    Sound,
    /// 底部「保存并应用」按钮：保存所有页配置 + 刷新面板数据
    Sync,
    /// 时间选择器字段（点击弹出面板）
    Time(usize, i32),
    /// 时间选择面板内的步进按钮：参数 (部件, 方向)，部件 0=时 1=分；方向 +1/-1
    TimeStep(usize, i32),
    /// 直接点时/分数字框进入键盘输入（部件 0=时 1=分）
    TimeNum(usize),
    /// 时间选择面板确定 / 取消
    TimeOk,
    TimeCancel,
    Min,
    Close,
}

impl HitId {
    fn clickable(self) -> bool {
        !matches!(self, HitId::None)
    }
}

struct Hit {
    id: HitId,
    rect: RECT,
}

thread_local! {
    static WIN: Cell<isize> = const { Cell::new(0) };
    static CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
    static HITS: RefCell<Vec<Hit>> = const { RefCell::new(Vec::new()) };
    static PAGE: Cell<usize> = const { Cell::new(0) };
    static HOVER: Cell<HitId> = const { Cell::new(HitId::None) };
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    /// (页码, slot, hwnd)
    static EDITS: RefCell<Vec<(usize, i32, isize)>> = const { RefCell::new(Vec::new()) };
    static EDIT_BRUSH: Cell<isize> = const { Cell::new(0) };
    static EDIT_FONT: Cell<isize> = const { Cell::new(0) };
    static CLASS_ON: Cell<bool> = const { Cell::new(false) };
    static STATUS: RefCell<Option<(String, Instant)>> = const { RefCell::new(None) };
    /// 当前页的滚动偏移（逻辑 px）
    static SCROLL: Cell<f32> = const { Cell::new(0.0) };
    /// 时间选择器字段的草稿值：(page, slot) -> "HH:MM"。不创建原生 EDIT，用自绘选择器赋值。
    static TIME_VALS: RefCell<Vec<((usize, i32), String)>> = const { RefCell::new(Vec::new()) };
    /// 当前打开的时间选择器字段（page, slot），None 表示未打开
    static TIME_PICK: Cell<Option<(usize, i32)>> = const { Cell::new(None) };
    /// 选择器里正在调整的时 / 分
    static TIME_H: Cell<u32> = const { Cell::new(9) };
    static TIME_M: Cell<u32> = const { Cell::new(0) };
    /// 哪个数字框正在键盘输入：0=时 1=分，None=没有（保留 ▲▼ 步进行为）
    static TIME_EDIT: Cell<Option<usize>> = const { Cell::new(None) };
    /// 键盘输入时的原始字符串缓冲（用户可输 "8" 或 "08"），空串表示未输入
    static TIME_BUF: RefCell<[String; 2]> = const { RefCell::new([String::new(), String::new()]) };
}

fn tw(txt: &str, size: f32) -> f32 {
    let sz = s(size);
    txt.chars().map(|ch| if (ch as u32) > 0x2E7F { sz } else { sz * 0.56 }).sum()
}

/// 日程来源的当前状态：(颜色, 文案)。每次重绘实时读 config，开关与输入框改动后立即同步
fn cal_state(slot: i32) -> (u32, &'static str) {
    crate::app::with_cfg(|c| {
        let src = match slot {
            10 => &c.schedule_sub.feishu,
            20 => &c.schedule_sub.dingtalk,
            _ => &c.schedule_sub.wecom,
        };
        if !src.on {
            return (TXT3, "已停用");
        }
        let via_ics = !src.ics_url.trim().is_empty();
        let via_dav = !src.dav_server.trim().is_empty()
            && !src.dav_user.trim().is_empty()
            && !src.dav_pass.trim().is_empty();
        if via_ics || via_dav {
            (GREEN, "已启用")
        } else {
            (AMBER, "已启用 · 待配置")
        }
    })
}

fn cref(hex: u32) -> COLORREF {
    COLORREF(((hex & 0xFF) << 16) | (hex & 0xFF00) | ((hex >> 16) & 0xFF))
}

fn push_hit(hits: &mut Vec<Hit>, id: HitId, x: f32, y: f32, w: f32, h: f32) {
    hits.push(Hit {
        id,
        rect: RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + h) as i32 },
    });
}

// ───────────────────────── 配置读写 ─────────────────────────

fn module_enabled(page: usize) -> bool {
    crate::app::with_cfg(|cfg| match page {
        1 => cfg.modules.health,
        2 => cfg.modules.salary,
        3 => cfg.modules.schedule,
        4 => cfg.modules.todo,
        5 => cfg.modules.filehub,
        6 => cfg.modules.clipboard,
        7 => cfg.modules.zones,
        8 => cfg.modules.mailcode,
        9 => cfg.modules.typing,
        _ => true,
    })
}

fn page_on(page: usize) -> bool {
    match page {
        1..=8 => module_enabled(page),
        // 图标页含「样式 / 声音」等与打字开关无关的通用配置，始终常亮
        _ => true,
    }
}

fn set_module_enabled(page: usize, on: bool) {
    crate::app::with_cfg(|cfg| match page {
        1 => cfg.modules.health = on,
        2 => cfg.modules.salary = on,
        3 => cfg.modules.schedule = on,
        4 => cfg.modules.todo = on,
        5 => cfg.modules.filehub = on,
        6 => cfg.modules.clipboard = on,
        7 => cfg.modules.zones = on,
        8 => cfg.modules.mailcode = on,
        9 => cfg.modules.typing = on,
        _ => {}
    });
    crate::app::save_now();
    crate::app::redraw();
    if page == 7 {
        crate::modules::zones::sync();
    }
}

fn edit_hwnd(page: usize, slot: i32) -> Option<HWND> {
    EDITS.with(|c| {
        c.borrow()
            .iter()
            .find(|(p, sl, _)| *p == page && *sl == slot)
            .map(|(_, _, h)| HWND(*h as *mut core::ffi::c_void))
    })
}

fn time_val_get(page: usize, slot: i32) -> String {
    TIME_VALS.with(|c| {
        c.borrow()
            .iter()
            .find(|((p, s), _)| *p == page && *s == slot)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    })
}

fn time_val_set(page: usize, slot: i32, txt: &str) {
    TIME_VALS.with(|c| {
        let mut m = c.borrow_mut();
        if let Some((_, v)) = m.iter_mut().find(|((p, s), _)| *p == page && *s == slot) {
            *v = txt.to_string();
        } else {
            m.push(((page, slot), txt.to_string()));
        }
    });
}

fn field_text(page: usize, slot: i32) -> String {
    if is_time_slot(page, slot) {
        return time_val_get(page, slot);
    }
    unsafe {
        match edit_hwnd(page, slot) {
            Some(h) => {
                let n = GetWindowTextLengthW(h) as usize;
                let mut buf = vec![0u16; n + 1];
                let got = GetWindowTextW(h, &mut buf) as usize;
                String::from_utf16_lossy(&buf[..got])
            }
            None => String::new(),
        }
    }
}

fn set_field(page: usize, slot: i32, txt: &str) {
    if is_time_slot(page, slot) {
        time_val_set(page, slot, txt);
        return;
    }
    unsafe {
        if let Some(h) = edit_hwnd(page, slot) {
            let _ = SetWindowTextW(h, &HSTRING::from(txt));
        }
    }
}

/// 判断某字段是否是自绘时间选择器（打工鼓励器页的 4 个时间字段）
fn is_time_slot(page: usize, slot: i32) -> bool {
    page == 2 && matches!(slot, 4 | 5 | 6 | 7)
}

fn flash(msg: &str) {
    STATUS.with(|c| *c.borrow_mut() = Some((msg.to_string(), Instant::now() + Duration::from_millis(1600))));
    let hwnd = WIN.with(|c| c.get());
    if hwnd != 0 {
        unsafe {
            SetTimer(Some(HWND(hwnd as *mut core::ffi::c_void)), ID_TIMER_STATUS, 1700, None);
        }
    }
}

// ───────────────────────── 应用改动 ─────────────────────────

fn apply_page(page: usize) {
    match page {
        0 => {
            // 通用页无文本配置（主题色由点击即时生效）
        }
        1 => {
            let a = field_text(1, 1).trim().parse::<u32>().ok();
            let b = field_text(1, 2).trim().parse::<u32>().ok();
            let d = field_text(1, 3).trim().parse::<u32>().ok();
            crate::app::with_cfg(|c| {
                if let Some(v) = a {
                    c.health.sedentary_minutes = v.clamp(5, 600);
                }
                if let Some(v) = b {
                    c.health.water_interval_minutes = v.clamp(5, 600);
                }
                if let Some(v) = d {
                    c.health.water_goal_cups = v.clamp(1, 30);
                }
            });
        }
        2 => {
            let m = field_text(2, 1).trim().parse::<f64>().ok();
            let hd = field_text(2, 2).trim().parse::<f64>().ok();
            let st = field_text(2, 4).trim().to_string();
            let ls = field_text(2, 6).trim().to_string();
            let le = field_text(2, 7).trim().to_string();
            let en = field_text(2, 5).trim().to_string();
            crate::app::with_cfg(|c| {
                if let Some(v) = m {
                    c.salary.monthly = v.clamp(0.0, 100_000_000.0);
                }
                if let Some(v) = hd {
                    c.salary.hours_per_day = v.clamp(0.5, 24.0);
                }
                if st.len() >= 4 {
                    c.salary.work_start = st;
                }
                if ls.len() >= 4 {
                    c.salary.lunch_start = ls;
                }
                if le.len() >= 4 {
                    c.salary.lunch_end = le;
                }
                if en.len() >= 4 {
                    c.salary.work_end = en;
                }
                // 时间区间颠倒时按「午休 12:00-13:00」兜底，避免状态判定错乱
                if c.salary.lunch_start_min() < c.salary.work_start_min()
                    || c.salary.lunch_end_min() <= c.salary.lunch_start_min()
                    || c.salary.lunch_end_min() > c.salary.work_end_min()
                {
                    c.salary.lunch_start = "12:00".into();
                    c.salary.lunch_end = "13:00".into();
                }
            });
        }
        6 => {
            if let Some(v) = field_text(6, 1).trim().parse::<usize>().ok() {
                crate::app::with_cfg(|c| c.clipboard_history = v.clamp(1, 200));
            }
        }
        5 => {
            if let Some(v) = field_text(5, 1).trim().parse::<usize>().ok() {
                // 立即截断超限的旧条目；save_now 在 apply_page 末尾统一落盘
                let _ = crate::app::set_filehub_cap(v);
            }
        }
        3 => {
            let lead = field_text(3, 2).trim().parse::<u32>().ok();
            let refresh = field_text(3, 4).trim().parse::<u32>().ok();
            // 飞书没有 ICS 订阅地址，UI 已移除该字段；ics_url 保留 config 原值不覆盖
            let feishu = (
                field_text(3, 12).trim().to_string(),
                field_text(3, 13).trim().to_string(),
                field_text(3, 14).trim().to_string(),
            );
            // 钉钉与飞书一样没有 ICS 订阅地址，UI 已移除该字段；ics_url 保留 config 原值不覆盖
            let dingtalk = (
                field_text(3, 22).trim().to_string(),
                field_text(3, 23).trim().to_string(),
                field_text(3, 24).trim().to_string(),
            );
            // 企业微信同样不提供 ICS 订阅地址，UI 已移除该字段；ics_url 保留 config 原值不覆盖
            let wecom = (
                field_text(3, 31).trim().to_string(),
                field_text(3, 32).trim().to_string(),
                field_text(3, 33).trim().to_string(),
            );
            crate::app::with_cfg(|c| {
                if let Some(v) = lead {
                    c.schedule_lead_minutes = v.min(120);
                }
                if let Some(v) = refresh {
                    c.schedule_sub.refresh_minutes = v.max(10).min(720);
                }
                let sub = &mut c.schedule_sub;
                sub.feishu.dav_server = feishu.0.clone();
                sub.feishu.dav_user = feishu.1.clone();
                sub.feishu.dav_pass = feishu.2.clone();
                sub.dingtalk.dav_server = dingtalk.0.clone();
                sub.dingtalk.dav_user = dingtalk.1.clone();
                sub.dingtalk.dav_pass = dingtalk.2.clone();
                sub.wecom.dav_server = wecom.0.clone();
                sub.wecom.dav_user = wecom.1.clone();
                sub.wecom.dav_pass = wecom.2.clone();
            });
        }
        8 => {
            let host = field_text(8, 1).trim().to_string();
            let port = field_text(8, 2).trim().parse::<u16>().ok();
            let user = field_text(8, 3).trim().to_string();
            let pass = field_text(8, 4).trim().to_string();
            let folder = field_text(8, 5).trim().to_string();
            let poll = field_text(8, 6).trim().parse::<u32>().ok();
            crate::app::with_cfg(|c| {
                c.mail.imap_host = host;
                if let Some(v) = port {
                    c.mail.imap_port = v;
                }
                c.mail.user = user;
                c.mail.password = pass;
                if !folder.is_empty() {
                    c.mail.folder = folder;
                }
                if let Some(v) = poll {
                    c.mail.poll_seconds = v.clamp(10, 600);
                }
            });
        }
        _ => {}
    }
    crate::app::save_now();
    crate::app::redraw();
    sync_page(page);
}

/// 快捷位置：顶部居中（x=-1 由渲染层换算为屏幕居中）
fn sync_page(page: usize) {
    let list = pages();
    let Some(p) = list.get(page) else { return };
    for r in p.rows.iter().filter(|r| r.kind == RowKind::Field || r.kind == RowKind::Time) {
        let v = crate::app::with_cfg(|c| match (page, r.slot) {
            (1, 1) => c.health.sedentary_minutes.to_string(),
            (1, 2) => c.health.water_interval_minutes.to_string(),
            (1, 3) => c.health.water_goal_cups.to_string(),
            (2, 1) => fmt_num(c.salary.monthly),
            (2, 2) => fmt_num(c.salary.hours_per_day),
            (2, 4) => c.salary.work_start.clone(),
            (2, 5) => c.salary.work_end.clone(),
            (2, 6) => c.salary.lunch_start.clone(),
            (2, 7) => c.salary.lunch_end.clone(),
            (6, 1) => c.clipboard_history.to_string(),
            (5, 1) => c.filehub_cap.to_string(),
            (3, 2) => c.schedule_lead_minutes.to_string(),
            (3, 4) => c.schedule_sub.refresh_minutes.to_string(),
            (3, 12) => c.schedule_sub.feishu.dav_server.clone(),
            (3, 13) => c.schedule_sub.feishu.dav_user.clone(),
            (3, 14) => c.schedule_sub.feishu.dav_pass.clone(),
            (3, 22) => c.schedule_sub.dingtalk.dav_server.clone(),
            (3, 23) => c.schedule_sub.dingtalk.dav_user.clone(),
            (3, 24) => c.schedule_sub.dingtalk.dav_pass.clone(),
            (3, 31) => c.schedule_sub.wecom.dav_server.clone(),
            (3, 32) => c.schedule_sub.wecom.dav_user.clone(),
            (3, 33) => c.schedule_sub.wecom.dav_pass.clone(),
            (8, 1) => c.mail.imap_host.clone(),
            (8, 2) => c.mail.imap_port.to_string(),
            (8, 3) => c.mail.user.clone(),
            (8, 4) => c.mail.password.clone(),
            (8, 5) => c.mail.folder.clone(),
            (8, 6) => c.mail.poll_seconds.to_string(),
            _ => String::new(),
        });
        set_field(page, r.slot, &v);
    }
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v}")
    }
}

/// 解析 "HH:MM" 为 (时, 分)，非法返回 None
fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

fn sync_all() {
    for i in 0..12 {
        sync_page(i);
    }
}

/// 提交所有页的输入框到配置（关闭窗口 / 点同步时用）。
/// 输入框失焦与切换页面不再自动提交——所有改动统一走这里。
fn apply_all() {
    let n = pages().len();
    for p in 0..n {
        apply_page(p);
    }
}

// ───────────────────────── 绘制 ─────────────────────────

fn draw_all(cv: &mut Canvas, w: f32, h: f32, page: usize, hover: HitId, hits: &mut Vec<Hit>) {
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
            if (1..=9).contains(p) {
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
            RowKind::Toggle | RowKind::Field | RowKind::Time | RowKind::Action | RowKind::PageRow | RowKind::Theme | RowKind::IconPick | RowKind::Sound | RowKind::Weekday
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
            let (name, on, mod_off) = crate::app::with_cfg(|c| match c.panel.pages.get(slot) {
                Some(p) => (crate::config::panel_name(&p.id), p.on, !c.modules.page_on(&p.id)),
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
        RowKind::IconPick => {
            let kinds = crate::modules::icon::ICON_KINDS;
            let n = kinds.len();
            let bw = s(72.0);
            let bh = s(44.0);
            let gap = s(8.0);
            let total = n as f32 * bw + (n as f32 - 1.0) * gap;
            let text_w = (inner_w - total - s(16.0)).max(s(120.0));
            cv.text1(inner_l, ly, text_w, s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, text_w, s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }

            let mut bx = cx + cw - s(18.0) - total;
            let by = ry + (rh - bh) / 2.0;
            let cur = crate::app::with_cfg(|c| crate::modules::icon::index(&c.icon));
            for (i, (id, label)) in kinds.iter().enumerate() {
                let sel = i == cur;
                let hov = hover == HitId::Icon(i);
                let base = if sel {
                    crate::anim::mix(CARD, crate::theme::main(), 0.20)
                } else if hov {
                    0x23232B
                } else {
                    0x1B1B22
                };
                cv.rrect(bx, by, bw, bh, s(9.0), base, ta);
                cv.rrect_stroke(
                    bx + 0.5, by + 0.5, bw - 1.0, bh - 1.0, s(9.0),
                    if sel { crate::theme::main() } else if hov { 0x3C3C4A } else { CARD_BD },
                    if sel { 0.95 } else { 0.85 }, s(1.2),
                );
                // 卡片内实时预览图标：固定相位 + 中高能量，直观看到每种形态
                let sig = crate::modules::icon::Signal { energy: 0.62, phase: 1.7 + i as f32 * 0.45 };
                crate::modules::icon::draw_icon(cv, *id, bx + bw / 2.0, by + s(17.0), s(34.0), s(26.0), sig, ta);
                cv.text_c(bx, by + s(31.0), bw, s(12.0), *label, 9.0,
                    if sel { W_SEMI } else { W_MED }, if sel { TXT } else { TXT2 }, ta);
                push_hit(hits, HitId::Icon(i), bx, by, bw, bh);
                bx += bw + gap;
            }
        }
        RowKind::Sound => {
            cv.text1(inner_l, ly, inner_w - s(64.0), s(19.0), row.label, 13.0, W_MED, TXT, ta);
            if has_desc {
                cv.text1(inner_l, dy, inner_w - s(64.0), s(16.0), row.desc, 11.0, W_NORMAL, TXT3, ta);
            }
            let on = crate::app::with_cfg(|c| c.sound_react);
            let sw = s(44.0);
            let sh = s(26.0);
            let sx = cx + cw - s(18.0) - sw;
            let sy = ry + (rh - sh) / 2.0;
            draw_switch(cv, sx, sy, sw, sh, on, hover == HitId::Sound, ta);
            push_hit(hits, HitId::Sound, sx - s(6.0), sy - s(6.0), sw + s(12.0), sh + s(12.0));
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

fn draw_hero(cv: &mut Canvas, x: f32, y: f32, w: f32, h: f32) {
    cv.rrect_grad(x, y, w, h, s(CARD_R), crate::theme::dark(), 0x15151A, true, 1.0);
    cv.rrect_stroke(x + 0.5, y + 0.5, w - 1.0, h - 1.0, s(CARD_R), crate::theme::light(), 0.9, s(1.0));

    let bs = s(46.0);
    let bx = x + s(20.0);
    let by = y + (h - bs) / 2.0;
    cv.rrect_grad(bx, by, bs, bs, s(13.0), crate::theme::light(), crate::theme::dark(), false, 1.0);
    cv.rrect(bx + bs * 0.28, by + bs * 0.42, bs * 0.44, s(6.0), s(3.0), 0xFFFFFF, 0.95);
    cv.circle(bx + bs * 0.78, by + bs * 0.44, s(2.4), 0xFFFFFF, 0.9);

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

// ───────────────────────── 窗口 / 控件 ─────────────────────────

fn client_size() -> (i32, i32) {
    (si(WIN_W), si(WIN_H))
}

pub fn open() {
    unsafe {
        let existing = WIN.with(|c| c.get());
        if existing != 0 {
            let hwnd = HWND(existing as *mut core::ffi::c_void);
            sync_all();
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            invalidate(hwnd);
            crate::app::set_island_topmost(false);
            return;
        }

        let hinst = GetModuleHandleW(None).unwrap();
        ensure_class(hinst.into());

        let (cw, ch) = client_size();
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("WinIslandSettings"),
            w!("WinIsland 设置"),
            WS_POPUP | WS_THICKFRAME,
            (sw - cw) / 2,
            (sh - ch) / 2,
            cw,
            ch,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("CreateWindowExW settings");

        // Win11：圆角 + 去掉系统边框线，交给自绘
        let pref: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let no_border: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &no_border as *const _ as *const core::ffi::c_void, 4);

        WIN.with(|c| c.set(hwnd.0 as isize));
        build_edits(hwnd);
        sync_all();
        show_page(hwnd, 0);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        crate::app::set_island_topmost(false);
    }
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe fn ensure_class(hinst: HINSTANCE) {
    if CLASS_ON.with(|c| c.get()) {
        return;
    }
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: w!("WinIslandSettings"),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);
    CLASS_ON.with(|c| c.set(true));
}

unsafe fn build_edits(parent: HWND) {
    let font = CreateFontW(
        -((13.0 * s(1.0)).round() as i32),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        DEFAULT_PITCH.0 as u32,
        w!("Microsoft YaHei UI"),
    );
    EDIT_FONT.with(|c| c.set(font.0 as isize));
    let brush = CreateSolidBrush(cref(INPUT_BG));
    EDIT_BRUSH.with(|c| c.set(brush.0 as isize));

    let list = pages();
    let cx = s(SIDE_W + PAD);
    let cw = s(WIN_W - SIDE_W - PAD * 2.0);

    for (page_idx, page) in list.iter().enumerate() {
        let lay = layout(page);
        for pl in lay.placed.iter() {
            let row = &page.rows[pl.row];
            if row.kind != RowKind::Field {
                continue;
            }
            let bw = s(row.fw);
            let bx = cx + cw - s(18.0) - bw;
            let by = s(pl.y) + (s(ROW_H) - s(INPUT_H)) / 2.0;
            let suffix_w = if row.suffix.is_empty() { 0.0 } else { tw(row.suffix, 10.5) + s(8.0) };

            let mut style: i32 = ES_LEFT | ES_AUTOHSCROLL;
            if row.int {
                style |= ES_NUMBER;
            }
            if row.pwd {
                style |= ES_PASSWORD;
            }
            let h = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WINDOW_STYLE(style as u32) | WS_CHILD | WS_TABSTOP,
                (bx + s(11.0)) as i32,
                (by + s(5.0)) as i32,
                (bw - s(22.0) - suffix_w).max(s(30.0)) as i32,
                (s(INPUT_H) - s(10.0)) as i32,
                Some(parent),
                Some(HMENU((page_idx * 10 + row.slot as usize) as *mut core::ffi::c_void)),
                Some(GetModuleHandleW(None).unwrap().into()),
                None,
            )
            .expect("CreateWindowExW edit");

            let _ = SendMessageW(h, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            let _ = SetWindowTheme(h, w!(""), w!(""));
            let _ = SendMessageW(h, EM_SETMARGINS, Some(WPARAM(EC_LEFTMARGIN | EC_RIGHTMARGIN)), Some(LPARAM(0)));
            if !row.placeholder.is_empty() {
                let wide = HSTRING::from(row.placeholder);
                let _ = SendMessageW(h, EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(wide.as_ptr() as isize)));
            }
            let _ = ShowWindow(h, SW_HIDE);
            EDITS.with(|c| c.borrow_mut().push((page_idx, row.slot, h.0 as isize)));
        }
    }
}

/// 按当前滚动偏移摆放当前页的原生输入框（自绘外框同步移动）
unsafe fn place_edits(page: usize) {
    let off = SCROLL.with(|c| c.get());
    let list = pages();
    let Some(p) = list.get(page) else { return };
    let lay = layout(p);
    let cx = s(SIDE_W + PAD);
    let cw = s(WIN_W - SIDE_W - PAD * 2.0);
    // 输入框是独立 HWND，不受 D2D 内容裁剪约束。策略：只有当输入框**完整**落在
    // 内容视口内才显示；部分越界或完全越界一律隐藏（外框在 draw_row 同步不画），
    // 避免滚动时出现被平切一半的破碎框。判定基准与 draw_row 一致：外框矩形
    // （HWND 顶部 = 外框顶 +5、高 INPUT_H-10）。
    let vp_top = s(130.0);
    let vp_bot = s(WIN_H - FOOT_H);
    for pl in lay.placed.iter() {
        let row = &p.rows[pl.row];
        if row.kind != RowKind::Field {
            continue;
        }
        let Some(h) = edit_hwnd(page, row.slot) else { continue };
        let bw = s(row.fw);
        let bx = cx + cw - s(18.0) - bw + s(11.0);
        let ftop = s(pl.y - off) + (s(pl.h) - s(INPUT_H)) / 2.0; // 外框顶
        let fbot = ftop + s(INPUT_H);
        let hw = h;
        if ftop < vp_top || fbot > vp_bot {
            let _ = ShowWindow(hw, SW_HIDE);
        } else {
            let by = ftop + s(5.0);
            let _ = SetWindowPos(hw, None, bx as i32, by as i32, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            let _ = ShowWindow(hw, SW_SHOW);
        }
    }
}

unsafe fn show_page(hwnd: HWND, page: usize) {
    PAGE.with(|c| c.set(page));
    SCROLL.with(|c| c.set(0.0));
    EDITS.with(|c| {
        for (p, _, h) in c.borrow().iter() {
            let _ = ShowWindow(HWND(*h as *mut core::ffi::c_void), if *p == page { SW_SHOW } else { SW_HIDE });
        }
    });
    place_edits(page); // 放在显隐循环之后：按视口位置最终决定显示/隐藏
    invalidate(hwnd);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right.max(1), rc.bottom.max(1));

    let page = PAGE.with(|c| c.get());
    let hover = HOVER.with(|c| c.get());
    let mut hits: Vec<Hit> = Vec::new();

    CANVAS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Canvas::new().ok();
        }
        if let Some(cv) = slot.as_mut() {
            let memdc = cv.draw_to_mem(w, h, BG, |cv, w, h| {
                draw_all(cv, w, h, page, hover, &mut hits);
            });
            let _ = BitBlt(hdc, 0, 0, w, h, Some(memdc), 0, 0, SRCCOPY);
        }
    });

    HITS.with(|c| *c.borrow_mut() = hits);
    let _ = EndPaint(hwnd, &ps);
}

/// 把键盘输入缓冲同步到时/分数值；缓冲为空时保留原值。
fn time_apply_buf(part: usize) {
    let txt = TIME_BUF.with(|b| b.borrow()[part].clone());
    if txt.is_empty() {
        return;
    }
    if let Ok(v) = txt.parse::<u32>() {
        if part == 0 {
            TIME_H.with(|c| c.set(v.min(23)));
        } else {
            TIME_M.with(|c| c.set(v.min(59)));
        }
    }
}

/// 键盘输入一位数字。
/// 缓冲只存用户真实敲入的位（1~2 位）；若该位本身已超十位上限（时的 3~9 / 分的 6~9），
/// 则它必然是个位，立即完成该列（时列自动跳到分）。满两位或该位越十位时，
/// 把缓冲规范成两位显示并钳制数值；此后再输入视为重新开始一位。
fn time_type_digit(part: usize, ch: char) {
    if !ch.is_ascii_digit() {
        return;
    }
    let max: u32 = if part == 0 { 23 } else { 59 };
    let digit = ch.to_digit(10).unwrap_or(0);
    let mut buf = TIME_BUF.with(|b| b.borrow()[part].clone());
    // 已完成状态（满两位，或首位已超十位）下再输入 → 从新的一位开始
    let first_over = buf
        .chars()
        .next()
        .map(|c| c.to_digit(10).unwrap_or(0) > max / 10)
        .unwrap_or(false);
    if buf.len() >= 2 || first_over {
        buf.clear();
    }
    buf.push(ch);
    let value = buf.parse::<u32>().unwrap_or(0).min(max);
    let this_over = digit > max / 10;
    if buf.len() >= 2 || this_over {
        // 该列完成：规范成两位、钳制数值、时列跳到分
        buf = format!("{value:02}");
        if part == 0 {
            TIME_EDIT.with(|c| c.set(Some(1)));
        }
    }
    TIME_BUF.with(|b| b.borrow_mut()[part] = buf);
    time_apply_buf(part);
}

/// 退格：删掉当前缓冲最后一位。
fn time_backspace(part: usize) {
    let mut buf = TIME_BUF.with(|b| b.borrow()[part].clone());
    buf.pop();
    TIME_BUF.with(|b| b.borrow_mut()[part] = buf);
    time_apply_buf(part);
}

/// 确定时间选择并关闭浮层。
fn confirm_time(hwnd: HWND) {
    if let Some((page, slot)) = TIME_PICK.with(|c| c.get()) {
        let hh = TIME_H.with(|c| c.get());
        let mm = TIME_M.with(|c| c.get());
        set_field(page, slot, &format!("{hh:02}:{mm:02}"));
        flash("时间已选择 · 点底部「保存并应用」生效");
    }
    TIME_PICK.with(|c| c.set(None));
    TIME_EDIT.with(|c| c.set(None));
    invalidate(hwnd);
}

fn hit_at(x: i32, y: i32) -> HitId {
    HITS.with(|c| {
        c.borrow()
            .iter()
            .rev()
            .find(|h| x >= h.rect.left && x <= h.rect.right && y >= h.rect.top && y <= h.rect.bottom)
            .map(|h| h.id)
            .unwrap_or(HitId::None)
    })
}

fn on_click(hwnd: HWND, id: HitId) {
    match id {
        HitId::Nav(p) => {
            let cur = PAGE.with(|c| c.get());
            if cur != p {
                // 切页不自动提交输入：所有改动统一由底部「保存并应用」按钮保存并生效
                unsafe { show_page(hwnd, p) };
            }
        }
        HitId::Toggle(page, slot) => {
            if page == 7 && slot == 1 {
                // 桌面分区：开机自启（真正写入系统启动项）
                let want = crate::app::with_cfg(|c| !c.zones_autostart);
                if crate::modules::autostart::set_enabled(want) {
                    crate::app::with_cfg(|c| c.zones_autostart = want);
                    crate::app::save_now();
                    crate::app::redraw();
                    flash(if want { "已设置开机自启" } else { "已关闭开机自启" });
                } else {
                    flash("未能写入开机自启项");
                }
            } else if page == 0 && slot == 0 {
                let want = !crate::app::with_cfg(|c| c.autostart);
                if crate::modules::autostart::set_enabled(want) {
                    crate::app::with_cfg(|c| c.autostart = want);
                    crate::app::save_now();
                    flash("已保存");
                } else {
                    flash("未能写入开机自启项");
                }
            } else if page == 0 && slot == 1 {
                let on = crate::app::with_cfg(|c| {
                    c.auto_collapse = !c.auto_collapse;
                    c.auto_collapse
                });
                crate::app::save_now();
                crate::app::redraw();
                flash(if on { "已开启遮挡自动收起" } else { "已改为常驻面板" });
            } else if page == 3 && matches!(slot, 10 | 20 | 30) {
                // 三个来源各自的启用开关
                let on = crate::app::with_cfg(|c| {
                    let s = match slot {
                        10 => &mut c.schedule_sub.feishu,
                        20 => &mut c.schedule_sub.dingtalk,
                        _ => &mut c.schedule_sub.wecom,
                    };
                    s.on = !s.on;
                    s.on
                });
                crate::app::save_now();
                crate::app::redraw();
                let name = match slot { 10 => "飞书日历", 20 => "钉钉日历", _ => "企业微信日历" };
                let msg = if on { format!("{name}已启用") } else { format!("{name}已停用") };
                flash(&msg);
            } else if page == 11 {
                crate::app::with_cfg(|c| {
                    if let Some(p) = c.panel.pages.get_mut(slot as usize) {
                        p.on = !p.on;
                    }
                });
                crate::app::save_now();
                crate::app::redraw();
                flash("已保存");
            } else {
                let want = !module_enabled(page);
                set_module_enabled(page, want);
                flash(if want { "模块已开启" } else { "模块已关闭" });
            }
            invalidate(hwnd);
        }
        HitId::Btn(page, slot) => {
            match (page, slot) {
                (4, 1) => crate::app::open_data_file(),
                (8, 7) => {
                    apply_page(8);
                    flash("已保存，后台会立即按新参数重连");
                }
                (8, 8) => {
                    // 先落盘再测，否则测的是旧参数
                    apply_page(8);
                    crate::modules::mail::test_connection_async();
                    flash("正在测试连接…状态显示在下方");
                    unsafe {
                        SetTimer(Some(hwnd), ID_TIMER_MAIL, 500, None);
                    }
                }
                (3, 5) => {
                    apply_page(3);
                    let sub = crate::app::with_cfg(|c| c.schedule_sub.clone());
                    let any = sub.feishu.configured()
                        || sub.dingtalk.configured()
                        || sub.wecom.configured();
                    if !any {
                        flash("请先配置至少一个来源（飞书 / 钉钉 / 企业微信）");
                    } else {
                        crate::modules::sched_ics::sync_all_async(&sub);
                        unsafe {
                            SetTimer(Some(hwnd), ID_TIMER_ICS, 600, None);
                        }
                        flash("正在同步外部日历…");
                    }
                }
                (11, slot) => {
                    // 上移 / 下移：槽位偶数=上移，奇数=下移
                    let i = (slot / 2) as usize;
                    let dir: i32 = if slot % 2 == 0 { -1 } else { 1 };
                    let mut moved = false;
                    crate::app::with_cfg(|c| {
                        let n = c.panel.pages.len();
                        let j = i as i32 + dir;
                        if i < n && j >= 0 && (j as usize) < n {
                            c.panel.pages.swap(i, j as usize);
                            moved = true;
                        }
                    });
                    if moved {
                        crate::app::save_now();
                        crate::app::redraw();
                        flash("顺序已更新");
                    }
                }
                (10, 0) => crate::app::open_config_file(),
                (10, 1) => crate::app::open_data_dir(),
                _ => {}
            }
            invalidate(hwnd);
        }
        HitId::Theme(i) => {
            crate::theme::set_index(i);
            crate::app::with_cfg(|c| c.theme = crate::theme::current_id().to_string());
            crate::app::save_now();
            crate::app::redraw(); // 岛立即换色
            flash("主题已更新");
            invalidate(hwnd);
        }
        HitId::Icon(i) => {
            if let Some((id, _)) = crate::modules::icon::ICON_KINDS.get(i) {
                crate::app::with_cfg(|c| c.icon = (*id).to_string());
                crate::app::save_now();
                crate::app::redraw(); // 岛立即换图标
                flash(&format!("图标已切换为「{}」", crate::modules::icon::name(*id)));
                invalidate(hwnd);
            }
        }
        HitId::Sound => {
            let on = crate::app::with_cfg(|c| {
                c.sound_react = !c.sound_react;
                c.sound_react
            });
            crate::app::save_now();
            crate::app::redraw();
            flash(if on { "已开启声音响应" } else { "已关闭声音响应" });
            invalidate(hwnd);
        }
        HitId::Sync => {
            // 统一提交所有页的输入框，保存并让面板立即用上新配置
            apply_all();
            crate::app::save_now();
            crate::app::reload_runtime();
            flash("已保存并应用到面板");
            invalidate(hwnd);
        }
        HitId::Time(page, slot) => {
            // 打开选择器：读取当前值（草稿优先，其次配置）初始化时/分
            let mut val = field_text(page, slot);
            if val.trim().is_empty() {
                val = crate::app::with_cfg(|c| match slot {
                    4 => c.salary.work_start.clone(),
                    5 => c.salary.work_end.clone(),
                    6 => c.salary.lunch_start.clone(),
                    _ => c.salary.lunch_end.clone(),
                });
            }
            let (hh, mm) = parse_hhmm(&val).unwrap_or((9, 0));
            TIME_H.with(|c| c.set(hh));
            TIME_M.with(|c| c.set(mm));
            TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
            TIME_EDIT.with(|c| c.set(None));
            TIME_PICK.with(|c| c.set(Some((page, slot))));
            invalidate(hwnd);
        }
        HitId::TimeStep(part, dir) => {
            // 用 ▲▼ 时退出键盘输入态，并以当前数值为准步进
            TIME_EDIT.with(|c| c.set(None));
            TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
            if part == 0 {
                TIME_H.with(|c| {
                    let v = c.get() as i32 + dir;
                    c.set((((v % 24) + 24) % 24) as u32);
                });
            } else {
                TIME_M.with(|c| {
                    let v = c.get() as i32 + dir;
                    c.set((((v % 60) + 60) % 60) as u32);
                });
            }
            invalidate(hwnd);
        }
        HitId::TimeNum(part) => {
            // 点数字框：进入键盘输入态，缓冲置空（首键即从 0 开始），并设置焦点接收字符
            TIME_EDIT.with(|c| c.set(Some(part)));
            TIME_BUF.with(|b| b.borrow_mut()[part].clear());
            unsafe {
                let hwnd_now = WIN.with(|c| c.get());
                if hwnd_now != 0 {
                    let _ = SetFocus(Some(HWND(hwnd_now as *mut core::ffi::c_void)));
                }
            }
            invalidate(hwnd);
        }
        HitId::TimeOk => confirm_time(hwnd),
        HitId::TimeCancel => {
            TIME_PICK.with(|c| c.set(None));
            TIME_EDIT.with(|c| c.set(None));
            invalidate(hwnd);
        }
        HitId::Weekday(i) => {
            crate::app::with_cfg(|c| {
                let mut w = c.salary.weekdays_view();
                w[i] = !w[i];
                if !w.iter().any(|v| *v) {
                    w[i] = true; // 至少保留一天，否则永远没有工作日
                }
                c.salary.weekdays = w.to_vec();
            });
            crate::app::save_now();
            crate::app::redraw(); // 状态徽标可能立刻变化
            flash("工作日已更新");
            invalidate(hwnd);
        }
        HitId::Min => unsafe {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
            // 最小化后设置窗口不再是前台视觉主体，把置顶还给灵动岛，
            // 否则岛会一直停在普通层，被浏览器等窗口压住（小黑条也会看不见）
            crate::app::set_island_topmost(true);
        },
        HitId::Close => unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        HitId::None => {}
    }
}

fn hide_and_restore(hwnd: HWND) {
    apply_all(); // 关闭窗口时兜底提交所有页输入，避免用户改了没点同步就丢失
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    crate::app::set_island_topmost(true);
}

/// 退出灵动岛时调用：同步把设置窗口藏掉（并保存当前页输入），
/// 确保主窗口销毁后屏幕上不残留设置窗口
pub fn close_for_quit() {
    let hwnd = WIN.with(|c| c.get());
    if hwnd != 0 {
        hide_and_restore(HWND(hwnd as *mut core::ffi::c_void));
    }
}

fn hover_set(hwnd: HWND, id: HitId) {
    let changed = HOVER.with(|c| {
        if c.get() == id {
            false
        } else {
            c.set(id);
            true
        }
    });
    if changed {
        invalidate(hwnd);
    }
}

unsafe fn update_hover(hwnd: HWND, lparam: LPARAM) {
    let x = (lparam.0 & 0xFFFF) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
    hover_set(hwnd, hit_at(x, y));

    if !TRACKING.with(|c| c.get()) {
        TRACKING.with(|c| c.set(true));
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme as *mut TRACKMOUSEEVENT);    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0), // 去掉系统边框，客户区铺满窗口
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xFFFF) != 0;
            if active {
                // 设置窗口获得激活（含从最小化还原）→ 灵动岛让位，避免压住设置界面
                if !unsafe { IsIconic(hwnd).as_bool() } {
                    crate::app::set_island_topmost(false);
                }
            } else {
                // 失去激活（被浏览器等窗口盖住、或切到别的程序）→ 把置顶还给岛。
                // 否则设置窗只是「还开着」就足以让岛永久沉到普通层，小黑条被浏览器压掉。
                crate::app::set_island_topmost(true);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ID_TIMER_STATUS => {
            STATUS.with(|c| *c.borrow_mut() = None);
            let _ = KillTimer(Some(hwnd), ID_TIMER_STATUS);
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ID_TIMER_MAIL => {
            invalidate(hwnd);
            let st = crate::modules::mail::last_status();
            if !st.starts_with("正在测试") {
                let _ = KillTimer(Some(hwnd), ID_TIMER_MAIL);
                if !st.is_empty() {
                    flash(&st);
                }
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == ID_TIMER_ICS => {
            // 订阅日历后台拉取期间刷新状态行；结束时把结果写进状态栏
            invalidate(hwnd);
            if crate::modules::sched_ics::sync_state() != crate::modules::sched_ics::SYNC_RUN {
                let _ = KillTimer(Some(hwnd), ID_TIMER_ICS);
                let note = crate::modules::sched_ics::sync_note();
                if !note.is_empty() {
                    flash(&note);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            update_hover(hwnd, lparam);
            LRESULT(0)
        }
        0x020A => {
            // 时间选择面板打开时不滚动背景
            if TIME_PICK.with(|c| c.get()).is_some() {
                return LRESULT(0);
            }
            // WM_MOUSEWHEEL：滚动右侧配置区（原生输入框跟随移动）
            let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
            let cur = PAGE.with(|c| c.get());
            let list = pages();
            if let Some(p) = list.get(cur) {
                let lay = layout(p);
                let bottom = lay.placed.iter().map(|pl| pl.y + pl.h).fold(0.0f32, f32::max) + 20.0;
                let max = (bottom - (WIN_H - FOOT_H)).max(0.0);
                SCROLL.with(|c| c.set((c.get() - delta as f32 / 120.0 * 52.0).clamp(0.0, max)));
                place_edits(cur);
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // 点空白处收起输入焦点（触发 EN_KILLFOCUS 保存）
            let _ = SetFocus(Some(hwnd));
            LRESULT(0)
        }
        0x02A3 => {
            // WM_MOUSELEAVE（windows crate 未导出该常量）
            TRACKING.with(|c| c.set(false));
            hover_set(hwnd, HitId::None);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let id = hit_at(x, y);
            on_click(hwnd, id);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let hov = HOVER.with(|c| c.get());
            let cur = if hov.clickable() { LoadCursorW(None, IDC_HAND) } else { LoadCursorW(None, IDC_ARROW) };
            let _ = SetCursor(cur.ok());
            LRESULT(1)
        }
        WM_NCHITTEST => {
            // 顶部 titlebar 可拖动；右上角按钮区仍交给客户区
            let sx = (lparam.0 & 0xFFFF) as i16 as i32;
            let sy = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            if pt.y >= 0 && (pt.y as f32) < s(TITLE_H) {
                let bw = s(WIN_BTN);
                let min_x = si(WIN_W) as f32 - s(16.0) - bw - s(6.0) - bw;
                if (pt.x as f32) < min_x - s(6.0) {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            LRESULT(HTCLIENT as isize)
        }
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, cref(TXT));
            SetBkColor(hdc, cref(INPUT_BG));
            let _ = SetBkMode(hdc, TRANSPARENT);
            LRESULT(EDIT_BRUSH.with(|c| c.get()))
        }
        WM_COMMAND => {
            let code = ((wparam.0 >> 16) & 0xFFFF) as u32;
            if code == EN_SETFOCUS_CODE {
                invalidate(hwnd);
            } else if code == EN_KILLFOCUS_CODE {
                // 失焦不再自动保存/生效：所有修改统一由底部「保存并应用」按钮提交
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        0x0102 => {
            // WM_CHAR：时间选择面板里，数字直接输入到当前编辑列
            if let Some(part) = TIME_EDIT.with(|c| c.get()) {
                let ch = char::from_u32((wparam.0 & 0xFFFF) as u32).unwrap_or('\0');
                if ch.is_ascii_digit() {
                    time_type_digit(part, ch);
                } else if ch == '\r' {
                    // 回车确定
                    confirm_time(hwnd);
                    return LRESULT(0);
                }
                invalidate(hwnd);
                return LRESULT(0);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            const VK_TAB: usize = 0x09;
            const VK_BACK: usize = 0x08;
            const VK_RETURN: usize = 0x0D;
            const VK_UP: usize = 0x26;
            const VK_DOWN: usize = 0x28;
            // 时间选择面板打开时的键盘操作
            if TIME_PICK.with(|c| c.get()).is_some() {
                if wparam.0 == 0x1B {
                    // Esc：关闭时间选择面板（不保存）
                    TIME_PICK.with(|c| c.set(None));
                    TIME_EDIT.with(|c| c.set(None));
                    invalidate(hwnd);
                    return LRESULT(0);
                }
                if wparam.0 == VK_TAB {
                    // Tab：在时/分之间切换输入列
                    let cur = TIME_EDIT.with(|c| c.get()).unwrap_or(0);
                    TIME_EDIT.with(|c| c.set(Some(1 - cur)));
                    invalidate(hwnd);
                    return LRESULT(0);
                }
                if wparam.0 == VK_BACK {
                    if let Some(part) = TIME_EDIT.with(|c| c.get()) {
                        time_backspace(part);
                        invalidate(hwnd);
                    }
                    return LRESULT(0);
                }
                if wparam.0 == VK_RETURN {
                    confirm_time(hwnd);
                    return LRESULT(0);
                }
                if wparam.0 == VK_UP || wparam.0 == VK_DOWN {
                    // 上下键：沿用 ▲▼ 步进（不受是否在输入态影响）
                    let part = TIME_EDIT.with(|c| c.get()).unwrap_or(0);
                    let dir = if wparam.0 == VK_UP { 1 } else { -1 };
                    TIME_EDIT.with(|c| c.set(None));
                    TIME_BUF.with(|b| *b.borrow_mut() = [String::new(), String::new()]);
                    if part == 0 {
                        TIME_H.with(|c| {
                            let v = c.get() as i32 + dir;
                            c.set((((v % 24) + 24) % 24) as u32);
                        });
                    } else {
                        TIME_M.with(|c| {
                            let v = c.get() as i32 + dir;
                            c.set((((v % 60) + 60) % 60) as u32);
                        });
                    }
                    invalidate(hwnd);
                    return LRESULT(0);
                }
            }
            if wparam.0 == 0x1B {
                // Esc：没有时间面板时关闭设置窗口
                hide_and_restore(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            hide_and_restore(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let brush = EDIT_BRUSH.with(|c| c.get());
            if brush != 0 {
                let _ = DeleteObject(HBRUSH(brush as *mut core::ffi::c_void).into());
            }
            let font = EDIT_FONT.with(|c| c.get());
            if font != 0 {
                let _ = DeleteObject(HFONT(font as *mut core::ffi::c_void).into());
            }
            WIN.with(|c| c.set(0));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
