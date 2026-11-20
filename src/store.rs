//! 本地数据（data.json）：日程 / 待办 / 剪贴板历史 / 文件中转 / 打字统计 / 喝水记录
//! 全部仅存于本机，绝不上传。

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ScheduleItem {
    pub time: String, // "09:30"
    pub title: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClipItem {
    pub text: String,
    pub ts: i64, // unix 秒
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileItem {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MailCode {
    pub code: String,
    pub from: String,
    pub ts: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Store {
    pub todos: Vec<TodoItem>,
    pub clips: Vec<ClipItem>,
    pub files: Vec<FileItem>,
    pub mail_code: Option<MailCode>,
    pub seen_mail_ids: Vec<String>,
    /// 已处理过的最大邮件 UID（IMAP）——验证码邮件主题往往都叫「验证码」，
    /// 用「发件人|主题」当 key 会把后续同主题的码全当成重复跳过，必须用 UID
    pub mail_last_uid: u32,
    // 打字统计（按本地日期归零）
    pub typing_day: String,
    pub typing_keys: u64,
    // 喝水统计
    pub water_day: String,
    pub water_cups: u32,
    // 日程提醒去重：已提醒过的 key（日期|时间|标题），跨天清空。
    // 用集合而不是单个字符串——单个只能记住一条，多条日程同时在窗口内时
    // 会 A→B→A 来回弹。
    pub sched_ack_day: String,
    pub sched_ack: Vec<String>,
}

impl Store {
    pub fn load() -> Store {
        Store::try_load().unwrap_or_default()
    }

    /// 与 `load()` 相同，但解析失败时返回 `None`，供「合并磁盘字段」场景区分
    /// 「磁盘上没有邮件状态」和「文件正在被后台写入、暂时读坏了」——
    /// 后者若当成空状态合并，会反而把 UID 水位清掉。
    pub fn try_load() -> Option<Store> {
        let p = crate::config::data_path();
        let txt = std::fs::read_to_string(&p).ok()?;
        serde_json::from_str::<Store>(&txt).ok()
    }

    pub fn save(&self) {
        let p = crate::config::data_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(txt) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&p, txt);
        }
    }

    /// 主界面保存 data.json 时用：保留邮件后台线程独立维护的字段。
    ///
    /// 主界面的 `Store` 是进程启动时的快照，而邮件监听线程会实时把 UID 水位、
    /// 最新验证码写进同一个文件。若主界面直接 `save()`，就会把水位写回启动时的旧值，
    /// 下一轮拉取会把同一封旧邮件重新判为新邮件 —— 表现就是「验证码一直是旧的」
    /// 且「复制后反复提醒」。这里在写盘前把磁盘上的邮件字段合并进当前快照。
    pub fn save_preserving_mail(&self) {
        let p = crate::config::data_path();
        let mut merged = self.clone();
        if let Some(disk) = Store::try_load() {
            merged.mail_code = disk.mail_code;
            merged.seen_mail_ids = disk.seen_mail_ids;
            merged.mail_last_uid = disk.mail_last_uid;
        }
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(txt) = serde_json::to_string_pretty(&merged) {
            let _ = std::fs::write(&p, txt);
        }
    }

    pub fn reset_day_if_needed(&mut self, today: &str) {
        if self.typing_day != today {
            self.typing_day = today.to_string();
            self.typing_keys = 0;
        }
        if self.water_day != today {
            self.water_day = today.to_string();
            self.water_cups = 0;
        }
        if self.sched_ack_day != today {
            self.sched_ack_day = today.to_string();
            self.sched_ack.clear();
        }
    }

    /// 这条日程今天是否已经提醒过
    pub fn sched_seen(&self, key: &str) -> bool {
        self.sched_ack.iter().any(|k| k == key)
    }

    /// 记住这条日程已经提醒过（同一天不再弹第二次）
    pub fn sched_mark(&mut self, key: &str) {
        if self.sched_seen(key) {
            return;
        }
        self.sched_ack.push(key.to_string());
        // 极端情况（一天几百条日程）也不会无限涨
        if self.sched_ack.len() > 200 {
            let cut = self.sched_ack.len() - 200;
            self.sched_ack.drain(0..cut);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Store;

    #[test]
    fn sched_ack_remembers_all_items_not_just_last() {
        // 回归：多个日程同时落在提醒窗口内时，原来只记一个 key，
        // 会 A→B→A 反复弹；现在每条都记，点掉后都不再弹
        let mut s = Store::default();
        let a = "2026-11-15|22:00|钉钉测试".to_string();
        let b = "2026-11-15|22:10|产品评审".to_string();
        s.sched_mark(&a);
        s.sched_mark(&b);
        assert!(s.sched_seen(&a));
        assert!(s.sched_seen(&b));
        assert!(!s.sched_seen("2026-11-15|23:00|别的会"));
    }

    #[test]
    fn sched_ack_mark_is_idempotent_and_capped() {
        let mut s = Store::default();
        s.sched_mark("k");
        s.sched_mark("k");
        assert_eq!(s.sched_ack.len(), 1);
        for i in 0..260 {
            s.sched_mark(&format!("k{i}"));
        }
        assert_eq!(s.sched_ack.len(), 200);
    }

    #[test]
    fn sched_ack_clears_on_new_day() {
        // 跨天后要重新允许提醒（第二天同一时间的日程照常弹）
        let mut s = Store::default();
        s.reset_day_if_needed("2026-11-15");
        s.sched_mark("2026-11-15|22:00|钉钉测试");
        assert!(s.sched_seen("2026-11-15|22:00|钉钉测试"));
        s.reset_day_if_needed("2026-11-16");
        assert!(!s.sched_seen("2026-11-15|22:00|钉钉测试"));
        assert_eq!(s.sched_ack_day, "2026-11-16");
    }
}
