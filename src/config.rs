//! 配置文件：所有功能均可自定义（%APPDATA%\WinIsland\config.json）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Modules {
    pub clock: bool,
    pub salary: bool,
    pub health: bool,
    pub typing: bool,
    pub schedule: bool,
    pub todo: bool,
    pub clipboard: bool,
    pub filehub: bool,
    pub zones: bool,
    pub mailcode: bool,
    /// 盯盘助手：自选实时行情 / 涨跌提醒 / 今日盈亏
    pub stock: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Modules {
            clock: true,
            salary: true,
            health: true,
            typing: true,
            schedule: true,
            todo: true,
            clipboard: true,
            filehub: true,
            zones: true,
            mailcode: false, // 需要用户自行配置 IMAP 后开启
            stock: true,     // 无自选时仅显示引导，不联网；配好自选后自动轮询
        }
    }
}

impl Modules {
    /// 面板轮播页 id 对应的模块是否开启（决定该页是否参与轮播）
    pub fn page_on(&self, id: &str) -> bool {
        match id {
            "schedule" => self.schedule,
            "todo" => self.todo,
            "files" => self.filehub,
            "clips" => self.clipboard,
            "health" => self.health,
            "salary" => self.salary,
            "watch" => self.stock,
            _ => true, // overview
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Salary {
    pub monthly: f64,
    pub hours_per_day: f64,
    pub workdays_per_month: f64,
    pub work_start: String,  // "09:00" 上午上班
    pub work_end: String,    // "18:00" 下班
    pub lunch_start: String, // "12:00" 午休开始
    pub lunch_end: String,   // "13:00" 下午上班
    /// 自定义工作日（7 项，索引 0 = 周一）。空 = 未自定义，按法定工作日判定
    pub weekdays: Vec<bool>,
    /// 联网同步官方节假日 / 调休（只下载公开日历数据，不上传任何信息）
    pub online_calendar: bool,
}

impl Salary {
    /// 自定义工作日集合：仅在长度为 7 且至少勾选一天时生效
    pub fn custom_weekdays(&self) -> Option<[bool; 7]> {
        if self.weekdays.len() != 7 || !self.weekdays.iter().any(|v| *v) {
            return None;
        }
        let mut a = [false; 7];
        for (i, v) in self.weekdays.iter().enumerate().take(7) {
            a[i] = *v;
        }
        Some(a)
    }

    /// 界面展示用：未自定义时按「周一至周五」显示
    pub fn weekdays_view(&self) -> [bool; 7] {
        self.custom_weekdays().unwrap_or([true, true, true, true, true, false, false])
    }

    /// 上午上班时间（分钟，自 0 点）
    pub fn work_start_min(&self) -> f64 {
        crate::modules::clock::parse_hm(&self.work_start).unwrap_or(9 * 60) as f64
    }
    /// 午休开始
    pub fn lunch_start_min(&self) -> f64 {
        crate::modules::clock::parse_hm(&self.lunch_start).unwrap_or(12 * 60) as f64
    }
    /// 下午上班（午休结束）
    pub fn lunch_end_min(&self) -> f64 {
        crate::modules::clock::parse_hm(&self.lunch_end).unwrap_or(13 * 60) as f64
    }
    /// 下班时间
    pub fn work_end_min(&self) -> f64 {
        crate::modules::clock::parse_hm(&self.work_end).unwrap_or(18 * 60) as f64
    }
}

impl Default for Salary {
    fn default() -> Self {
        Salary {
            monthly: 12000.0,
            hours_per_day: 8.0,
            workdays_per_month: 22.0,
            work_start: "09:00".into(),
            work_end: "18:00".into(),
            lunch_start: "12:00".into(),
            lunch_end: "13:00".into(),
            weekdays: Vec::new(),
            online_calendar: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Health {
    pub sedentary_minutes: u32,
    pub water_interval_minutes: u32,
    pub water_goal_cups: u32,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            sedentary_minutes: 60,
            water_interval_minutes: 45,
            water_goal_cups: 8,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Mail {
    pub imap_host: String,
    pub imap_port: u16,
    pub user: String,
    pub password: String,
    pub folder: String,
    pub poll_seconds: u32,
}

impl Default for Mail {
    fn default() -> Self {
        Mail {
            imap_host: String::new(),
            imap_port: 993,
            user: String::new(),
            password: String::new(),
            folder: "INBOX".into(),
            poll_seconds: 20,
        }
    }
}

// ───────── 盯盘助手 ─────────

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Stock {
    /// 拉取实时行情的间隔（秒）
    pub refresh_secs: u32,
    /// 涨跌提醒阈值（百分比绝对值）；0 = 不提醒
    pub alert_pct: f64,
    /// 交易时段外是否仍持续轮询（默认只在交易时段拉取，省流量）
    pub off_hours: bool,
}

impl Default for Stock {
    fn default() -> Self {
        Stock {
            refresh_secs: 10,
            alert_pct: 0.0,
            off_hours: false,
        }
    }
}

/// 盯盘数据文件（%APPDATA%\WinIsland\watchlist.txt）的路径。
/// 每行一条：`代码 名称 成本价 数量`；名称可留空自动获取，成本价+数量>0 计入今日盈亏。
pub fn watchlist_path() -> PathBuf {
    data_dir().join("watchlist.txt")
}

// ───────── 面板轮播页 ─────────

/// 面板可轮播页面的固定清单（id, 展示名）。顺序即默认轮播顺序。
/// id 一旦发布不可改（用户配置里存的是 id）。
pub const PANEL_KINDS: [(&str, &str); 8] = [
    ("overview", "今日概览"),
    ("schedule", "日程安排"),
    ("todo", "待办清单"),
    ("files", "文件中转站"),
    ("clips", "剪贴板中转"),
    ("health", "健康提醒"),
    ("salary", "打工鼓励器"),
    ("watch", "盯盘助手"),
];

/// id -> 展示名（未知 id 返回 "其他"）
#[allow(dead_code)]
pub fn panel_name(id: &str) -> &'static str {
    PANEL_KINDS
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, n)| *n)
        .unwrap_or("其他")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct PanelPage {
    /// PANEL_KINDS 中的 id
    pub id: String,
    /// 是否参与轮播（模块本身关闭时也会自动跳过）
    pub on: bool,
}

impl PanelPage {
    pub fn new(id: &str, on: bool) -> PanelPage {
        PanelPage { id: id.to_string(), on }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Panel {
    pub pages: Vec<PanelPage>,
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            pages: PANEL_KINDS.iter().map(|(id, _)| PanelPage::new(id, true)).collect(),
        }
    }
}

impl Panel {
    /// 保留用户顺序，剔除未知/重复 id，补齐新增页（默认开启）
    pub fn normalize(&mut self) {
        let mut out: Vec<PanelPage> = Vec::new();
        for p in self.pages.iter() {
            if PANEL_KINDS.iter().any(|(k, _)| *k == p.id) && !out.iter().any(|q| q.id == p.id) {
                out.push(p.clone());
            }
        }
        for (id, _) in PANEL_KINDS.iter() {
            if !out.iter().any(|q| q.id == *id) {
                out.push(PanelPage::new(id, true));
            }
        }
        self.pages = out;
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub modules: Modules,
    /// 展开面板的轮播页顺序与开关
    pub panel: Panel,
    pub salary: Salary,
    pub health: Health,
    pub mail: Mail,
    pub stock: Stock,
    pub autostart: bool,
    /// 桌面分区：是否随系统启动自动显示
    pub zones_autostart: bool,
    /// 桌面分区列表（纯视觉分组，不移动/不删除任何文件）
    pub zones: Vec<ZoneConf>,
    /// 分区分类方案版本标记：旧版（文档/应用/其他）配置升级到完整分类后置 true，
    /// 避免每次启动都把用户已调整过的分区重置
    pub zones_migrated: bool,
    /// 分区面板不透明度（80-230），越小壁纸越清楚；右键菜单三档可调
    pub zone_alpha: u32,
    /// 剪贴板历史条数
    pub clipboard_history: usize,
    /// 文件中转站最多暂存的文件数（只存路径引用，此上限仅为列表管理，非内存约束）
    pub filehub_cap: usize,
    /// 主题色 id（theme::THEMES 之一）；胶囊固定在屏幕顶部居中，货币符号固定为 ¥
    pub theme: String,
    /// 胶囊最左侧动态图标 id（icon::ICON_KINDS 之一）
    pub icon: String,
    /// 图标是否随系统输出音量（播放音乐/视频）加大跳动幅度
    pub sound_react: bool,
    /// 面板区域被其它应用窗口压住时，自动收成顶部横条（鼠标移上去再展开）
    pub auto_collapse: bool,
    /// 日程开始前多少分钟在胶囊上提醒（0 = 不提醒）
    pub schedule_lead_minutes: u32,
    /// 外部日历订阅（ICS / webcal，钉钉、飞书等导出的链接）
    pub schedule_sub: ScheduleSub,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ScheduleSub {
    /// 自动刷新间隔（分钟）
    pub refresh_minutes: u32,
    /// 飞书日历
    pub feishu: CalSource,
    /// 钉钉日历
    pub dingtalk: CalSource,
    /// 企业微信日历
    pub wecom: CalSource,
}

/// 单个外部日历来源：订阅链接与 CalDAV 二选一，填了订阅链接优先走订阅
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CalSource {
    /// 该来源是否参与同步
    pub on: bool,
    /// ICS / webcal 订阅地址
    pub ics_url: String,
    /// CalDAV 服务器（默认已按各家官方地址预填，可改）
    pub dav_server: String,
    pub dav_user: String,
    pub dav_pass: String,
}

impl Default for CalSource {
    fn default() -> Self {
        CalSource {
            on: true,
            ics_url: String::new(),
            dav_server: String::new(),
            dav_user: String::new(),
            dav_pass: String::new(),
        }
    }
}

impl CalSource {
    fn with_server(server: &str) -> Self {
        CalSource { dav_server: server.to_string(), ..Default::default() }
    }

    /// 已启用且至少填了一种凭据
    pub fn configured(&self) -> bool {
        if !self.on {
            return false;
        }
        let ics = !self.ics_url.trim().is_empty();
        let dav = !self.dav_user.trim().is_empty() && !self.dav_pass.trim().is_empty();
        ics || dav
    }
}

/// 三个来源的默认 CalDAV 服务器。
/// 企业微信填页面上写的 wecom.work 即可（程序在 DAV 层自动映射到 caldav 子域，
/// wecom.work 本身是手机 Exchange 网关，对 CalDAV 一律回 403）。
pub const FEISHU_DAV: &str = "caldav.feishu.cn";
pub const DINGTALK_DAV: &str = "calendar.dingtalk.com";
pub const WECOM_DAV: &str = "wecom.work";

impl Default for ScheduleSub {
    fn default() -> Self {
        ScheduleSub {
            refresh_minutes: 30,
            feishu: CalSource::with_server(FEISHU_DAV),
            dingtalk: CalSource::with_server(DINGTALK_DAV),
            wecom: CalSource::with_server(WECOM_DAV),
        }
    }
}

/// 单个桌面分区（围栏）：一块毛玻璃面板，里面托管显示归入本分区的桌面图标。
/// 坐标基于桌面工作区的物理像素。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ZoneConf {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 主题色 id（theme::THEMES 之一），决定标题栏强调色
    pub color: String,
    /// 当前是否显示
    pub visible: bool,
    /// 是否折叠（只留标题栏）
    pub collapsed: bool,
    /// 归入本分区的桌面项目（文件名，如 "方案.docx"）；不含路径
    pub items: Vec<String>,
}

impl Default for ZoneConf {
    fn default() -> Self {
        ZoneConf {
            name: "新分区".into(),
            x: 120,
            y: 120,
            w: 300,
            h: 200,
            color: "blue".into(),
            visible: true,
            collapsed: false,
            items: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            modules: Modules::default(),
            panel: Panel::default(),
            salary: Salary::default(),
            health: Health::default(),
            mail: Mail::default(),
            stock: Stock::default(),
            autostart: false,
            zones_autostart: true,
            zones: Vec::new(),
            zones_migrated: false,
            zone_alpha: 150,
            clipboard_history: 20,
            filehub_cap: crate::modules::files::DEFAULT_FILEHUB_CAP,
            theme: "blue".into(),
            icon: "glow".into(),
            sound_react: true,
            auto_collapse: true,
            schedule_lead_minutes: 5,
            schedule_sub: ScheduleSub::default(),
        }
    }
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("WinIsland")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn data_path() -> PathBuf {
    data_dir().join("data.json")
}

impl Config {
    pub fn load() -> Config {
        let p = config_path();
        if let Ok(txt) = std::fs::read_to_string(&p) {
            match serde_json::from_str::<Config>(&txt) {
                Ok(mut c) => {
                    c.panel.normalize();
                    return c;
                }
                Err(e) => {
                    // 备份损坏配置，避免用户数据丢失
                    let _ = std::fs::copy(&p, p.with_extension("json.bak"));
                    eprintln!("[winisland] config.json 解析失败（已备份为 .bak），使用默认配置: {e}");
                }
            }
        }
        Config::default()
    }

    pub fn save(&self) {
        let p = config_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(txt) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&p, txt);
        }
    }
}
