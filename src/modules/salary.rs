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

    /// 已工作秒数（**扣除午休**，按秒连续，截止现在、不超过下班）。
    ///
    /// 正常班次（end≥start）：只统计 [start, now]∩[start,end]，并只扣落在
    /// 工作窗口内的午休（不强依赖设置校验，午休即使配置到窗口外也不误扣）。
    ///
    /// 跨零点班次（end<start，如 22:00–06:00）：班次横跨 [start, 24h) 与 [0, end)
    /// 两段，分别累计「到 now 为止已上过的时长」并各扣午休重叠。
    pub fn worked_secs_excl_lunch(&self, now: f64) -> f64 {
        let s = self.start_min as f64 * 60.0;
        let e = self.end_min as f64 * 60.0;
        let lo = self.lunch_start * 60.0;
        let hi = (self.lunch_end * 60.0).max(lo);
        let day = 24.0 * 3600.0;

        if e >= s {
            // 正常单段班次
            let now = now.clamp(s, e);
            let overlap = seg_overlap(lo, hi, s, now);
            (now - s - overlap).max(0.0)
        } else {
            // 跨零点：段 A [s, 24h) ∪ 段 B [0, e)
            let mut worked = 0.0;
            if now >= s {
                worked += now - s;
                worked -= seg_overlap(lo, hi, s, now.min(day));
            }
            if now < e {
                worked += now; // 段 B 从 0 点到 now
                worked -= seg_overlap(lo, hi, 0.0, now);
            }
            worked.max(0.0)
        }
    }

    /// 今日已赚（按毫秒级时间连续增长，午餐时间不计入）
    pub fn earned_today_f(&self, now: f64) -> f64 {
        self.per_sec * self.worked_secs_excl_lunch(now)
    }

    /// 今日工作进度 0..1：正常班次按 [start,end] 线性；跨零点按
    /// 「班次总时长 = (24h-start)+end」的展开轴计算。
    pub fn progress(&self, now_min: f64) -> f32 {
        let s = self.start_min as f64;
        let e = self.end_min as f64;
        let day_min = 24.0 * 60.0;
        if e >= s {
            let span = (e - s).max(1.0);
            (((now_min - s) / span) as f32).clamp(0.0, 1.0)
        } else {
            // 跨零点展开轴：now 在 [s,1440) 映射 (now-s)，在 [0,e) 映射 (now + (1440-s))
            let span = ((day_min - s) + e).max(1.0);
            let pos = if now_min >= s {
                now_min - s
            } else if now_min < e {
                now_min + (day_min - s)
            } else {
                return 0.0;
            };
            ((pos / span) as f32).clamp(0.0, 1.0)
        }
    }

    /// 距下一个休息节点（按秒）：返回 (标签, 剩余秒)
    /// - 还没到午休 → 距午休还有；午休中 → 午休剩余
    /// - 下午工作中 → 距下班还有；已下班 → 已下班
    pub fn next_break_secs(&self, now: f64) -> (&'static str, f64) {
        let ls = self.lunch_start * 60.0;
        let le = (self.lunch_end * 60.0).max(ls);
        let s = self.start_min as f64 * 60.0;
        let e = self.end_min as f64 * 60.0;
        let day = 24.0 * 3600.0;

        if e >= s {
            // 正常班次
            if now < ls {
                ("距午休还有", (ls - now).max(0.0))
            } else if now < le {
                ("午休剩余", (le - now).max(0.0))
            } else if now < e {
                ("距下班还有", (e - now).max(0.0))
            } else {
                ("已下班", (now - e).max(0.0))
            }
        } else {
            // 跨零点：无午休语义，直接算距下班；段 A 内需绕零点
            if now < e {
                ("距下班还有", (e - now).max(0.0))
            } else if now < s {
                // 两段之间（e~s）不在本班次
                ("未上班", 0.0)
            } else {
                // 段 A [s,24h) 中：距绕零点后的下班
                ("距下班还有", ((day - now) + e).max(0.0))
            }
        }
    }
}

