//! 打工人薪资模拟器 — 纯本地计算：月薪/工时/工作日 -> 每秒收入

use crate::modules::clock;
use crate::config::Salary;

pub struct SalaryCalc {
    pub per_sec: f64,
    pub start_min: i64,
    pub end_min: i64,
    /// 午休起止（分钟，自 0 点）
    pub lunch_start: f64,
    pub lunch_end: f64,
}

impl SalaryCalc {
    pub fn new(cfg: &Salary) -> Self {
        let total_work_secs = cfg.workdays_per_month * cfg.hours_per_day * 3600.0;
        let per_sec = if total_work_secs > 0.0 {
            cfg.monthly / total_work_secs
        } else {
            0.0
        };
        SalaryCalc {
            per_sec,
            start_min: clock::parse_hm(&cfg.work_start).unwrap_or(9 * 60),
            end_min: clock::parse_hm(&cfg.work_end).unwrap_or(18 * 60),
            lunch_start: cfg.lunch_start_min(),
            lunch_end: cfg.lunch_end_min(),
        }
    }

    /// 已工作秒数（**扣除午休**，按秒连续，截止现在、不超过下班）
    pub fn worked_secs_excl_lunch(&self, now: f64) -> f64 {
        let s = self.start_min as f64 * 60.0;
        let e = self.end_min.max(self.start_min) as f64 * 60.0;
        let now = now.clamp(s, e);
        let lo = self.lunch_start * 60.0;
        let hi = (self.lunch_end * 60.0).max(lo);
        let overlap = (now.min(hi) - lo).max(0.0);
        (now - s - overlap).max(0.0)
    }

    /// 今日已赚（按毫秒级时间连续增长，午餐时间不计入）
    pub fn earned_today_f(&self, now: f64) -> f64 {
        self.per_sec * self.worked_secs_excl_lunch(now)
    }

    /// 今日工作进度 0..1（从上午上班到下班的时间轴）
    pub fn progress(&self, now_min: f64) -> f32 {
        let span = (self.end_min as f64 - self.start_min as f64).max(1.0);
        (((now_min - self.start_min as f64) / span) as f32).clamp(0.0, 1.0)
    }

    /// 距下一个休息节点（按秒）：返回 (标签, 剩余秒)
    /// - 还没到午休 → 距午休还有
    /// - 午休中 → 午休剩余
    /// - 下午工作中 → 距下班还有
    /// - 已下班 → 已下班
    pub fn next_break_secs(&self, now: f64) -> (&'static str, f64) {
        let ls = self.lunch_start * 60.0;
        let le = (self.lunch_end * 60.0).max(ls);
        let e = self.end_min.max(self.start_min) as f64 * 60.0;
        if now < ls {
            ("距午休还有", (ls - now).max(0.0))
        } else if now < le {
            ("午休剩余", (le - now).max(0.0))
        } else if now < e {
            ("距下班还有", (e - now).max(0.0))
        } else {
            ("已下班", 0.0)
        }
    }
}

/// "3h04m12s" / "4m12s" / "12s" —— 秒级时长
#[allow(dead_code)]
pub fn fmt_dur_secs(secs: f64) -> String {
    let t = secs.max(0.0).floor() as i64;
    let (h, m, s) = (t / 3600, (t % 3600) / 60, t % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// "3h04m12s" 的分段表示：[("3","h"),("04","m"),("12","s")]
/// 供胶囊「数值大字 + 单位小字」分色展示；省略高位为 0 的段。
pub fn dur_segs(secs: f64) -> Vec<(String, String)> {
    let t = secs.max(0.0).floor() as i64;
    let (h, m, s) = (t / 3600, (t % 3600) / 60, t % 60);
    let mut v = Vec::with_capacity(3);
    if h > 0 {
        v.push((h.to_string(), "时".to_string()));
    }
    if h > 0 || m > 0 {
        v.push((format!("{m:02}"), "分".to_string()));
    }
    v.push((format!("{s:02}"), "秒".to_string()));
    v
}

/// 金额小数位：标准两位小数（用户要求，变化慢或静止均可接受）。
pub const MONEY_DECIMALS: usize = 2;

pub fn fmt_money(v: f64) -> String {
    format!("{:.*}", MONEY_DECIMALS, v)
}
