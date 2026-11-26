//! Daily timetable scheduler: data model, persistence, validation, and due-block
//! background timer. Pure functions are decoupled from OS time for testability.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub templates: Vec<Template>,
    #[serde(default)]
    pub week: WeekPlan,
    /// Daily checklist state: local date ("YYYY-MM-DD") -> ids of blocks marked done that day.
    #[serde(default)]
    pub done: BTreeMap<String, Vec<String>>,
}

fn default_version() -> u32 {
    1
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            version: 1,
            templates: Vec::new(),
            week: WeekPlan::default(),
            done: BTreeMap::new(),
        }
    }
}

/// How many days of checklist history are kept.
const MAX_DONE_DAYS: usize = 90;

fn is_valid_date_key(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits_ok = b
        .iter()
        .enumerate()
        .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !digits_ok {
        return false;
    }
    let month: u16 = s[5..7].parse().unwrap_or(0);
    let day: u16 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Marks one block done/undone for a given date. Returns false if the date or id is invalid.
pub fn set_block_done(schedule: &mut Schedule, date: &str, block_id: &str, done: bool) -> bool {
    if !is_valid_date_key(date) || block_id.is_empty() || block_id.len() > 64 {
        return false;
    }
    if done {
        let ids = schedule.done.entry(date.to_string()).or_default();
        if !ids.iter().any(|id| id == block_id) {
            ids.push(block_id.to_string());
        }
    } else if let Some(ids) = schedule.done.get_mut(date) {
        ids.retain(|id| id != block_id);
        if ids.is_empty() {
            schedule.done.remove(date);
        }
    }
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    pub time: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WeekPlan {
    pub mon: Option<String>,
    pub tue: Option<String>,
    pub wed: Option<String>,
    pub thu: Option<String>,
    pub fri: Option<String>,
    pub sat: Option<String>,
    pub sun: Option<String>,
}

impl WeekPlan {
    pub fn template_for_day(&self, day_of_week: u16) -> Option<&str> {
        match day_of_week {
            0 => self.sun.as_deref(),
            1 => self.mon.as_deref(),
            2 => self.tue.as_deref(),
            3 => self.wed.as_deref(),
            4 => self.thu.as_deref(),
            5 => self.fri.as_deref(),
            6 => self.sat.as_deref(),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn set_template_for_day(&mut self, day_of_week: u16, template_id: Option<String>) {
        match day_of_week {
            0 => self.sun = template_id,
            1 => self.mon = template_id,
            2 => self.tue = template_id,
            3 => self.wed = template_id,
            4 => self.thu = template_id,
            5 => self.fri = template_id,
            6 => self.sat = template_id,
            _ => {}
        }
    }
}

pub fn parse_hhmm(s: &str) -> Option<(u16, u16)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 || parts[0].len() != 2 || parts[1].len() != 2 {
        return None;
    }
    let h: u16 = parts[0].parse().ok()?;
    let m: u16 = parts[1].parse().ok()?;
    if h < 24 && m < 60 {
        Some((h, m))
    } else {
        None
    }
}

pub fn is_valid_hhmm(s: &str) -> bool {
    parse_hhmm(s).is_some()
}

pub fn validate(schedule: &mut Schedule) {
    schedule.version = 1;
    if schedule.templates.len() > 20 {
        schedule.templates.truncate(20);
    }

    let mut valid_template_ids = std::collections::HashSet::new();

    for template in &mut schedule.templates {
        template.name = template.name.trim().chars().take(120).collect();
        if template.name.is_empty() {
            template.name = "Untitled".to_string();
        }

        let mut valid_blocks = Vec::new();
        for block in &mut template.blocks {
            block.text = block.text.trim().chars().take(120).collect();
            if block.text.is_empty() {
                continue;
            }
            if !is_valid_hhmm(&block.time) {
                continue;
            }
            valid_blocks.push(block.clone());
            if valid_blocks.len() >= 100 {
                break;
            }
        }
        valid_blocks.sort_by(|a, b| a.time.cmp(&b.time));
        template.blocks = valid_blocks;

        valid_template_ids.insert(template.id.clone());
    }

    let check_ref = |day_slot: &mut Option<String>| {
        if let Some(id) = day_slot {
            if !valid_template_ids.contains(id) {
                *day_slot = None;
            }
        }
    };
    check_ref(&mut schedule.week.mon);
    check_ref(&mut schedule.week.tue);
    check_ref(&mut schedule.week.wed);
    check_ref(&mut schedule.week.thu);
    check_ref(&mut schedule.week.fri);
    check_ref(&mut schedule.week.sat);
    check_ref(&mut schedule.week.sun);

    // Checklist state: drop malformed dates/ids, dedupe, and keep only recent history.
    schedule.done.retain(|date, _| is_valid_date_key(date));
    for ids in schedule.done.values_mut() {
        ids.retain(|id| !id.is_empty() && id.len() <= 64);
        ids.sort();
        ids.dedup();
        ids.truncate(100);
    }
    schedule.done.retain(|_, ids| !ids.is_empty());
    while schedule.done.len() > MAX_DONE_DAYS {
        let Some(oldest) = schedule.done.keys().next().cloned() else { break };
        schedule.done.remove(&oldest);
    }
}

pub fn schedule_path() -> Option<PathBuf> {
    let current = crate::ai::config_path()?;
    let path = current.parent()?.join("schedule.json");
    if !path.exists() {
        if let Some(legacy) = crate::ai::legacy_config_path().and_then(|p| p.parent().map(|d| d.join("schedule.json"))) {
            if legacy.is_file() {
                if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
                let _ = std::fs::copy(legacy, &path);
            }
        }
    }
    Some(path)
}
pub fn load_schedule() -> Schedule {
    let Some(path) = schedule_path() else {
        return Schedule::default();
    };
    load_schedule_from(&path)
}

pub fn load_schedule_from(path: &std::path::Path) -> Schedule {
    if !path.exists() {
        return Schedule::default();
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Schedule>(&text) {
            Ok(mut sched) => {
                validate(&mut sched);
                sched
            }
            Err(e) => {
                eprintln!(
                    "schedule.json is invalid ({e}); renaming to schedule.json.bad and using defaults"
                );
                let bad_path = path.with_extension("json.bad");
                let _ = std::fs::rename(path, bad_path);
                Schedule::default()
            }
        },
        Err(e) => {
            eprintln!("Failed to read schedule.json ({e}); using defaults");
            Schedule::default()
        }
    }
}

pub fn save_schedule(schedule: &mut Schedule) -> Result<(), String> {
    let path = schedule_path().ok_or_else(|| "Could not determine schedule path".to_string())?;
    save_schedule_to(&path, schedule)
}

pub fn save_schedule_to(path: &std::path::Path, schedule: &mut Schedule) -> Result<(), String> {
    validate(schedule);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json_text = serde_json::to_string_pretty(schedule)
        .map_err(|e| format!("Serialization error: {e}"))?;
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json_text)
        .map_err(|e| format!("Failed to write tmp file: {e}"))?;
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("Failed to atomically rename schedule file: {e}"))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalDateTime {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub day_of_week: u16, // 0 = Sunday, 1 = Monday, ..., 6 = Saturday
}

impl LocalDateTime {
    #[allow(dead_code)]
    pub fn new(
        year: u16,
        month: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        day_of_week: u16,
    ) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            day_of_week,
        }
    }

    pub fn to_epoch_secs(&self) -> i64 {
        let days = days_from_civil(self.year as i32, self.month as u32, self.day as u32);
        days * 86_400 + (self.hour as i64) * 3600 + (self.minute as i64) * 60 + (self.second as i64)
    }
}