/// 区间 [a,b] 与 [x,y] 的交集长度；三者均以「秒」为单位的一维时间。
fn seg_overlap(a: f64, b: f64, x: f64, y: f64) -> f64 {
    (b.min(y) - a.max(x)).max(0.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn calc() -> SalaryCalc {
        SalaryCalc { per_sec: 0.0, start_min: 9 * 60, end_min: 18 * 60, lunch_start: 12.0 * 60.0, lunch_end: 13.0 * 60.0 }
    }

    #[test]
    fn after_work_elapsed_seconds_grow() {
        let c = calc();
        let end = 18.0 * 3600.0;
        // 18:05:00 下班后 5 分钟 = 300 秒，且随时钟递增
        let (lbl1, s1) = c.next_break_secs(end + 300.0);
        assert_eq!(lbl1, "已下班");
        assert!((s1 - 300.0).abs() < 0.01, "下班后应为 300 秒, got {s1}");
        let (lbl2, s2) = c.next_break_secs(end + 3723.0);
        assert_eq!(lbl2, "已下班");
        assert!((s2 - 3723.0).abs() < 0.01, "1时2分3秒后应为 3723 秒, got {s2}");
        assert!(s2 > s1, "已下班时长应随时间递增");
    }

    #[test]
    fn before_end_counts_down() {
        let c = calc();
        let (lbl, s) = c.next_break_secs(17.0 * 3600.0);
        assert_eq!(lbl, "距下班还有");
        assert!((s - 3600.0).abs() < 0.01, "距下班 1 小时, got {s}");
    }

    #[test]
    fn lunch_within_work_window_only() {
        // 正常班次 9–18、午休 12–13：11:30 应计满 2.5h
        let c = calc();
        let w = c.worked_secs_excl_lunch(11.5 * 3600.0);
        assert!((w - 2.5 * 3600.0).abs() < 1.0, "11:30 已工作 2.5h, got {w}");
        // 14:00：4h 工作 + 1h 午休 = 5h 墙钟 → 计 4h
        let w = c.worked_secs_excl_lunch(14.0 * 3600.0);
        assert!((w - 4.0 * 3600.0).abs() < 1.0, "14:00 已工作 4h, got {w}");
    }

    #[test]
    fn lunch_outside_work_window_not_subtracted() {
        // 午休起点早于上班（8:00–9:00），但上班是 9:00——不应误扣：
        // 只是把算法写健壮（设置里本也校验午休落在窗口内）。
        let c = SalaryCalc { per_sec: 1.0, start_min: 9 * 60, end_min: 18 * 60, lunch_start: 8.0 * 60.0, lunch_end: 9.0 * 60.0 };
        let w = c.worked_secs_excl_lunch(10.0 * 3600.0);
        assert!((w - 1.0 * 3600.0).abs() < 1.0, "09:00-10:00 应计 1h, got {w}");
    }

    #[test]
    fn cross_midnight_shift() {
        // 夜班 22:00–06:00，无午休
        let c = SalaryCalc { per_sec: 1.0, start_min: 22 * 60, end_min: 6 * 60, lunch_start: 12.0 * 60.0, lunch_end: 13.0 * 60.0 };
        // 23:00：段 A 已上 1h
        let w = c.worked_secs_excl_lunch(23.0 * 3600.0);
        assert!((w - 1.0 * 3600.0).abs() < 1.0, "23:00 应计 1h, got {w}");
        // 05:30：段 B 已上到 05:30
        let w = c.worked_secs_excl_lunch(5.5 * 3600.0);
        assert!((w - 5.5 * 3600.0).abs() < 1.0, "05:30 应计 5.5h, got {w}");
        // 12:00：不在班次
        let w = c.worked_secs_excl_lunch(12.0 * 3600.0);
        assert!((w - 0.0).abs() < 1.0, "12:00 不应计, got {w}");
        // 进度：22:00 为 0，06:00 为 1
        assert!((c.progress(22.0 * 60.0) - 0.0).abs() < 1e-4, "22:00 进度 0");
        assert!((c.progress(6.0 * 60.0) - 1.0).abs() < 1e-4, "06:00 进度 1");
    }
}
