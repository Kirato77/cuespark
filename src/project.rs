//! Show file model: 5 banks x 10 cues, saved as JSON.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const BANKS: usize = 5;
pub const CUES_PER_BANK: usize = 10;
pub const CUE_COUNT: usize = BANKS * CUES_PER_BANK;
pub const EXTENSION: &str = "cuespark";

/// Index into `Project::cues`: `bank * CUES_PER_BANK + slot`.
pub type CueId = usize;

pub fn bank_letter(bank: usize) -> char {
    (b'A' + bank as u8) as char
}

pub fn cue_key(id: CueId) -> String {
    format!("[F{}]", id % CUES_PER_BANK + 1)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Bank {
    pub name: String,
    pub link_cues: bool,
    pub link_bank: bool,
    pub loop_bank: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Cue {
    pub name: String,
    /// Absolute in memory; stored relative to the show file when it lives below it.
    pub path: String,
    pub link: bool,
    pub notes: String,
}

impl Cue {
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Playback {
    pub auto_preselect: bool,
    /// true: preselect the next cue after a cue starts; false: keep the current one.
    pub preselect_next: bool,
    pub loop_cue: bool,
    pub auto_start: bool,
    /// Local wall-clock time, "HH:MM".
    pub start_time: String,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            auto_preselect: true,
            preselect_next: true,
            loop_cue: false,
            auto_start: false,
            start_time: "20:00".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Project {
    pub version: u32,
    pub name: String,
    pub created: String,
    pub modified: String,
    pub banks: Vec<Bank>,
    pub cues: Vec<Cue>,
    pub playback: Playback,
}

impl Default for Project {
    fn default() -> Self {
        let today = today();
        Self {
            version: 1,
            name: "Untitled".into(),
            created: today.clone(),
            modified: today,
            banks: (0..BANKS)
                .map(|_| Bank { name: "--".into(), ..Default::default() })
                .collect(),
            cues: vec![Cue::default(); CUE_COUNT],
            playback: Playback::default(),
        }
    }
}

pub fn today() -> String {
    chrono::Local::now().format("%d.%m.%Y").to_string()
}

impl Project {
    /// Fix up anything a hand-edited or older file could get wrong.
    fn normalize(&mut self) {
        self.banks.resize_with(BANKS, Bank::default);
        self.cues.resize_with(CUE_COUNT, Cue::default);
    }

    pub fn load(file: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
        let mut p: Project =
            serde_json::from_str(&text).map_err(|e| format!("{} is not a valid show file: {e}", file.display()))?;
        p.normalize();
        let dir = file.parent().unwrap_or(Path::new("."));
        for cue in &mut p.cues {
            if !cue.path.is_empty() {
                cue.path = resolve(dir, &cue.path).to_string_lossy().into_owned();
            }
        }
        Ok(p)
    }

    /// Atomic save: a crash or full disk never leaves a half-written show file.
    pub fn save(&self, file: &Path) -> Result<(), String> {
        let dir = file.parent().unwrap_or(Path::new("."));
        let mut out = self.clone();
        for cue in &mut out.cues {
            if let Ok(rel) = Path::new(&cue.path).strip_prefix(dir) {
                cue.path = rel.to_string_lossy().replace('\\', "/");
            }
        }
        let json = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
        let tmp = file.with_extension(format!("{EXTENSION}.tmp"));
        std::fs::write(&tmp, json).map_err(|e| format!("Cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, file).map_err(|e| format!("Cannot replace {}: {e}", file.display()))
    }

    /// The cue that automatically follows `id` when it ends, if any.
    /// A link needs both the bank's "Link Cues" switch and the cue's own link box.
    pub fn follower(&self, id: CueId, has_clip: impl Fn(CueId) -> bool) -> Option<CueId> {
        let bank = id / CUES_PER_BANK;
        let b = &self.banks[bank];
        if !(b.link_cues && self.cues[id].link) {
            return None;
        }
        let first = bank * CUES_PER_BANK;
        let last_used = (first..first + CUES_PER_BANK).rev().find(|&c| has_clip(c))?;
        if id < last_used {
            return Some(id + 1).filter(|&n| has_clip(n));
        }
        if b.loop_bank {
            return (first..first + CUES_PER_BANK).find(|&c| has_clip(c));
        }
        if b.link_bank && bank + 1 < BANKS {
            let next = first + CUES_PER_BANK;
            return (next..next + CUES_PER_BANK).find(|&c| has_clip(c));
        }
        None
    }

    /// Next / previous playable cue in show order, for preselection.
    pub fn step(&self, from: CueId, forward: bool, has_clip: impl Fn(CueId) -> bool) -> Option<CueId> {
        if forward {
            (from + 1..CUE_COUNT).find(|&c| has_clip(c))
        } else {
            (0..from).rev().find(|&c| has_clip(c))
        }
    }

    pub fn first_playable(&self, has_clip: impl Fn(CueId) -> bool) -> Option<CueId> {
        (0..CUE_COUNT).find(|&c| has_clip(c))
    }
}

/// Resolve a stored path. Shows move between machines (D:\ vs C:\, Windows vs Linux),
/// so when the stored path is gone, look for the file name next to the show file
/// and one folder below it.
pub fn resolve(dir: &Path, stored: &str) -> PathBuf {
    let normalized = stored.replace('\\', "/");
    let p = Path::new(&normalized);
    let direct = if p.is_absolute() || stored.contains(':') {
        PathBuf::from(stored)
    } else {
        // Rebuild from components so Windows gets native separators.
        dir.join(normalized.split('/').collect::<PathBuf>())
    };
    if direct.exists() {
        return direct;
    }
    let Some(name) = Path::new(&normalized).file_name() else { return direct };
    let candidate = dir.join(name);
    if candidate.exists() {
        return candidate;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let c = e.path().join(name);
            if c.exists() {
                return c;
            }
        }
    }
    direct
}

/// Next time the wall clock shows `start_time` ("HH:MM"): today if still ahead, else tomorrow.
/// Built from the local date, so a DST change does not shift it by an hour.
pub fn next_start<Tz: chrono::TimeZone>(now: chrono::DateTime<Tz>, start_time: &str) -> Option<chrono::DateTime<Tz>> {
    let (h, m) = parse_hhmm(start_time)?;
    let tz = now.timezone();
    let at = |d: chrono::NaiveDate| tz.from_local_datetime(&d.and_hms_opt(h, m, 0)?).earliest();
    let today = at(now.date_naive())?;
    if today > now { Some(today) } else { at(now.date_naive().succ_opt()?) }
}

/// "HH:MM" -> (h, m)
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has(p: &Project) -> impl Fn(CueId) -> bool + '_ {
        move |c| !p.cues[c].is_empty()
    }

    fn project_with(paths: &[CueId]) -> Project {
        let mut p = Project::default();
        for &c in paths {
            p.cues[c].path = format!("/x/{c}.wav");
            p.cues[c].link = true;
        }
        p
    }

    #[test]
    fn follower_rules() {
        let mut p = project_with(&[0, 1, 2, 10, 11]);
        assert_eq!(p.follower(0, has(&p)), None, "Link Cues off");
        p.banks[0].link_cues = true;
        assert_eq!(p.follower(0, has(&p)), Some(1));
        assert_eq!(p.follower(2, has(&p)), None, "end of bank, no loop/link");
        p.banks[0].loop_bank = true;
        assert_eq!(p.follower(2, has(&p)), Some(0));
        p.banks[0].loop_bank = false;
        p.banks[0].link_bank = true;
        assert_eq!(p.follower(2, has(&p)), Some(10));
        p.cues[1].link = false;
        assert_eq!(p.follower(1, has(&p)), None, "cue link box off");
        // A gap inside a bank stops the chain.
        let mut q = project_with(&[0, 2]);
        q.banks[0].link_cues = true;
        assert_eq!(q.follower(0, has(&q)), None);
    }

    #[test]
    fn save_load_roundtrip_with_relative_paths() {
        let dir = std::env::temp_dir().join(format!("cuespark-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("clips")).unwrap();
        std::fs::write(dir.join("clips/a.wav"), b"x").unwrap();
        let mut p = Project::default();
        p.cues[3].path = dir.join("clips/a.wav").to_string_lossy().into_owned();
        p.cues[3].notes = "Jets!".into();
        let file = dir.join("show.cuespark");
        p.save(&file).unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.contains("\"clips/a.wav\""), "stored relative: {raw}");
        assert_eq!(Project::load(&file).unwrap(), p);
        // Moved machine: absolute path gone, file found next to the show.
        assert_eq!(resolve(&dir, "D:\\old\\clips\\a.wav"), dir.join("clips/a.wav"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn next_start_today_or_tomorrow() {
        use chrono::{FixedOffset, TimeZone};
        let tz = FixedOffset::east_opt(3600).unwrap();
        let at = |d, h, m| tz.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap();
        assert_eq!(next_start(at(9, 19, 0), "20:00"), Some(at(9, 20, 0)));
        assert_eq!(next_start(at(9, 20, 0), "20:00"), Some(at(10, 20, 0)), "exactly now -> tomorrow");
        assert_eq!(next_start(at(9, 21, 30), "20:00"), Some(at(10, 20, 0)));
        assert_eq!(next_start(at(9, 19, 0), "nope"), None);
    }

    #[test]
    fn hhmm() {
        assert_eq!(parse_hhmm("20:05"), Some((20, 5)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("abc"), None);
    }
}