/// Howard Hinnant algorithm for days since 1970-01-01
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u32;
    let m = month + if month > 2 { 0 } else { 12 };
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era as i64) * 146097 + (doe as i64) - 719468
}

/// Convert days since 1970-01-01 back to (year, month, day)
#[allow(dead_code)]
pub fn civil_from_days(days: i64) -> (u16, u16, u16) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as u16, m as u16, d as u16)
}

pub fn day_of_week_from_days(days: i64) -> u16 {
    // 1970-01-01 was Thursday (4). Sunday = 0.
    (((days + 4) % 7 + 7) % 7) as u16
}

#[cfg(windows)]
pub fn now_local() -> LocalDateTime {
    let st = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    LocalDateTime {
        year: st.wYear,
        month: st.wMonth,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        day_of_week: st.wDayOfWeek,
    }
}

#[cfg(not(windows))]
pub fn now_local() -> LocalDateTime {
    // Fallback for non-windows compilation
    LocalDateTime::new(2026, 1, 1, 0, 0, 0, 4)
}

/// Finds blocks of today's (or immediately past across rollover) schedule with
/// `last_check < block_time <= now` and lateness <= 5 minutes (300 seconds).
pub fn find_due_blocks(
    schedule: &Schedule,
    last_check: LocalDateTime,
    now: LocalDateTime,
) -> Vec<String> {
    let last_secs = last_check.to_epoch_secs();
    let now_secs = now.to_epoch_secs();
    if now_secs <= last_secs {
        return Vec::new();
    }

    let min_secs = last_secs.max(now_secs - 300);

    let last_day = days_from_civil(last_check.year as i32, last_check.month as u32, last_check.day as u32);
    let now_day = days_from_civil(now.year as i32, now.month as u32, now.day as u32);

    let min_day = (min_secs / 86_400).max(last_day);

    let mut due = Vec::new();

    for day_idx in min_day..=now_day {
        let day_of_week = day_of_week_from_days(day_idx);
        let Some(template_id) = schedule.week.template_for_day(day_of_week) else {
            continue;
        };
        let Some(template) = schedule.templates.iter().find(|t| t.id == template_id) else {
            continue;
        };

        for block in &template.blocks {
            let Some((h, m)) = parse_hhmm(&block.time) else {
                continue;
            };
            let block_secs = day_idx * 86_400 + (h as i64) * 3600 + (m as i64) * 60;
            if block_secs > min_secs && block_secs <= now_secs {
                due.push(format!("{} - {}", block.time, block.text));
            }
        }
    }

    due
}

