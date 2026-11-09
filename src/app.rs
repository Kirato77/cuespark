//! The two-mode user interface: Show Mode (fire cues) and Edit Mode (configuration matrix).

use crate::decode::{Clip, decode_file};
use crate::engine::{Cmd, Engine, Plan, Snapshot, output_device_names};
use crate::update::{self, VERSION};
use crate::project::{
    BANKS, CUE_COUNT, CUES_PER_BANK, Cue, CueId, EXTENSION, Project, bank_letter, cue_key, next_start, parse_hhmm, today,
};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Event, FontId, Key, Layout, Modifiers, Pos2, Rect, Sense, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "ogg", "oga", "m4a", "aac", "mp4", "aif", "aiff", "caf", "mka", "opus"];

// Show Mode palette (dark, high contrast for a dark booth).
const BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const PANEL: Color32 = Color32::from_rgb(0x1b, 0x1b, 0x1b);
const BLACK: Color32 = Color32::from_rgb(0x08, 0x08, 0x08);
const BTN_TOP: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
const BTN_BOTTOM: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f);
const RED: Color32 = Color32::from_rgb(0xd8, 0x1e, 0x1e);
const RED_DARK: Color32 = Color32::from_rgb(0xb0, 0x10, 0x10);
const GREY: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x8a);
const DIM: Color32 = Color32::from_rgb(0x55, 0x55, 0x55);
const WHITE: Color32 = Color32::from_rgb(0xf2, 0xf2, 0xf2);
const ROW_SELECTED: Color32 = Color32::from_rgb(0xf6, 0xee, 0xc4);

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub device: Option<String>,
    /// Fade used by Stop / Esc. 0 = immediate (5 ms anti-click).
    pub stop_fade_ms: u32,
    pub recent: Vec<PathBuf>,
    pub reopen_last: bool,
    pub keep_awake: bool,
    pub check_updates: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { device: None, stop_fade_ms: 0, recent: Vec::new(), reopen_last: true, keep_awake: true, check_updates: true }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Show,
    Edit,
}

enum SlotState {
    Empty,
    Loading,
    Ready(Arc<Clip>),
    Failed(String),
}

struct Slot {
    path: String,
    rate: u32,
    generation: u64,
    state: SlotState,
    size: Option<u64>,
}

impl Slot {
    fn clip(&self) -> Option<&Arc<Clip>> {
        match &self.state {
            SlotState::Ready(c) => Some(c),
            _ => None,
        }
    }
}

struct Job {
    generation: u64,
    path: PathBuf,
    rate: u32,
}

struct Done {
    generation: u64,
    result: Result<Clip, String>,
}

/// Decodes clips on a few background threads so loading a show never freezes the UI.
struct Loader {
    jobs: mpsc::Sender<Job>,
    done: mpsc::Receiver<Done>,
}

impl Loader {
    fn new(ctx: egui::Context) -> Self {
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done) = mpsc::channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 4);
        for i in 0..workers {
            let (rx, tx, ctx) = (job_rx.clone(), done_tx.clone(), ctx.clone());
            std::thread::Builder::new()
                .name(format!("decoder-{i}"))
                .spawn(move || {
                    loop {
                        let job = match rx.lock() {
                            Ok(guard) => guard.recv(),
                            Err(_) => return,
                        };
                        let Ok(job) = job else { return };
                        let result = std::panic::catch_unwind(|| decode_file(&job.path, job.rate))
                            .unwrap_or_else(|_| Err("decoder crashed on this file".into()));
                        if tx.send(Done { generation: job.generation, result }).is_err() {
                            return;
                        }
                        ctx.request_repaint();
                    }
                })
                .expect("spawn decoder thread");
        }
        Self { jobs, done }
    }
}

#[derive(Clone)]
enum Pending {
    New,
    Open,
    OpenPath(PathBuf),
    Quit,
}

enum Dialog {
    None,
    Discard(Pending),
    QuitShow,
    Message(String, String),
    Shortcuts,
    About,
    Licenses,
}