/// Computes duration until the next block today or within the next 7 days.
pub fn time_to_next_block(schedule: &Schedule, now: LocalDateTime) -> Option<Duration> {
    let now_secs = now.to_epoch_secs();
    let current_day = days_from_civil(now.year as i32, now.month as u32, now.day as u32);

    for offset in 0..=7 {
        let day_idx = current_day + offset;
        let day_of_week = day_of_week_from_days(day_idx);
        let Some(template_id) = schedule.week.template_for_day(day_of_week) else {
            continue;
        };
        let Some(template) = schedule.templates.iter().find(|t| t.id == template_id) else {
            continue;
        };

        for block in &template.blocks {
            let Some((h, m)) = parse_hhmm(&block.time) else {
                continue;
            };
            let block_secs = day_idx * 86_400 + (h as i64) * 3600 + (m as i64) * 60;
            if block_secs > now_secs {
                let diff = (block_secs - now_secs) as u64;
                return Some(Duration::from_secs(diff));
            }
        }
    }

    None
}

pub type SharedSchedule = Arc<(Mutex<Schedule>, Condvar)>;

pub fn new_shared_schedule(schedule: Schedule) -> SharedSchedule {
    Arc::new((Mutex::new(schedule), Condvar::new()))
}

pub fn start_scheduler_thread<F>(
    shared: SharedSchedule,
    on_reminder: F,
) -> std::thread::JoinHandle<()>
where
    F: Fn(Vec<String>) + Send + Sync + 'static,
{
    std::thread::spawn(move || {
        let mut last_check = now_local();
        let (lock, cvar) = &*shared;
        loop {
            let (due_lines, wait_dur) = {
                let schedule = lock.lock().unwrap();
                let now = now_local();
                let due = find_due_blocks(&schedule, last_check, now);
                last_check = now;
                let next = time_to_next_block(&schedule, now);
                let dur = next
                    .map(|d| d.min(Duration::from_secs(30)))
                    .unwrap_or(Duration::from_secs(30));
                (due, dur)
            };

            if !due_lines.is_empty() {
                on_reminder(due_lines);
            }

            let guard = lock.lock().unwrap();
            let _ = cvar.wait_timeout(guard, wait_dur);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_schedule() -> Schedule {
        Schedule {
            version: 1,
            templates: vec![
                Template {
                    id: "t1".into(),
                    name: "Weekday".into(),
                    blocks: vec![
                        Block {
                            id: "b1".into(),
                            time: "06:00".into(),
                            text: "Padhai".into(),
                        },
                        Block {
                            id: "b2".into(),
                            time: "07:30".into(),
                            text: "Exercise".into(),
                        },
                    ],
                },
                Template {
                    id: "t2".into(),
                    name: "Night Owl".into(),
                    blocks: vec![
                        Block {
                            id: "b3".into(),
                            time: "00:10".into(),
                            text: "Midnight Journal".into(),
                        },
                    ],
                },
            ],
            week: WeekPlan {
                mon: Some("t1".into()),
                tue: Some("t1".into()),
                wed: Some("t1".into()),
                thu: Some("t1".into()),
                fri: Some("t1".into()),
                sat: Some("t1".into()),
                sun: Some("t2".into()),
            },
            done: BTreeMap::new(),
        }
    }

    #[test]
    fn test_day_of_week_calculation() {
        // 2026-11-15 is Sunday (0)
        let days = days_from_civil(2026, 11, 15);
        assert_eq!(day_of_week_from_days(days), 0);
        // 2026-11-16 is Monday (1)
        assert_eq!(day_of_week_from_days(days + 1), 1);
        // 2026-11-14 is Saturday (6)
        assert_eq!(day_of_week_from_days(days - 1), 6);
    }

    #[test]
    fn test_normal_fire() {
        let sched = sample_schedule();
        // Monday (day_of_week = 1)
        let last_check = LocalDateTime::new(2026, 11, 16, 5, 59, 50, 1);
        let now = LocalDateTime::new(2026, 11, 16, 6, 0, 5, 1);
        let due = find_due_blocks(&sched, last_check, now);
        assert_eq!(due, vec!["06:00 - Padhai"]);
    }

    #[test]
    fn test_once_per_day() {
        let sched = sample_schedule();
        let last_check = LocalDateTime::new(2026, 11, 16, 6, 0, 5, 1);
        let now = LocalDateTime::new(2026, 11, 16, 6, 0, 20, 1);
        let due = find_due_blocks(&sched, last_check, now);
        assert!(due.is_empty(), "Block must not fire twice");
    }

    #[test]
    fn test_five_minute_late_rule() {
        let sched = sample_schedule();
        // Block is at 06:00. Laptop wakes up at 06:04:30 (4m30s late <= 5 min)
        let last_check = LocalDateTime::new(2026, 11, 16, 5, 30, 0, 1);
        let now = LocalDateTime::new(2026, 11, 16, 6, 4, 30, 1);
        let due = find_due_blocks(&sched, last_check, now);
        assert_eq!(due, vec!["06:00 - Padhai"]);
    }

    #[test]
    fn test_skipped_old_blocks() {
        let sched = sample_schedule();
        // Block is at 06:00. Laptop wakes up at 07:05:00 (65m late > 5 min)
        let last_check = LocalDateTime::new(2026, 11, 16, 5, 30, 0, 1);
        let now = LocalDateTime::new(2026, 11, 16, 7, 5, 0, 1);
        let due = find_due_blocks(&sched, last_check, now);
        assert!(due.is_empty(), "Blocks older than 5 minutes must be skipped");
    }

    #[test]
    fn test_midnight_rollover() {
        let sched = sample_schedule();
        // Day rollover to Sunday: t2 assigned to Sunday has block at 00:10
        // Check across rollover from Saturday 23:59:50 to Sunday 00:10:05
        let sat_days = days_from_civil(2026, 11, 14);
        assert_eq!(day_of_week_from_days(sat_days), 6);
        let sun_days = days_from_civil(2026, 11, 15);
        assert_eq!(day_of_week_from_days(sun_days), 0);

        let last_check = LocalDateTime::new(2026, 11, 15, 0, 9, 50, 0);
        let now = LocalDateTime::new(2026, 11, 15, 0, 10, 5, 0);
        let due = find_due_blocks(&sched, last_check, now);
        assert_eq!(due, vec!["00:10 - Midnight Journal"]);
    }

    #[test]
    fn test_week_plan_null_day() {
        let mut sched = sample_schedule();
        // Sunday has no template (None)
        sched.week.sun = None;
        let last_check = LocalDateTime::new(2026, 11, 15, 0, 9, 50, 0);
        let now = LocalDateTime::new(2026, 11, 15, 0, 10, 5, 0);
        let due = find_due_blocks(&sched, last_check, now);
        assert!(due.is_empty());
    }

    #[test]
    fn test_schedule_edited_while_running() {
        let mut sched = sample_schedule();
        let last_check = LocalDateTime::new(2026, 11, 16, 7, 59, 50, 1);
        let now = LocalDateTime::new(2026, 11, 16, 8, 0, 10, 1);

        // Before edit: no blocks around 08:00
        assert!(find_due_blocks(&sched, last_check, now).is_empty());

        // Edit schedule while app is running: add block at 08:00
        sched.templates[0].blocks.push(Block {
            id: "b_new".into(),
            time: "08:00".into(),
            text: "Standup".into(),
        });
        validate(&mut sched);

        // Immediate next check with updated schedule fires the new block!
        let due = find_due_blocks(&sched, last_check, now);
        assert_eq!(due, vec!["08:00 - Standup"]);
    }

    #[test]
    fn test_time_to_next_block() {
        let sched = sample_schedule();
        // Monday 05:45:00. Next is 06:00:00 (15 min = 900s)
        let now = LocalDateTime::new(2026, 11, 16, 5, 45, 0, 1);
        let next = time_to_next_block(&sched, now);
        assert_eq!(next, Some(Duration::from_secs(900)));
    }

    #[test]
    fn test_validation_and_clean_up() {
        let mut sched = Schedule {
            version: 99,
            templates: vec![Template {
                id: "t_valid".into(),
                name: "  Work Day  ".into(),
                blocks: vec![
                    Block {
                        id: "1".into(),
                        time: "09:00".into(),
                        text: "  Start  ".into(),
                    },
                    Block {
                        id: "2".into(),
                        time: "08:00".into(),
                        text: "Earlier".into(),
                    },
                    Block {
                        id: "3".into(),
                        time: "bad-time".into(),
                        text: "Drop me".into(),
                    },
                    Block {
                        id: "4".into(),
                        time: "10:00".into(),
                        text: "   ".into(), // empty text
                    },
                ],
            }],
            week: WeekPlan {
                mon: Some("t_valid".into()),
                tue: Some("non_existent".into()),
                ..Default::default()
            },
            done: BTreeMap::new(),
        };

        validate(&mut sched);
        assert_eq!(sched.version, 1);
        assert_eq!(sched.templates[0].name, "Work Day");
        assert_eq!(sched.templates[0].blocks.len(), 2);
        // sorted by time
        assert_eq!(sched.templates[0].blocks[0].time, "08:00");
        assert_eq!(sched.templates[0].blocks[1].time, "09:00");
        // invalid week references are pruned
        assert_eq!(sched.week.mon, Some("t_valid".into()));
        assert_eq!(sched.week.tue, None);
    }

    #[test]
    fn test_checklist_toggle_and_validation() {
        let mut sched = sample_schedule();
        assert!(set_block_done(&mut sched, "2026-11-27", "b1", true));
        assert!(set_block_done(&mut sched, "2026-11-27", "b1", true)); // idempotent
        assert!(set_block_done(&mut sched, "2026-11-27", "b2", true));
        assert_eq!(sched.done["2026-11-27"], vec!["b1", "b2"]);

        assert!(set_block_done(&mut sched, "2026-11-27", "b1", false));
        assert_eq!(sched.done["2026-11-27"], vec!["b2"]);
        assert!(set_block_done(&mut sched, "2026-11-27", "b2", false));
        assert!(!sched.done.contains_key("2026-11-27"), "empty days are removed");

        assert!(!set_block_done(&mut sched, "27-11-2026", "b1", true));
        assert!(!set_block_done(&mut sched, "2026-13-01", "b1", true));
        assert!(!set_block_done(&mut sched, "2026-11-27", "", true));
    }

    #[test]
    fn test_checklist_old_history_is_pruned_and_garbage_dropped() {
        let mut sched = sample_schedule();
        for i in 0..120u32 {
            let month = 1 + (i / 28) as u16;
            let day = 1 + (i % 28) as u16;
            sched.done.insert(format!("2026-{month:02}-{day:02}"), vec!["b1".into()]);
        }
        sched.done.insert("garbage".into(), vec!["b1".into()]);
        sched.done.insert("2026-02-02".into(), vec!["x".into(), "x".into()]);
        validate(&mut sched);
        assert!(sched.done.len() <= MAX_DONE_DAYS);
        assert!(!sched.done.contains_key("garbage"));
        // oldest dates are the ones dropped
        assert!(!sched.done.contains_key("2026-01-01"));
        assert!(sched.done.contains_key("2026-05-08"), "newest date is kept");
    }

    #[test]
    fn test_old_schedule_json_without_done_still_loads() {
        let text = r#"{"version":1,"templates":[{"id":"t1","name":"A","blocks":[]}],"week":{"mon":"t1"}}"#;
        let sched: Schedule = serde_json::from_str(text).unwrap();
        assert!(sched.done.is_empty());
    }

    #[test]
    fn test_file_io_missing_bad_and_atomic_save() {
        let temp_dir = std::env::temp_dir().join("ai_companion_test_sched");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_path = temp_dir.join("schedule.json");

        // 1. Missing file -> returns default
        let loaded = load_schedule_from(&test_path);
        assert_eq!(loaded, Schedule::default());

        // 2. Atomic save and load
        let mut sample = sample_schedule();
        assert!(save_schedule_to(&test_path, &mut sample).is_ok());
        let reloaded = load_schedule_from(&test_path);
        assert_eq!(reloaded, sample);

        // 3. Corrupt file -> does not crash, logs, renames to .bad, returns default
        std::fs::write(&test_path, "not a valid { json ").unwrap();
        let fallback = load_schedule_from(&test_path);
        assert_eq!(fallback, Schedule::default());
        assert!(!test_path.exists());
        let bad_path = test_path.with_extension("json.bad");
        assert!(bad_path.exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