pub struct App {
    project: Project,
    file: Option<PathBuf>,
    dirty: bool,
    mode: Mode,
    prefs: Prefs,
    engine: Option<Engine>,
    audio_error: Option<String>,
    last_reconnect: Instant,
    slots: Vec<Slot>,
    loader: Loader,
    next_generation: u64,
    plan_dirty: bool,
    preselect: Option<CueId>,
    view_bank: usize,
    show_remaining: bool,
    selected: CueId,
    seen_starts: u64,
    seen_autostarts: u64,
    armed: Option<CueId>,
    dialog: Dialog,
    allow_close: bool,
    title: String,
    keep_awake_on: bool,
    /// Cached: enumerating devices costs 8-23 ms, too slow to redo every frame.
    devices: Vec<String>,
    /// Set when New/Open swap the whole show this frame, so it isn't seen as an edit.
    replaced: bool,
    update: update::Shared,
    logo: egui::TextureHandle,
    /// Show "up to date" / errors only when the user asked; a silent startup check stays silent.
    update_manual: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, open_arg: Option<PathBuf>) -> Self {
        let prefs: Prefs = cc.storage.and_then(|s| eframe::get_value(s, "prefs")).unwrap_or_default();
        let (engine, audio_error) = match Engine::open(prefs.device.as_deref()) {
            Ok(e) => (Some(e), None),
            Err(e) => (None, Some(e)),
        };
        let mut app = Self {
            project: Project::default(),
            file: None,
            dirty: false,
            mode: Mode::Edit,
            prefs,
            engine,
            audio_error,
            last_reconnect: Instant::now(),
            slots: (0..CUE_COUNT)
                .map(|_| Slot { path: String::new(), rate: 0, generation: 0, state: SlotState::Empty, size: None })
                .collect(),
            loader: Loader::new(cc.egui_ctx.clone()),
            next_generation: 1,
            plan_dirty: true,
            preselect: None,
            view_bank: 0,
            show_remaining: false,
            selected: 0,
            seen_starts: 0,
            seen_autostarts: 0,
            armed: None,
            dialog: Dialog::None,
            allow_close: false,
            title: String::new(),
            keep_awake_on: false,
            devices: output_device_names(),
            replaced: false,
            update: Arc::new(Mutex::new(update::State::Idle)),
            logo: cc.egui_ctx.load_texture(
                "logo",
                egui::ColorImage::from_rgba_unmultiplied([128, 128], include_bytes!("../assets/icon-128.rgba")),
                egui::TextureOptions::LINEAR,
            ),
            update_manual: false,
        };
        if app.prefs.check_updates {
            update::check(&app.update, &cc.egui_ctx);
        }
        let reopen = app.prefs.recent.first().filter(|p| app.prefs.reopen_last && p.exists()).cloned();
        if let Some(path) = open_arg.or(reopen) {
            app.open_path(&path);
        }
        app
    }

    // ---------------------------------------------------------------- state

    fn has_clip(&self, c: CueId) -> bool {
        self.slots[c].clip().is_some()
    }

    /// Frames of all loaded clips in a bank.
    fn bank_total(&self, bank: usize) -> u64 {
        let cues = bank * CUES_PER_BANK..(bank + 1) * CUES_PER_BANK;
        cues.filter_map(|c| self.slots[c].clip().map(|k| k.frames() as u64)).sum()
    }

    fn rate(&self) -> u32 {
        self.engine.as_ref().map(|e| e.rate).unwrap_or(0)
    }

    fn send(&mut self, cmd: Cmd) {
        if let Some(e) = &mut self.engine {
            e.send(cmd);
        }
    }

    fn status(&self) -> Snapshot {
        match &self.engine {
            Some(e) => e.status.snapshot(),
            None => Snapshot { cue: None, pos: 0, len: 0, paused: false },
        }
    }

    /// Start decoding every cue whose file or output rate changed.
    fn sync_slots(&mut self) {
        let rate = self.rate();
        for c in 0..CUE_COUNT {
            let path = self.project.cues[c].path.clone();
            let slot = &self.slots[c];
            if slot.path == path && (slot.rate == rate || path.is_empty()) {
                continue;
            }
            let generation = self.next_generation;
            self.next_generation += 1;
            let size = std::fs::metadata(&path).ok().map(|m| m.len());
            let state = if path.is_empty() {
                SlotState::Empty
            } else if rate == 0 {
                SlotState::Failed("no audio device".into())
            } else if size.is_none() {
                SlotState::Failed("file not found".into())
            } else {
                let _ = self.loader.jobs.send(Job { generation, path: PathBuf::from(&path), rate });
                SlotState::Loading
            };
            self.slots[c] = Slot { path, rate, generation, state, size };
            self.plan_dirty = true;
        }
    }

    fn rebuild_plan(&mut self) {
        let has = |c: CueId| self.has_clip(c);
        let plan = Plan {
            clips: self.slots.iter().map(|s| s.clip().cloned()).collect(),
            next: (0..CUE_COUNT).map(|c| self.project.follower(c, has)).collect(),
            loop_cue: self.project.playback.loop_cue,
        };
        self.send(Cmd::SetPlan(Arc::new(plan)));
        self.plan_dirty = false;
        // A cue that is no longer playable cannot stay preselected.
        if self.preselect.is_none_or(|p| !self.has_clip(p)) {
            // Wait for loading to finish, or the first clip decoded (not the first cue) wins.
            if !self.slots.iter().any(|s| matches!(s.state, SlotState::Loading)) {
                self.preselect = self.project.first_playable(|c| self.has_clip(c));
            }
        }
        self.rearm();
    }

    fn fire(&mut self, cue: CueId) {
        if self.has_clip(cue) {
            self.send(Cmd::Fire(cue));
        }
    }

    fn go(&mut self) {
        if let Some(p) = self.preselect {
            self.fire(p);
        }
    }

    fn stop(&mut self) {
        let fade_frames = (self.prefs.stop_fade_ms as u64 * self.rate() as u64 / 1000) as u32;
        self.send(Cmd::Stop { fade_frames });
    }

    fn step_preselect(&mut self, forward: bool) {
        let from = self.preselect.unwrap_or(if forward { usize::MAX } else { CUE_COUNT });
        let next = if from == usize::MAX {
            self.project.first_playable(|c| self.has_clip(c))
        } else {
            self.project.step(from, forward, |c| self.has_clip(c))
        };
        if let Some(n) = next {
            self.preselect = Some(n);
            self.view_bank = n / CUES_PER_BANK;
            self.rearm();
        }
    }

    // ------------------------------------------------------- automatic start

    fn start_at(&self) -> Option<SystemTime> {
        next_start(chrono::Local::now(), &self.project.playback.start_time).map(SystemTime::from)
    }

    fn set_armed(&mut self, on: bool) {
        self.armed = if on && self.project.playback.auto_start { self.preselect } else { None };
        self.rearm();
    }

    /// Keep the audio thread's alarm pointing at the current preselection.
    fn rearm(&mut self) {
        if self.armed.is_some() {
            self.armed = self.preselect;
        }
        let arm = match (self.armed, self.start_at()) {
            (Some(cue), Some(at)) if self.project.playback.auto_start => Some((at, cue)),
            _ => None,
        };
        if arm.is_none() {
            self.armed = None;
        }
        self.send(Cmd::Arm(arm));
    }

    fn to_go(&self) -> String {
        match self.start_at() {
            Some(at) => {
                let s = at.duration_since(SystemTime::now()).unwrap_or_default().as_secs();
                format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
            }
            None => "--:--:--".into(),
        }
    }

    // ----------------------------------------------------------------- files

    fn request(&mut self, p: Pending) {
        if self.dirty {
            self.dialog = Dialog::Discard(p);
        } else {
            self.run_pending(p);
        }
    }

    fn run_pending(&mut self, p: Pending) {
        match p {
            Pending::New => {
                self.send(Cmd::Stop { fade_frames: 0 });
                self.project = Project::default();
                self.file = None;
                self.dirty = false;
                self.replaced = true;
                self.reset_show_state();
            }
            Pending::Open => {
                if let Some(path) = rfd::FileDialog::new().add_filter("Cue Spark show", &[EXTENSION]).pick_file() {
                    self.open_path(&path);
                }
            }
            Pending::OpenPath(path) => self.open_path(&path),
            Pending::Quit => {
                self.allow_close = true;
            }
        }
    }

    fn reset_show_state(&mut self) {
        self.preselect = None;
        self.armed = None;
        self.view_bank = 0;
        self.selected = 0;
        self.sync_slots();
        self.plan_dirty = true;
    }

    fn open_path(&mut self, path: &Path) {
        match Project::load(path) {
            Ok(p) => {
                self.send(Cmd::Stop { fade_frames: 0 });
                self.project = p;
                self.file = Some(path.to_path_buf());
                self.dirty = false;
                self.replaced = true;
                self.remember(path);
                self.reset_show_state();
            }
            Err(e) => self.dialog = Dialog::Message("Cannot open show".into(), e),
        }
    }

    fn remember(&mut self, path: &Path) {
        self.prefs.recent.retain(|p| p != path);
        self.prefs.recent.insert(0, path.to_path_buf());
        self.prefs.recent.truncate(8);
    }

    fn save(&mut self, choose: bool) -> bool {
        let path = match (&self.file, choose) {
            (Some(f), false) => f.clone(),
            _ => {
                let name = format!("{}.{EXTENSION}", self.project.name);
                match rfd::FileDialog::new().add_filter("Cue Spark show", &[EXTENSION]).set_file_name(name).save_file()
                {
                    Some(p) if p.extension().is_none() => p.with_extension(EXTENSION),
                    Some(p) => p,
                    None => return false,
                }
            }
        };
        match self.project.save(&path) {
            Ok(()) => {
                self.file = Some(path.clone());
                self.dirty = false;
                self.remember(&path);
                true
            }
            Err(e) => {
                self.dialog = Dialog::Message("Save failed".into(), e);
                false
            }
        }
    }

    fn assign_file(&mut self, cue: CueId, path: &Path) {
        let c = &mut self.project.cues[cue];
        c.path = path.to_string_lossy().into_owned();
        if c.name.is_empty() {
            c.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
    }

    fn browse(&mut self, cue: CueId) {
        let mut dialog = rfd::FileDialog::new().add_filter("Audio", AUDIO_EXTENSIONS);
        if let Some(dir) = Path::new(&self.project.cues[cue].path).parent().filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        } else if let Some(dir) = self.file.as_ref().and_then(|f| f.parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.assign_file(cue, &path);
        }
    }

    fn move_cue(&mut self, down: bool) {
        let a = self.selected;
        let Some(b) = (if down { a.checked_add(1).filter(|&b| b < CUE_COUNT) } else { a.checked_sub(1) }) else {
            return;
        };
        self.project.cues.swap(a, b);
        self.slots.swap(a, b);
        self.selected = b;
        self.plan_dirty = true;
    }

    fn reopen_audio(&mut self) {
        self.engine = None; // close the old stream before opening the device again
        self.devices = output_device_names();
        match Engine::open(self.prefs.device.as_deref()) {
            Ok(e) => {
                self.engine = Some(e);
                self.audio_error = None;
                self.sync_slots();
                self.plan_dirty = true;
            }
            Err(e) => self.audio_error = Some(e),
        }
        self.last_reconnect = Instant::now();
    }

    // ------------------------------------------------- per-frame background

    fn background(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.loader.done.try_recv() {
            // Job ids are unique; matching on them survives Up/Down swapping slots mid-decode.
            let Some(slot) = self.slots.iter_mut().find(|s| s.generation == done.generation) else {
                continue; // file changed while decoding
            };
            slot.state = match done.result {
                Ok(clip) => SlotState::Ready(Arc::new(clip)),
                Err(e) => SlotState::Failed(e),
            };
            self.plan_dirty = true;
        }
        if self.plan_dirty {
            self.rebuild_plan();
        }

        let lost = self.engine.as_ref().is_some_and(|e| e.status.stream_error.load(std::sync::atomic::Ordering::Relaxed));
        if lost {
            self.audio_error = Some("Audio device lost. Reconnecting...".into());
            // Clips are kept: sync_slots() only re-decodes if the new device runs at another rate.
            self.engine = None;
        }
        if self.engine.is_none() && self.last_reconnect.elapsed() > Duration::from_secs(2) {
            self.reopen_audio();
        }

        let (starts, autostarts) = match &mut self.engine {
            Some(e) => {
                e.collect_garbage();
                let st = &e.status;
                (st.starts.load(std::sync::atomic::Ordering::Relaxed), st.autostarted.load(std::sync::atomic::Ordering::Relaxed))
            }
            None => (self.seen_starts, self.seen_autostarts),
        };
        if autostarts != self.seen_autostarts {
            self.seen_autostarts = autostarts;
            self.armed = None;
        }
        if starts != self.seen_starts {
            self.seen_starts = starts;
            let pb = &self.project.playback;
            if pb.auto_preselect
                && let Some(cur) = self.status().cue
            {
                let next = if pb.preselect_next { self.project.step(cur, true, |c| self.has_clip(c)) } else { Some(cur) };
                if let Some(n) = next {
                    self.preselect = Some(n);
                    if self.mode == Mode::Show {
                        self.view_bank = n / CUES_PER_BANK;
                    }
                }
            }
        }

        let busy = self.status().cue.is_some()
            || self.armed.is_some()
            || self.slots.iter().any(|s| matches!(s.state, SlotState::Loading))
            || self.engine.is_none();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        let awake = self.mode == Mode::Show && self.prefs.keep_awake;
        if awake != self.keep_awake_on {
            self.keep_awake_on = awake;
            crate::set_keep_awake(awake);
        }
    }

    // ------------------------------------------------------------ keyboard

    /// Show Mode keys are handled straight from the raw OS events, before any widget
    /// sees them: a focused button can never swallow Space, and GO costs no frame.
    fn show_keys(&mut self, raw: &mut egui::RawInput) {
        let mut handled = Vec::new();
        for (i, ev) in raw.events.iter().enumerate() {
            let Event::Key { key, pressed, repeat, modifiers, .. } = ev else { continue };
            if modifiers.ctrl || modifiers.command || modifiers.alt {
                continue;
            }
            let fkeys = [Key::F1, Key::F2, Key::F3, Key::F4, Key::F5, Key::F6, Key::F7, Key::F8, Key::F9, Key::F10];
            let ours = fkeys.contains(key)
                || matches!(
                    key,
                    Key::Space
                        | Key::Enter
                        | Key::Escape
                        | Key::P
                        | Key::ArrowUp
                        | Key::ArrowDown
                        | Key::ArrowLeft
                        | Key::ArrowRight
                        | Key::PageUp
                        | Key::PageDown
                        | Key::Tab
                        | Key::Num1
                        | Key::Num2
                        | Key::Num3
                        | Key::Num4
                        | Key::Num5
                );
            if !ours {
                continue;
            }
            handled.push(i);
            if !pressed || *repeat {
                continue; // holding a key never re-fires
            }
            if let Some(n) = fkeys.iter().position(|k| k == key) {
                self.fire(self.view_bank * CUES_PER_BANK + n);
                continue;
            }
            match key {
                Key::Space | Key::Enter => self.go(),
                Key::Escape => self.stop(),
                Key::P => self.send(Cmd::TogglePause),
                Key::ArrowUp => self.step_preselect(false),
                Key::ArrowDown => self.step_preselect(true),
                Key::ArrowLeft | Key::PageUp => self.view_bank = self.view_bank.saturating_sub(1),
                Key::ArrowRight | Key::PageDown => self.view_bank = (self.view_bank + 1).min(BANKS - 1),
                Key::Tab => self.show_remaining = !self.show_remaining,
                Key::Num1 => self.view_bank = 0,
                Key::Num2 => self.view_bank = 1,
                Key::Num3 => self.view_bank = 2,
                Key::Num4 => self.view_bank = 3,
                Key::Num5 => self.view_bank = 4,
                _ => {}
            }
        }
        for i in handled.into_iter().rev() {
            raw.events.remove(i);
        }
        // Space also arrives as text input; drop it so nothing types it.
        raw.events.retain(|e| !matches!(e, Event::Text(t) if t == " "));
    }

    fn global_shortcuts(&mut self, ctx: &egui::Context) {
        let sc = |m, k| egui::KeyboardShortcut::new(m, k);
        let (toggle, new, open, save_as, save) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::E)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::N)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::O)),
                i.consume_shortcut(&sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::S)),
                i.consume_shortcut(&sc(Modifiers::COMMAND, Key::S)),
            )
        });
        if toggle {
            self.toggle_mode();
        }
        if self.mode == Mode::Edit {
            if new {
                self.request(Pending::New);
            }
            if open {
                self.request(Pending::Open);
            }
            if save_as {
                self.save(true);
            } else if save {
                self.save(false);
            }
            if !ctx.egui_wants_keyboard_input() {
                let (enter, esc, del, up, down) = ctx.input_mut(|i| {
                    (
                        i.consume_key(Modifiers::NONE, Key::Enter),
                        i.consume_key(Modifiers::NONE, Key::Escape),
                        i.consume_key(Modifiers::NONE, Key::Delete),
                        i.consume_key(Modifiers::ALT, Key::ArrowUp),
                        i.consume_key(Modifiers::ALT, Key::ArrowDown),
                    )
                });
                if enter {
                    self.fire(self.selected);
                }
                if esc {
                    self.stop();
                }
                if del {
                    self.project.cues[self.selected] = Cue::default();
                }
                if up || down {
                    self.move_cue(down);
                }
                let (sel_up, sel_down) =
                    ctx.input(|i| (i.key_pressed(Key::ArrowUp) && i.modifiers.is_none(), i.key_pressed(Key::ArrowDown) && i.modifiers.is_none()));
                if sel_up {
                    self.selected = self.selected.saturating_sub(1);
                }
                if sel_down {
                    self.selected = (self.selected + 1).min(CUE_COUNT - 1);
                }
            }
        }
    }

    fn toggle_mode(&mut self) {
        self.mode = if self.mode == Mode::Show { Mode::Edit } else { Mode::Show };
        if self.mode == Mode::Show
            && let Some(p) = self.preselect
        {
            self.view_bank = p / CUES_PER_BANK;
        }
    }

    // ============================================================ SHOW MODE

    fn show_mode(&mut self, ui: &mut Ui) {
        let snap = self.status();
        egui::Panel::top("show_top").frame(egui::Frame::new().fill(BLACK).inner_margin(6)).show(ui, |ui| {
            if ui.add(egui::Button::new(egui::RichText::new("Edit Mode").color(WHITE)).fill(BTN_BOTTOM)).clicked() {
                self.toggle_mode();
            }
        });
        egui::Panel::top("show_header")
            .exact_size(132.0)
            .frame(egui::Frame::new().fill(BLACK).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ui, |ui| self.show_header(ui, &snap));
        self.status_bar(ui, &snap);
        egui::Panel::bottom("brand")
            .exact_size(44.0)
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add(egui::Image::new(&self.logo).fit_to_exact_size(vec2(28.0, 28.0)));
                    ui.label(egui::RichText::new("Cue").color(WHITE).size(22.0).strong());
                    ui.add_space(-6.0);
                    ui.label(egui::RichText::new("Spark").color(RED).size(22.0).strong());
                });
            });
        egui::Panel::right("show_side")
            .exact_size(340.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 6, right: 14, top: 20, bottom: 6 }))
            .show(ui, |ui| self.show_side(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin { left: 14, right: 6, top: 20, bottom: 6 }))
            .show(ui, |ui| self.show_grid(ui, &snap));
    }

    fn show_header(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let rate = self.rate().max(1) as u64;
        let full = ui.available_rect_before_wrap();
        let transport_w = 330.0;
        let left = Rect::from_min_max(full.min, pos2(full.max.x - transport_w - 12.0, full.max.y));
        let p = ui.painter();

        // Big clock.
        let shown = if self.show_remaining { snap.len.saturating_sub(snap.pos) } else { snap.pos };
        let shown = if snap.cue.is_some() { shown } else { 0 };
        let clock = fmt_time(shown, rate);
        let small = FontId::proportional(10.0);
        let cx = left.min.x;
        let y0 = left.min.y + 4.0;
        for (txt, dx) in [("min", 0.0), ("sec", 82.0), ("msec", 165.0)] {
            p.text(pos2(cx + dx, y0), Align2::LEFT_TOP, txt, small.clone(), GREY);
        }
        let clock_rect = p.text(pos2(cx - 3.0, y0 + 10.0), Align2::LEFT_TOP, &clock, FontId::monospace(50.0), WHITE);
        let ela = pos2(clock_rect.max.x + 8.0, clock_rect.center().y - 12.0);
        p.text(ela, Align2::LEFT_TOP, "ELA", FontId::proportional(12.0), if self.show_remaining { DIM } else { WHITE });
        p.text(ela + vec2(0.0, 14.0), Align2::LEFT_TOP, "REM", FontId::proportional(12.0), if self.show_remaining { WHITE } else { DIM });
        let toggle = Rect::from_min_size(ela - vec2(4.0, 2.0), vec2(34.0, 32.0));
        if ui.interact(toggle, ui.scope_id().with("elarem"), Sense::click()).on_hover_text("Elapsed / remaining (Tab)").clicked() {
            self.show_remaining = !self.show_remaining;
        }

        // Bank total.
        let bx = ela.x + 50.0;
        let tot = self.bank_total(self.view_bank);
        let p = ui.painter();
        p.rect_filled(Rect::from_min_size(pos2(bx, y0), vec2(64.0, 14.0)), 0.0, Color32::from_gray(0x40));
        p.text(pos2(bx + 4.0, y0 + 1.0), Align2::LEFT_TOP, format!("BANK  {}", bank_letter(self.view_bank)), FontId::proportional(11.0), WHITE);
        p.rect_filled(Rect::from_min_size(pos2(bx + 66.0, y0), vec2(36.0, 14.0)), 0.0, Color32::from_gray(0x28));
        p.text(pos2(bx + 70.0, y0 + 1.0), Align2::LEFT_TOP, "TOT", FontId::proportional(11.0), GREY);
        for (txt, dx) in [("min", 0.0), ("sec", 26.0), ("msec", 52.0)] {
            p.text(pos2(bx + dx, y0 + 17.0), Align2::LEFT_TOP, txt, FontId::proportional(9.0), GREY);
        }
        p.text(pos2(bx, y0 + 26.0), Align2::LEFT_TOP, fmt_time(tot, rate), FontId::monospace(20.0), WHITE);

        // Now playing.
        if let Some(c) = snap.cue {
            let label = format!("{} {}  {}", bank_letter(c / CUES_PER_BANK), cue_key(c), self.project.cues[c].name);
            let color = if snap.paused { GREY } else { RED };
            let txt = if snap.paused { format!("PAUSED   {label}") } else { format!("PLAYING   {label}") };
            fit_text(p, pos2(bx, y0 + 54.0), Align2::LEFT_TOP, &txt, FontId::proportional(14.0), color, left.max.x - bx);
        }

        // Progress bar with ticks.
        let bar = Rect::from_min_max(pos2(left.min.x, full.max.y - 34.0), pos2(left.max.x, full.max.y - 20.0));
        p.rect_filled(bar, 0.0, Color32::from_gray(0x2a));
        if snap.len > 0 && snap.cue.is_some() {
            let f = (snap.pos as f32 / snap.len as f32).clamp(0.0, 1.0);
            p.rect_filled(Rect::from_min_max(bar.min, pos2(bar.min.x + bar.width() * f, bar.max.y)), 0.0, WHITE);
        }
        for i in 0..=10 {
            let x = bar.min.x + bar.width() * i as f32 / 10.0;
            p.line_segment([pos2(x, bar.min.y), pos2(x, bar.min.y + 6.0)], Stroke::new(1.0, Color32::from_gray(0x70)));
        }
        let len = if snap.cue.is_some() { snap.len } else { 0 };
        p.text(pos2(bar.min.x, bar.max.y + 3.0), Align2::LEFT_TOP, "00:00:000", FontId::proportional(10.0), GREY);
        p.text(pos2(bar.max.x, bar.max.y + 3.0), Align2::RIGHT_TOP, fmt_time(len, rate), FontId::proportional(10.0), GREY);

        // Transport.
        let tw = (transport_w - 3.0 * 4.0) / 4.0;
        let ty = full.max.y - 60.0;
        let icons = [Icon::Stop, Icon::Pause, Icon::Prev, Icon::Next];
        let tips = ["Stop (Esc, twice = cut)", "Pause / resume (P)", "Preselect previous cue (Up arrow)", "Preselect next cue (Down arrow)"];
        for (i, (icon, tip)) in icons.into_iter().zip(tips).enumerate() {
            let r = Rect::from_min_size(pos2(full.max.x - transport_w + i as f32 * (tw + 4.0), ty), vec2(tw, 40.0));
            let resp = ui.interact(r, ui.scope_id().with(("transport", i)), Sense::click()).on_hover_text(tip);
            let active = matches!(icon, Icon::Pause) && snap.paused;
            draw_button_bg(ui.painter(), r, if active { RED } else { BTN_TOP }, if active { RED_DARK } else { BTN_BOTTOM }, resp.hovered());
            draw_icon(ui.painter(), r.center(), icon, WHITE);
            if resp.clicked() {
                match icon {
                    Icon::Stop => self.stop(),
                    Icon::Pause => self.send(Cmd::TogglePause),
                    Icon::Prev => self.step_preselect(false),
                    Icon::Next => self.step_preselect(true),
                }
            }
        }
    }

    fn show_grid(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let full = ui.available_rect_before_wrap();
        ui.painter().rect_filled(full, 2.0, PANEL);
        ui.painter().rect_stroke(full, 2.0, Stroke::new(1.0, Color32::from_gray(0x3a)), StrokeKind::Inside);

        // Bank tabs.
        let tab_h = 22.0;
        let tab_w = full.width() / BANKS as f32;
        for b in 0..BANKS {
            let r = Rect::from_min_size(pos2(full.min.x + b as f32 * tab_w, full.min.y), vec2(tab_w, tab_h));
            let resp = ui.interact(r, ui.scope_id().with(("tab", b)), Sense::click());
            let active = b == self.view_bank;
            let p = ui.painter();
            p.rect_filled(r.shrink(1.0), 0.0, if active { PANEL } else { Color32::from_gray(0x26) });
            if active {
                p.line_segment([r.left_top() + vec2(1.0, 1.0), r.right_top() + vec2(-1.0, 1.0)], Stroke::new(2.0, RED));
            }
            let name = &self.project.banks[b].name;
            let name = if name.is_empty() { "--" } else { name };
            fit_text(p, r.center(), Align2::CENTER_CENTER, name, FontId::proportional(12.0), if active { RED } else { WHITE }, tab_w - 10.0);
            if resp.clicked() {
                self.view_bank = b;
            }
        }

        // 2 columns x 5 cue buttons, link brackets on the right of each.
        let area = Rect::from_min_max(pos2(full.min.x + 8.0, full.min.y + tab_h + 8.0), full.max - vec2(8.0, 8.0));
        let bracket = 34.0;
        let col_gap = 26.0;
        let bw = (area.width() - 2.0 * bracket - col_gap) / 2.0;
        let gap = 6.0;
        let bh = ((area.height() - 4.0 * gap) / 5.0).max(30.0);
        let bank = &self.project.banks[self.view_bank];
        let mut fire = None;
        let mut presel = None;
        for slot in 0..CUES_PER_BANK {
            let (col, row) = (slot / 5, slot % 5);
            let id = self.view_bank * CUES_PER_BANK + slot;
            let r = Rect::from_min_size(
                pos2(area.min.x + col as f32 * (bw + bracket + col_gap), area.min.y + row as f32 * (bh + gap)),
                vec2(bw, bh),
            );
            let resp = ui.interact(r, ui.scope_id().with(("cue", id)), Sense::click());
            let cue = &self.project.cues[id];
            let playing = snap.cue == Some(id);
            let p = ui.painter();
            let (top, bottom) = if playing { (RED, RED_DARK) } else { (BTN_TOP, BTN_BOTTOM) };
            draw_button_bg(p, r, top, bottom, resp.hovered() && !cue.is_empty());
            if self.preselect == Some(id) && !playing {
                p.rect_filled(Rect::from_min_size(r.min, vec2(12.0, r.height())), 0.0, RED);
            }
            let slot_state = &self.slots[id].state;
            let name_color = if cue.is_empty() { DIM } else { WHITE };
            let name = if cue.is_empty() { "" } else if cue.name.is_empty() { "(no name)" } else { &cue.name };
            let name_size = (bh * 0.24).clamp(12.0, 20.0);
            fit_text(p, pos2(r.center().x, r.min.y + bh * 0.25), Align2::CENTER_CENTER, name, FontId::proportional(name_size), name_color, bw - 28.0);
            p.line_segment(
                [pos2(r.min.x + 1.0, r.min.y + bh * 0.5), pos2(r.max.x - 1.0, r.min.y + bh * 0.5)],
                Stroke::new(1.0, if playing { RED_DARK } else { BLACK }),
            );
            let info_y = r.min.y + bh * 0.75;
            p.text(pos2(r.min.x + 20.0, info_y), Align2::LEFT_CENTER, cue_key(id), FontId::proportional(name_size * 0.85), name_color);
            let (right_text, right_color) = match slot_state {
                SlotState::Empty => (String::new(), DIM),
                SlotState::Loading => ("loading…".into(), GREY),
                SlotState::Failed(_) => ("MISSING".into(), Color32::from_rgb(0xff, 0x80, 0x40)),
                SlotState::Ready(c) => {
                    let rate = self.rate().max(1) as u64;
                    let t = if playing { snap.len.saturating_sub(snap.pos) } else { c.frames() as u64 };
                    (fmt_time(t, rate), WHITE)
                }
            };
            p.text(pos2(r.max.x - 14.0, info_y), Align2::RIGHT_CENTER, right_text, FontId::monospace(name_size * 0.85), right_color);

            // Link bracket towards the next cue.
            let linked = bank.link_cues && cue.link;
            let stroke = Stroke::new(3.0, if linked { RED } else { DIM });
            let x0 = r.max.x + 4.0;
            let x1 = x0 + 10.0;
            let yt = r.max.y - bh * 0.3;
            let yb = if row == 4 { r.max.y - 2.0 } else { r.max.y + gap + bh * 0.3 };
            p.line_segment([pos2(x0, yt), pos2(x1, yt)], stroke);
            p.line_segment([pos2(x1, yt), pos2(x1, yb)], stroke);
            if row != 4 {
                p.line_segment([pos2(x0, yb), pos2(x1, yb)], stroke);
            }

            let tip = match slot_state {
                SlotState::Failed(e) => format!("{}\n{}", cue.path, e),
                _ => format!("{}\nClick: fire · Right-click: preselect", cue.path),
            };
            let resp = if cue.is_empty() { resp } else { resp.on_hover_text(tip) };
            // Fire on press, not release: the cue goes as soon as the button is hit.
            if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                fire = Some(id);
            }
            if resp.secondary_clicked() {
                presel = Some(id);
            }
        }
        ui.painter().text(
            area.max + vec2(2.0, 4.0),
            Align2::RIGHT_BOTTOM,
            bank_letter(self.view_bank).to_string(),
            FontId::proportional(26.0),
            RED,
        );
        if let Some(id) = fire {
            self.fire(id);
        }
        if let Some(id) = presel.filter(|&c| self.has_clip(c)) {
            self.preselect = Some(id);
            self.rearm();
        }
    }

    fn show_side(&mut self, ui: &mut Ui) {
        let full = ui.available_rect_before_wrap();
        let half = (full.width() - 10.0) / 2.0;
        let mut y = full.min.y;
        let heading = |ui: &mut Ui, x: f32, w: f32, y: f32, txt: &str| {
            let p = ui.painter();
            p.text(pos2(x + w, y), Align2::RIGHT_TOP, txt, FontId::proportional(11.0), WHITE);
            p.line_segment([pos2(x, y + 16.0), pos2(x + w, y + 16.0)], Stroke::new(1.0, Color32::from_gray(0x60)));
        };
        heading(ui, full.min.x, half, y, "BANK SETTINGS");
        heading(ui, full.min.x + half + 10.0, half, y, "PLAYBACK SETTINGS");
        y += 22.0;
        let bw = (half - 4.0) / 2.0;
        let bh = 24.0;
        let b = self.view_bank;
        let auto_start = self.project.playback.auto_start;
        let mut armed = self.armed.is_some();
        let pb = &mut self.project.playback;
        let bank = &mut self.project.banks[b];
        let mut toggles: Vec<(f32, f32, &str, &mut bool, bool, &str)> = vec![
            (0.0, 0.0, "LINK CUES", &mut bank.link_cues, true, "Linked cues start the next one when they end"),
            (bw + 4.0, 0.0, "LOOP BNK", &mut bank.loop_bank, true, "After the last cue, go back to the first cue of this bank"),
            (0.0, bh + 4.0, "LINK BNK", &mut bank.link_bank, true, "After the last cue, continue with the next bank"),
            (half + 10.0, 0.0, "LOOP CUE", &mut pb.loop_cue, true, "Repeat the playing cue until stopped"),
            (half + 10.0 + bw + 4.0, 0.0, "PRESL NXT", &mut pb.auto_preselect, true, "Automatically preselect the next cue"),
            (half + 10.0, bh + 4.0, "AUTO STRT", &mut armed, auto_start, "Arm the automatic start (set the time in Edit Mode)"),
        ];
        for (i, (dx, dy, label, value, enabled, tip)) in toggles.iter_mut().enumerate() {
            let r = Rect::from_min_size(pos2(full.min.x + *dx, y + *dy), vec2(bw, bh));
            if led_button(ui, r, label, value, *enabled, ui.scope_id().with(("toggle", i))).on_hover_text(*tip).clicked() && *enabled {
                **value = !**value;
            }
        }
        if armed != self.armed.is_some() {
            self.set_armed(armed);
        }
        y += 2.0 * bh + 30.0;

        heading(ui, full.min.x, full.width(), y, "CUE INFORMATION");
        y += 24.0;
        let info = Rect::from_min_max(pos2(full.min.x, y), full.max);
        let p = ui.painter();
        p.rect_filled(info, 0.0, BLACK);
        if let Some(c) = self.preselect {
            let head = Rect::from_min_size(info.min, vec2(info.width(), 26.0));
            p.rect_filled(head, 0.0, Color32::from_gray(0x26));
            p.line_segment([pos2(info.min.x + 54.0, head.min.y), pos2(info.min.x + 54.0, head.max.y)], Stroke::new(1.0, BLACK));
            p.text(pos2(info.min.x + 8.0, head.center().y), Align2::LEFT_CENTER, cue_key(c), FontId::proportional(15.0), GREY);
            fit_text(p, pos2(info.min.x + 62.0, head.center().y), Align2::LEFT_CENTER, &self.project.cues[c].name, FontId::proportional(15.0), WHITE, info.width() - 70.0);
            let body = Rect::from_min_max(pos2(info.min.x + 8.0, head.max.y + 10.0), info.max - vec2(8.0, 8.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body));
            egui::ScrollArea::vertical().id_salt("notes").show(&mut child, |ui| {
                ui.label(egui::RichText::new(&self.project.cues[c].notes).color(WHITE).size(17.0).strong());
            });
        }
    }

    // ============================================================ EDIT MODE

    fn edit_mode(&mut self, ui: &mut Ui) {
        let snap = self.status();
        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Show Mode").on_hover_text("Ctrl+E").clicked() {
                    self.toggle_mode();
                }
                ui.separator();
                if ui.button("New").on_hover_text("New (Ctrl+N)").clicked() {
                    self.request(Pending::New);
                }
                if ui.button("Open").on_hover_text("Open (Ctrl+O)").clicked() {
                    self.request(Pending::Open);
                }
                if ui.button("Save").on_hover_text("Save (Ctrl+S)").clicked() {
                    self.save(false);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.update_widget(ui));
            });
        });
        egui::Panel::top("matrix_title")
            .frame(egui::Frame::new().fill(Color32::from_gray(0x22)).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Configuration Matrix").color(Color32::from_gray(0xd0)).size(18.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let shown = if snap.cue.is_some() { snap.pos } else { 0 };
                        ui.label(egui::RichText::new(fmt_time(shown, self.rate().max(1) as u64)).monospace().size(20.0).color(WHITE));
                        ui.label(egui::RichText::new("CLIP\nElapsed").size(10.0).color(Color32::from_gray(0xc0)));
                        let (r, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
                        draw_icon(ui.painter(), r.center(), Icon::Pause, if snap.paused { RED } else { WHITE });
                        if resp.on_hover_text("Pause / resume").clicked() {
                            self.send(Cmd::TogglePause);
                        }
                        let (r, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
                        draw_icon(ui.painter(), r.center(), Icon::Stop, WHITE);
                        if resp.on_hover_text("Stop (Esc)").clicked() {
                            self.stop();
                        }
                    });
                });
            });
        self.status_bar(ui, &snap);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Project:");
                ui.add(egui::TextEdit::singleline(&mut self.project.name).desired_width(240.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.strong(&self.project.modified);
                    ui.label("Date of last change:");
                    ui.add_space(20.0);
                    ui.strong(&self.project.created);
                    ui.label("Date of creation:");
                });
            });
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                let w = ui.available_width();
                ui.allocate_ui(vec2(w * 0.58, 170.0), |ui| group(ui, "Bank Settings", |ui| self.bank_table(ui)));
                ui.allocate_ui(vec2(ui.available_width(), 170.0), |ui| group(ui, "Playback Settings", |ui| self.playback_box(ui)));
            });
            ui.add_space(6.0);
            let bottom_h = 120.0;
            let table_h = (ui.available_height() - bottom_h).max(150.0);
            ui.allocate_ui(vec2(ui.available_width(), table_h), |ui| group(ui, "Cue Settings", |ui| self.cue_table(ui, &snap)));
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    if ui.button("Up").on_hover_text("Move cue up (Alt+Up)").clicked() {
                        self.move_cue(false);
                    }
                    if ui.button("Down").on_hover_text("Move cue down (Alt+Down)").clicked() {
                        self.move_cue(true);
                    }
                });
                ui.vertical(|ui| {
                    let c = self.selected;
                    ui.label(format!("Cue information for {} {}  (shown in Show Mode when preselected)", bank_letter(c / CUES_PER_BANK), cue_key(c)));
                    ui.add(
                        egui::TextEdit::multiline(&mut self.project.cues[c].notes)
                            .desired_width(f32::INFINITY)
                            .desired_rows(4)
                            .hint_text("Script line, stage cue, reminder…"),
                    );
                });
            });
        });
    }

    /// Right side of the Edit Mode toolbar. Never shown in Show Mode.
    fn update_widget(&mut self, ui: &mut Ui) {
        let state = self.update.lock().map(|s| s.clone()).unwrap_or(update::State::Idle);
        let ctx = ui.ctx().clone();
        match state {
            update::State::Idle => {}
            update::State::Checking => {
                if self.update_manual {
                    ui.label("Checking for updates…");
                }
            }
            update::State::UpToDate => {
                if self.update_manual {
                    ui.label(format!("Cue Spark {VERSION} is up to date"));
                }
            }
            update::State::Failed(e) => {
                if self.update_manual {
                    ui.colored_label(Color32::from_rgb(0xc0, 0x20, 0x20), "Update failed").on_hover_text(e);
                }
            }
            update::State::Available(u) => {
                let label = egui::RichText::new(format!("Update to {}", u.version)).color(Color32::WHITE).strong();
                if ui.add(egui::Button::new(label).fill(RED)).on_hover_text("Download and install the new version").clicked() {
                    if self.status().cue.is_some() {
                        self.dialog = Dialog::Message("Update".into(), "Stop playback first.".into());
                    } else if self.dirty {
                        self.dialog = Dialog::Message("Update".into(), "Save the show first (Ctrl+S).".into());
                    } else {
                        update::install(u, &self.update, &ctx);
                    }
                }
            }
            update::State::Downloading(v) => {
                ui.add(egui::Spinner::new());
                ui.label(format!("Downloading {v}…"));
            }
            update::State::Installed(v) => {
                let label = egui::RichText::new(format!("Restart to use {v}")).color(Color32::WHITE).strong();
                if ui.add(egui::Button::new(label).fill(RED)).clicked() && self.status().cue.is_none() && !self.dirty {
                    match update::restart(self.file.as_deref()) {
                        Ok(()) => {
                            self.allow_close = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(e) => self.dialog = Dialog::Message("Restart failed".into(), e),
                    }
                }
            }
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New            Ctrl+N").clicked() {
                    self.request(Pending::New);
                }
                if ui.button("Open…          Ctrl+O").clicked() {
                    self.request(Pending::Open);
                }
                ui.menu_button("Open recent", |ui| {
                    for p in self.prefs.recent.clone() {
                        if ui.button(p.display().to_string()).clicked() {
                            self.request(Pending::OpenPath(p));
                        }
                    }
                });
                if ui.button("Save           Ctrl+S").clicked() {
                    self.save(false);
                }
                if ui.button("Save as…  Ctrl+Shift+S").clicked() {
                    self.save(true);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Project", |ui| {
                if ui.button("Reload all audio files").clicked() {
                    for s in &mut self.slots {
                        s.path.clear();
                    }
                    self.sync_slots();
                }
                ui.checkbox(&mut self.prefs.reopen_last, "Reopen last show at startup");
            });
            ui.menu_button("Preferences", |ui| {
                ui.label("Audio output");
                let mut choice = self.prefs.device.clone();
                ui.radio_value(&mut choice, None, "System default");
                for name in &self.devices {
                    ui.radio_value(&mut choice, Some(name.clone()), name.as_str());
                }
                if ui.button("Refresh device list").clicked() {
                    self.devices = output_device_names();
                }
                if choice != self.prefs.device {
                    self.prefs.device = choice;
                    self.reopen_audio();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Stop fade (Esc)");
                    ui.add(egui::DragValue::new(&mut self.prefs.stop_fade_ms).range(0..=10_000).suffix(" ms").speed(10));
                });
                ui.checkbox(&mut self.prefs.keep_awake, "Keep screen awake in Show Mode");
                ui.checkbox(&mut self.prefs.check_updates, "Check for updates at startup");
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts").clicked() {
                    self.dialog = Dialog::Shortcuts;
                }
                if ui.button("Check for updates").clicked() {
                    self.update_manual = true;
                    update::check(&self.update, ui.ctx());
                }
                if ui.button("About").clicked() {
                    self.dialog = Dialog::About;
                }
            });
        });
    }

    fn bank_table(&mut self, ui: &mut Ui) {
        let rate = self.rate().max(1) as u64;
        egui::Grid::new("banks").striped(true).spacing(vec2(14.0, 4.0)).show(ui, |ui| {
            for h in ["Bank", "Link Cues", "Link Bank", "Loop Bank", "Bank Name", "Length"] {
                ui.strong(h);
            }
            ui.end_row();
            for b in 0..BANKS {
                ui.label(bank_letter(b).to_string());
                let bank = &mut self.project.banks[b];
                ui.checkbox(&mut bank.link_cues, "");
                ui.checkbox(&mut bank.link_bank, "");
                ui.checkbox(&mut bank.loop_bank, "");
                ui.add(egui::TextEdit::singleline(&mut bank.name).desired_width(160.0));
                let tot = self.bank_total(b);
                ui.monospace(if tot == 0 { "--:--:---".into() } else { fmt_time(tot, rate) });
                ui.end_row();
            }
        });
        ui.label(format!(
            "Audio out: {}",
            self.engine.as_ref().map(|e| format!("{} ({} Hz)", e.device_name, e.rate)).unwrap_or_else(|| "none".into())
        ));
    }

    fn playback_box(&mut self, ui: &mut Ui) {
        let pb = &mut self.project.playback;
        ui.checkbox(&mut pb.auto_preselect, "Automatic Preselection");
        ui.add_enabled_ui(pb.auto_preselect, |ui| {
            ui.indent("presel", |ui| {
                ui.radio_value(&mut pb.preselect_next, true, "Preselect next Cue");
                ui.radio_value(&mut pb.preselect_next, false, "Preselect current Cue");
            });
        });
        ui.separator();
        ui.checkbox(&mut pb.auto_start, "Automatic Start");
        let mut arm_click = false;
        let valid = parse_hhmm(&pb.start_time).is_some();
        let auto_start = pb.auto_start;
        ui.add_enabled_ui(auto_start, |ui| {
            ui.horizontal(|ui| {
                ui.label("Start time:");
                ui.add(egui::TextEdit::singleline(&mut pb.start_time).desired_width(60.0).hint_text("HH:MM"));
                if !valid {
                    ui.colored_label(Color32::from_rgb(0xc0, 0x20, 0x20), "use HH:MM");
                }
            });
        });
        let armed = self.armed.is_some();
        ui.add_enabled_ui(auto_start && valid, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("To go: {}", self.to_go()));
                let label = if armed { "Disarm" } else { "Arm" };
                arm_click = ui
                    .add(egui::Button::new(label).selected(armed))
                    .on_hover_text("When armed, the preselected cue fires at the start time")
                    .clicked();
            });
        });
        if arm_click {
            self.set_armed(!armed);
        }
    }

    fn cue_table(&mut self, ui: &mut Ui, snap: &Snapshot) {
        use egui_extras::{Column, TableBuilder};
        let rate = self.rate().max(1) as u64;
        let mut browse = None;
        let mut reset = None;
        let mut preview = None;
        let mut hovered_row = None;
        TableBuilder::new(ui)
            .striped(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::exact(36.0))
            .column(Column::exact(44.0))
            .column(Column::exact(32.0))
            .column(Column::initial(170.0).resizable(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .column(Column::exact(86.0))
            .column(Column::exact(70.0))
            .column(Column::exact(40.0))
            .column(Column::exact(60.0))
            .header(20.0, |mut h| {
                for t in ["Bank", "Cue", "Link", "Cue Name", "Source path of the Clip", "Length", "Size", "Type", ""] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, CUE_COUNT, |mut row| {
                    let c = row.index();
                    row.set_selected(c == self.selected);
                    let playing = snap.cue == Some(c);
                    row.col(|ui| {
                        ui.label(bank_letter(c / CUES_PER_BANK).to_string());
                    });
                    row.col(|ui| {
                        let t = egui::RichText::new(cue_key(c));
                        ui.label(if playing { t.color(RED).strong() } else { t });
                    });
                    let cue = &mut self.project.cues[c];
                    row.col(|ui| {
                        ui.checkbox(&mut cue.link, "");
                    });
                    row.col(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut cue.name).desired_width(f32::INFINITY));
                    });
                    row.col(|ui| {
                        if ui.small_button("…").on_hover_text("Choose audio file").clicked() {
                            browse = Some(c);
                        }
                        let slot = &self.slots[c];
                        let txt = egui::RichText::new(&cue.path);
                        match &slot.state {
                            SlotState::Failed(e) => {
                                ui.label(txt.color(Color32::from_rgb(0xc0, 0x20, 0x20))).on_hover_text(e);
                            }
                            _ => {
                                ui.label(txt).on_hover_text(&cue.path);
                            }
                        }
                    });
                    let slot = &self.slots[c];
                    row.col(|ui| {
                        ui.monospace(match &slot.state {
                            SlotState::Ready(clip) => fmt_time(clip.frames() as u64, rate),
                            SlotState::Loading => "loading…".into(),
                            _ => "--:--:---".into(),
                        });
                    });
                    row.col(|ui| {
                        if let Some(s) = slot.size.filter(|_| !cue.path.is_empty()) {
                            ui.label(format!("{:.2} MB", s as f64 / 1_048_576.0));
                        }
                    });
                    row.col(|ui| {
                        let ext = Path::new(&cue.path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                        ui.label(ext);
                    });
                    row.col(|ui| {
                        if ui.small_button("RESET").on_hover_text("Clear this cue (Del)").clicked() {
                            reset = Some(c);
                        }
                    });
                    let resp = row.response();
                    if resp.hovered() {
                        hovered_row = Some(c);
                    }
                    if resp.clicked() {
                        self.selected = c;
                    }
                    if resp.double_clicked() {
                        preview = Some(c);
                    }
                });
            });
        if let Some(c) = browse {
            self.selected = c;
            self.browse(c);
        }
        if let Some(c) = reset {
            self.project.cues[c] = Cue::default();
        }
        if let Some(c) = preview {
            self.fire(c);
        }
        // Dropped files fill consecutive cues from the row under the mouse (or the selected one).
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            let start = hovered_row.unwrap_or(self.selected);
            for (i, path) in dropped.iter().filter(|p| is_audio(p)).take(CUE_COUNT - start).enumerate() {
                self.assign_file(start + i, path);
            }
            self.selected = start;
        }
    }

    // ============================================================== shared

    fn status_bar(&mut self, ui: &mut Ui, snap: &Snapshot) {
        let dark = self.mode == Mode::Show;
        let frame = egui::Frame::new()
            .fill(if dark { BLACK } else { Color32::from_gray(0xe4) })
            .inner_margin(egui::Margin::symmetric(10, 3));
        egui::Panel::bottom("status").frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                let text = if dark { WHITE } else { Color32::from_gray(0x20) };
                ui.label(egui::RichText::new(if dark { "Show Mode is active" } else { "Edit Mode is active" }).color(text));
                ui.separator();
                if let Some(err) = &self.audio_error {
                    ui.label(egui::RichText::new(format!("AUDIO: {err}")).color(Color32::from_rgb(0xff, 0x50, 0x50)).strong());
                } else {
                    let loading = self.slots.iter().filter(|s| matches!(s.state, SlotState::Loading)).count();
                    let failed = self.slots.iter().filter(|s| matches!(s.state, SlotState::Failed(_))).count();
                    if loading > 0 {
                        ui.label(egui::RichText::new(format!("Loading {loading} clip(s)…")).color(Color32::from_rgb(0xe0, 0xa0, 0x20)));
                    } else if failed > 0 {
                        ui.label(egui::RichText::new(format!("{failed} clip(s) missing or unreadable")).color(Color32::from_rgb(0xff, 0x70, 0x30)));
                    } else if let Some(c) = snap.cue.or(self.preselect) {
                        // Leave room for the project name on the right; long paths end in "…".
                        ui.set_max_width(ui.available_width() - 260.0);
                        ui.add(egui::Label::new(egui::RichText::new(&self.project.cues[c].path).color(text)).truncate());
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !dark {
                        ui.label(egui::RichText::new(format!("v{VERSION}")).color(Color32::from_gray(0x70)));
                        ui.separator();
                    }
                    let name = format!("{}{}", self.project.name, if self.dirty { " *" } else { "" });
                    ui.label(egui::RichText::new(name).color(text));
                    ui.separator();
                    if self.armed.is_some() {
                        ui.label(egui::RichText::new(format!("AUTO START in {}", self.to_go())).color(RED).strong());
                    }
                });
            });
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut then = None;
        let mut quit_show = false;
        let mut licenses = false;
        match &self.dialog {
            Dialog::None => return,
            Dialog::Discard(p) => {
                let p = p.clone();
                egui::Modal::new(egui::Id::unique("discard")).show(ctx, |ui| {
                    ui.heading("Unsaved changes");
                    ui.label(format!("Save changes to \"{}\"?", self.project.name));
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            close = true;
                            then = Some((p.clone(), true));
                        }
                        if ui.button("Discard").clicked() {
                            close = true;
                            then = Some((p.clone(), false));
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::QuitShow => {
                egui::Modal::new(egui::Id::unique("quitshow")).show(ctx, |ui| {
                    ui.heading("Show Mode is active");
                    ui.label("Really quit? Playback stops immediately.");
                    ui.horizontal(|ui| {
                        if ui.button("Quit").clicked() {
                            quit_show = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Message(title, msg) => {
                egui::Modal::new(egui::Id::unique("msg")).show(ctx, |ui| {
                    ui.heading(title.as_str());
                    ui.label(msg.as_str());
                    if ui.button("OK").clicked() {
                        close = true;
                    }
                });
            }
            Dialog::Shortcuts => {
                egui::Modal::new(egui::Id::unique("keys")).show(ctx, |ui| {
                    ui.heading("Keyboard shortcuts");
                    egui::Grid::new("keys_grid").striped(true).show(ui, |ui| {
                        for (k, v) in SHORTCUTS {
                            ui.strong(*k);
                            ui.label(*v);
                            ui.end_row();
                        }
                    });
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            }
            Dialog::About => {
                egui::Modal::new(egui::Id::unique("about")).show(ctx, |ui| {
                    ui.heading(format!("Cue Spark {VERSION}"));
                    ui.label("Instant playback of show sound cues.");
                    ui.hyperlink("https://github.com/Kirato77/cuespark");
                    ui.add_space(6.0);
                    ui.label("Inspired by the workflow of CueFire FREE. Not affiliated with audiobytes;");
                    ui.label("CueFire is their name. No code from it is used.");
                    ui.horizontal(|ui| {
                        if ui.button("Third-party licenses").clicked() {
                            licenses = true;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            }
            Dialog::Licenses => {
                egui::Modal::new(egui::Id::unique("licenses")).show(ctx, |ui| {
                    ui.heading("Third-party licenses");
                    let max = ctx.content_rect().size() * vec2(0.8, 0.7);
                    // ~7000 lines: only lay out the visible ones.
                    let lines: Vec<&str> = THIRD_PARTY_LICENSES.lines().collect();
                    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                    egui::ScrollArea::vertical().max_height(max.y).show_rows(ui, row_h, lines.len(), |ui, rows| {
                        ui.set_width(max.x);
                        for line in &lines[rows] {
                            ui.add(egui::Label::new(egui::RichText::new(*line).monospace()).extend());
                        }
                    });
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            }
        }
        if quit_show {
            if self.dirty {
                self.dialog = Dialog::Discard(Pending::Quit);
            } else {
                close = true;
                then = Some((Pending::Quit, false));
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
        if licenses {
            self.dialog = Dialog::Licenses;
        }
        if let Some((p, save_first)) = then {
            if !save_first || self.save(false) {
                self.dirty = false;
                self.run_pending(p);
                if self.allow_close {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.allow_close {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if !matches!(self.dialog, Dialog::None) {
            return;
        }
        self.dialog = if self.mode == Mode::Show {
            Dialog::QuitShow
        } else if self.dirty {
            Dialog::Discard(Pending::Quit)
        } else {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            Dialog::None
        };
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        if self.mode == Mode::Show {
            let mut v = egui::Visuals::dark();
            v.panel_fill = BG;
            ctx.set_visuals(v);
        } else {
            let mut v = egui::Visuals::light();
            v.selection.bg_fill = ROW_SELECTED;
            v.selection.stroke = Stroke::new(1.0, Color32::from_gray(0x30));
            ctx.set_visuals(v);
        }
    }
}

const SHORTCUTS: &[(&str, &str)] = &[
    ("Show Mode", ""),
    ("F1 … F10", "Fire cue of the visible bank"),
    ("Space / Enter", "GO: fire the preselected cue"),
    ("Esc", "Stop (with the stop fade). Twice: cut now"),
    ("P", "Pause / resume"),
    ("Up / Down arrows", "Preselect previous / next cue"),
    ("Left / Right, 1 … 5", "Show previous / next bank, or bank A … E"),
    ("Tab", "Elapsed / remaining time"),
    ("Right-click a cue", "Preselect it"),
    ("Edit Mode", ""),
    ("Enter / double-click", "Preview the selected cue"),
    ("Del", "Clear the selected cue"),
    ("Alt+Up / Alt+Down", "Move the selected cue"),
    ("Drop files", "Fill cues from the row under the mouse"),
    ("Ctrl+N / O / S", "New / open / save (Ctrl+Shift+S: save as)"),
    ("Both modes", ""),
    ("Ctrl+E", "Switch Show / Edit Mode"),
];

impl eframe::App for App {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if self.mode == Mode::Show && matches!(self.dialog, Dialog::None) {
            self.show_keys(raw);
        }
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.background(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.apply_theme(&ctx);
        self.handle_close(&ctx);
        if matches!(self.dialog, Dialog::None) {
            self.global_shortcuts(&ctx);
        }
        let before = self.project.clone();

        match self.mode {
            Mode::Show => self.show_mode(ui),
            Mode::Edit => self.edit_mode(ui),
        }
        self.dialogs(&ctx);

        if !std::mem::take(&mut self.replaced) && self.project != before {
            if self.project.cues != before.cues || self.project.banks != before.banks || self.project.playback != before.playback {
                self.project.modified = today();
            }
            self.dirty = true;
            self.sync_slots();
            self.plan_dirty = true;
        }
        let title = format!(
            "Cue Spark {VERSION}  |  {}{}",
            self.file.as_ref().and_then(|f| f.file_name()).map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| self.project.name.clone()),
            if self.dirty { " *" } else { "" }
        );
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "prefs", &self.prefs);
    }
}

// ================================================================ drawing

const THIRD_PARTY_LICENSES: &str = include_str!("../THIRD_PARTY_LICENSES.txt");

/// Paint one line of text, cut with "…" when wider than `max_w`.
fn fit_text(p: &egui::Painter, pos: Pos2, align: Align2, text: &str, font: FontId, color: Color32, max_w: f32) {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(10.0));
    let galley = p.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    p.galley(rect.min, galley, color);
}

fn is_audio(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// mm:ss:mmm, like the original's displays.
pub fn fmt_time(frames: u64, rate: u64) -> String {
    let ms = frames * 1000 / rate.max(1);
    format!("{:02}:{:02}:{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

fn group(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_size(ui.available_size());
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).strong().size(13.0));
            ui.separator();
            add(ui);
        });
    });
}

fn draw_button_bg(p: &egui::Painter, r: Rect, top: Color32, bottom: Color32, hovered: bool) {
    let (top, bottom) = if hovered { (top.gamma_multiply(1.25), bottom.gamma_multiply(1.25)) } else { (top, bottom) };
    p.rect_filled(r, CornerRadius::same(2), bottom);
    p.rect_filled(Rect::from_min_max(r.min, pos2(r.max.x, r.center().y)), CornerRadius { nw: 2, ne: 2, sw: 0, se: 0 }, top);
    p.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, Color32::from_gray(0x48)), StrokeKind::Inside);
}

fn led_button(ui: &mut Ui, r: Rect, label: &str, value: &bool, enabled: bool, id: egui::Id) -> egui::Response {
    let resp = ui.interact(r, id, if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    draw_button_bg(p, r, Color32::from_gray(0x2c), Color32::from_gray(0x22), resp.hovered() && enabled);
    let led = Rect::from_min_size(pos2(r.max.x - 9.0, r.min.y + 3.0), vec2(6.0, r.height() - 6.0));
    p.rect_filled(led, 1.0, if *value && enabled { RED } else { Color32::from_gray(0x14) });
    p.text(pos2(r.center().x - 4.0, r.center().y), Align2::CENTER_CENTER, label, FontId::proportional(10.5), if enabled { WHITE } else { DIM });
    resp
}

#[derive(Clone, Copy)]
enum Icon {
    Stop,
    Pause,
    Prev,
    Next,
}

fn draw_icon(p: &egui::Painter, c: Pos2, icon: Icon, color: Color32) {
    let s = 9.0;
    match icon {
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(2.0 * s)), 0.0, color);
        }
        Icon::Pause => {
            p.rect_filled(Rect::from_center_size(c - vec2(5.0, 0.0), vec2(5.0, 2.0 * s)), 0.0, color);
            p.rect_filled(Rect::from_center_size(c + vec2(5.0, 0.0), vec2(5.0, 2.0 * s)), 0.0, color);
        }
        Icon::Prev | Icon::Next => {
            let d = if matches!(icon, Icon::Next) { 1.0 } else { -1.0 };
            for off in [-5.0, 5.0] {
                let x = c.x + off;
                let tri = vec![pos2(x - d * 6.0, c.y - s), pos2(x + d * 6.0, c.y), pos2(x - d * 6.0, c.y + s)];
                p.add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fmt_time;

    #[test]
    fn time_format() {
        assert_eq!(fmt_time(0, 48000), "00:00:000");
        assert_eq!(fmt_time(48000 * 270 + 16800, 48000), "04:30:350");
        assert_eq!(fmt_time(48000 * 3600, 48000), "60:00:000");
    }
}
