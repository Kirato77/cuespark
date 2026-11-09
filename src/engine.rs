//! Real-time playback. The audio callback never locks, allocates or frees:
//! commands arrive through a lock-free ring, state goes out through atomics,
//! and anything that must be freed is shipped back to the UI thread.

use crate::decode::Clip;
use crate::project::{CUE_COUNT, CueId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::SystemTime;

pub const NONE: usize = usize::MAX;

/// What the audio thread needs to know about the show, rebuilt by the UI on every change.
pub struct Plan {
    pub clips: Vec<Option<Arc<Clip>>>,
    pub next: Vec<Option<CueId>>,
    pub loop_cue: bool,
}

impl Plan {
    pub fn empty() -> Self {
        Self { clips: vec![None; CUE_COUNT], next: vec![None; CUE_COUNT], loop_cue: false }
    }
}

pub enum Cmd {
    Fire(CueId),
    /// Fade out the playing cue. Sent while a fade is already running, it cuts at once.
    Stop { fade_frames: u32 },
    TogglePause,
    SetPlan(Arc<Plan>),
    /// Fire `cue` once `at` is reached (automatic start). None disarms.
    Arm(Option<(SystemTime, CueId)>),
}

/// Things the audio thread drops by handing them back to the UI thread.
#[allow(dead_code)] // fields are only held so they get dropped on the UI side
pub enum Garbage {
    Plan(Arc<Plan>),
    Clip(Arc<Clip>),
}

#[derive(Default)]
pub struct Status {
    pub cue: AtomicUsize,
    pub pos: AtomicU64,
    pub len: AtomicU64,
    /// 0 stopped, 1 playing, 2 paused
    pub state: AtomicU8,
    /// Bumped every time a cue starts (manual, linked or automatic).
    pub starts: AtomicU64,
    pub autostarted: AtomicU64,
    pub stream_error: AtomicBool,
}

const STOPPED: u8 = 0;
const PLAYING: u8 = 1;
const PAUSED: u8 = 2;

pub struct Snapshot {
    pub cue: Option<CueId>,
    pub pos: u64,
    pub len: u64,
    pub paused: bool,
}

impl Status {
    fn new() -> Self {
        let s = Self::default();
        s.cue.store(NONE, Relaxed);
        s
    }

    pub fn snapshot(&self) -> Snapshot {
        let cue = self.cue.load(Relaxed);
        let state = self.state.load(Relaxed);
        Snapshot {
            cue: (cue != NONE && state != STOPPED).then_some(cue),
            pos: self.pos.load(Relaxed),
            len: self.len.load(Relaxed),
            paused: state == PAUSED,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AtTarget {
    Hold,
    Pause,
    Kill,
}

struct Voice {
    cue: CueId,
    clip: Arc<Clip>,
    pos: usize,
    gain: f32,
    target: f32,
    step: f32,
    at_target: AtTarget,
}

impl Voice {
    fn new(cue: CueId, clip: Arc<Clip>, fade_in: u32) -> Self {
        let gain = if fade_in == 0 { 1.0 } else { 0.0 };
        let mut v = Voice { cue, clip, pos: 0, gain, target: 1.0, step: 1.0, at_target: AtTarget::Hold };
        v.ramp(1.0, fade_in, AtTarget::Hold);
        v
    }

    fn ramp(&mut self, target: f32, frames: u32, then: AtTarget) {
        self.target = target;
        self.step = (target - self.gain).abs() / frames.max(1) as f32;
        self.at_target = then;
    }

    fn at_target(&self) -> bool {
        self.gain == self.target
    }

    fn tick_gain(&mut self) {
        if self.gain < self.target {
            self.gain = (self.gain + self.step).min(self.target);
        } else if self.gain > self.target {
            self.gain = (self.gain - self.step).max(self.target);
        }
    }

    fn frame(&self) -> (f32, f32) {
        let i = self.pos * 2;
        let k = self.gain / 32768.0;
        (self.clip.data[i] as f32 * k, self.clip.data[i + 1] as f32 * k)
    }
}

const MAX_FADING: usize = 8;

pub struct Mixer {
    plan: Arc<Plan>,
    current: Option<Voice>,
    paused: bool,
    fading: Vec<Voice>,
    armed: Option<(SystemTime, CueId)>,
    declick: u32,
    fade_in: u32,
    cmds: rtrb::Consumer<Cmd>,
    garbage: rtrb::Producer<Garbage>,
    status: Arc<Status>,
}

impl Mixer {
    fn new(rate: u32, cmds: rtrb::Consumer<Cmd>, garbage: rtrb::Producer<Garbage>, status: Arc<Status>) -> Self {
        Self {
            plan: Arc::new(Plan::empty()),
            current: None,
            paused: false,
            fading: Vec::with_capacity(MAX_FADING),
            armed: None,
            declick: rate / 200, // 5 ms: inaudible as a fade, removes the click of a hard cut
            fade_in: (rate / 1000).max(1), // 1 ms: keeps transients, avoids a click on non-zero starts
            cmds,
            garbage,
            status,
        }
    }

    fn trash(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            // UI stopped draining: leaking beats freeing memory on the audio thread.
            std::mem::forget(g);
        }
    }

    fn retire(&mut self, v: Voice) {
        self.trash(Garbage::Clip(v.clip));
    }

    /// Never grows past its preallocated capacity: growing would allocate in the callback.
    fn push_fading(&mut self, v: Voice) {
        if self.fading.len() == MAX_FADING {
            let oldest = self.fading.remove(0);
            self.retire(oldest);
        }
        self.fading.push(v);
    }

    fn start(&mut self, cue: CueId, fade_in: u32) {
        let Some(clip) = self.plan.clips.get(cue).cloned().flatten() else { return };
        if let Some(mut old) = self.current.take() {
            old.ramp(0.0, self.declick, AtTarget::Kill);
            self.push_fading(old);
        }
        self.status.len.store(clip.frames() as u64, Relaxed);
        self.status.pos.store(0, Relaxed);
        self.status.cue.store(cue, Relaxed);
        self.status.state.store(PLAYING, Relaxed);
        self.status.starts.fetch_add(1, Relaxed);
        self.current = Some(Voice::new(cue, clip, fade_in));
        self.paused = false;
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Fire(cue) => self.start(cue, self.fade_in),
            Cmd::Stop { fade_frames } => {
                if let Some(mut v) = self.current.take() {
                    // A paused voice is already silent: just drop it.
                    v.ramp(0.0, if self.paused { 1 } else { fade_frames.max(self.declick) }, AtTarget::Kill);
                    self.push_fading(v);
                    self.paused = false;
                    self.status.state.store(STOPPED, Relaxed);
                    self.status.cue.store(NONE, Relaxed);
                } else {
                    for v in &mut self.fading {
                        let d = self.declick;
                        v.ramp(0.0, d, AtTarget::Kill);
                    }
                }
            }
            Cmd::TogglePause => {
                let d = self.declick;
                if let Some(v) = &mut self.current {
                    if self.paused {
                        self.paused = false;
                        v.ramp(1.0, d, AtTarget::Hold);
                        self.status.state.store(PLAYING, Relaxed);
                    } else {
                        self.paused = true;
                        v.ramp(0.0, d, AtTarget::Pause);
                        self.status.state.store(PAUSED, Relaxed);
                    }
                }
            }
            Cmd::SetPlan(plan) => {
                let old = std::mem::replace(&mut self.plan, plan);
                self.trash(Garbage::Plan(old));
            }
            Cmd::Arm(a) => self.armed = a,
        }
    }

    /// Called at the end of the playing clip: loop, follow the link, or stop.
    fn on_end(&mut self) {
        let cue = self.current.as_ref().map(|v| v.cue).unwrap_or(NONE);
        if self.plan.loop_cue {
            if let Some(v) = &mut self.current {
                v.pos = 0;
            }
            return;
        }
        let next = self.plan.next.get(cue).copied().flatten().filter(|&n| self.plan.clips[n].is_some());
        if let Some(v) = self.current.take() {
            self.retire(v);
        }
        match next {
            Some(n) => self.start(n, 0), // gapless: the follower starts on the very next sample
            None => {
                self.status.state.store(STOPPED, Relaxed);
                self.status.cue.store(NONE, Relaxed);
            }
        }
    }

    pub fn render<T: Sample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(cmd) = self.cmds.pop() {
            self.handle(cmd);
        }
        if let Some((at, cue)) = self.armed
            // Wall clock, not a monotonic one: survives suspend and NTP corrections.
            && SystemTime::now() >= at
        {
            self.armed = None;
            self.start(cue, self.fade_in);
            self.status.autostarted.fetch_add(1, Relaxed);
        }

        for frame in out.chunks_mut(channels) {
            let (mut l, mut r) = (0.0f32, 0.0f32);

            let mut ended = false;
            if let Some(v) = &mut self.current {
                let frozen = v.at_target == AtTarget::Pause && v.at_target();
                if !frozen {
                    let (a, b) = v.frame();
                    l += a;
                    r += b;
                    v.tick_gain();
                    v.pos += 1;
                    ended = v.pos >= v.clip.frames();
                }
            }
            if ended {
                self.on_end();
            }

            let mut i = 0;
            while i < self.fading.len() {
                let v = &mut self.fading[i];
                if v.pos < v.clip.frames() {
                    let (a, b) = v.frame();
                    l += a;
                    r += b;
                    v.tick_gain();
                    v.pos += 1;
                }
                if (v.at_target() && v.at_target == AtTarget::Kill) || v.pos >= v.clip.frames() {
                    let v = self.fading.swap_remove(i);
                    self.retire(v);
                } else {
                    i += 1;
                }
            }

            let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
            match frame {
                [m] => *m = T::from_sample((l + r) * 0.5),
                [a, b, rest @ ..] => {
                    *a = T::from_sample(l);
                    *b = T::from_sample(r);
                    for s in rest {
                        *s = T::EQUILIBRIUM;
                    }
                }
                [] => {}
            }
        }
        if let Some(v) = &self.current {
            self.status.pos.store(v.pos as u64, Relaxed);
        }
    }
}

/// UI-side handle on one output stream.
pub struct Engine {
    _stream: cpal::Stream,
    tx: rtrb::Producer<Cmd>,
    garbage: rtrb::Consumer<Garbage>,
    pub status: Arc<Status>,
    pub rate: u32,
    pub device_name: String,
}

pub fn output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect())
        .unwrap_or_default()
}

impl Engine {
    /// `device`: a name from `output_device_names()`, or None / unknown for the system default.
    pub fn open(device: Option<&str>) -> Result<Engine, String> {
        let host = cpal::default_host();
        let wanted = device.and_then(|name| {
            host.output_devices()
                .ok()?
                .find(|d| d.description().map(|x| x.name() == name).unwrap_or(false))
        });
        let dev = wanted.or_else(|| host.default_output_device()).ok_or("no audio output device found")?;
        let device_name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "Default".into());
        let supported = dev.default_output_config().map_err(|e| format!("{device_name}: {e}"))?;
        let rate = supported.sample_rate();
        let format = supported.sample_format();
        let base: cpal::StreamConfig = supported.into();

        // Small buffer = fast GO. Fall back to the driver default if the device refuses it.
        let mut last_err = String::new();
        for buffer in [cpal::BufferSize::Fixed(512), cpal::BufferSize::Default] {
            let config = cpal::StreamConfig { buffer_size: buffer, ..base.clone() };
            let (tx, rx) = rtrb::RingBuffer::new(256);
            let (gtx, grx) = rtrb::RingBuffer::new(1024);
            let status = Arc::new(Status::new());
            let mixer = Mixer::new(rate, rx, gtx, status.clone());
            let built = match format {
                cpal::SampleFormat::F32 => build::<f32>(&dev, &config, mixer, status.clone()),
                cpal::SampleFormat::I16 => build::<i16>(&dev, &config, mixer, status.clone()),
                cpal::SampleFormat::I32 => build::<i32>(&dev, &config, mixer, status.clone()),
                cpal::SampleFormat::U16 => build::<u16>(&dev, &config, mixer, status.clone()),
                cpal::SampleFormat::F64 => build::<f64>(&dev, &config, mixer, status.clone()),
                other => return Err(format!("{device_name}: unsupported sample format {other}")),
            };
            match built.and_then(|s| s.play().map(|_| s).map_err(|e| e.to_string())) {
                Ok(stream) => {
                    return Ok(Engine { _stream: stream, tx, garbage: grx, status, rate, device_name });
                }
                Err(e) => last_err = e,
            }
        }
        Err(format!("{device_name}: {last_err}"))
    }

    pub fn send(&mut self, cmd: Cmd) {
        // 256 slots drained every few ms: only full if the audio thread is dead,
        // which `stream_error` already reports.
        let _ = self.tx.push(cmd);
    }

    /// Free what the audio thread is done with. Call often from the UI thread.
    pub fn collect_garbage(&mut self) {
        while self.garbage.pop().is_ok() {}
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    dev: &cpal::Device,
    config: &cpal::StreamConfig,
    mut mixer: Mixer,
    status: Arc<Status>,
) -> Result<cpal::Stream, String> {
    let channels = config.channels as usize;
    dev.build_output_stream(
        config.clone(),
        move |out: &mut [T], _| mixer.render(out, channels),
        move |err| {
            // Xruns are survivable; a vanished or changed device is not.
            if !matches!(err.kind(), cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied) {
                status.stream_error.store(true, Relaxed);
            }
        },
        None,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000; // declick = 5 frames

    fn clip(frames: usize, value: i16) -> Arc<Clip> {
        Arc::new(Clip { data: vec![value; frames * 2] })
    }

    fn rig() -> (Mixer, rtrb::Producer<Cmd>, rtrb::Consumer<Garbage>, Arc<Status>) {
        let (tx, rx) = rtrb::RingBuffer::new(64);
        let (gtx, grx) = rtrb::RingBuffer::new(64);
        let status = Arc::new(Status::new());
        (Mixer::new(RATE, rx, gtx, status.clone()), tx, grx, status)
    }

    fn run(m: &mut Mixer, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames * 2];
        m.render(&mut out, 2);
        out.iter().step_by(2).copied().collect()
    }

    fn plan(clips: &[(CueId, Arc<Clip>)], next: &[(CueId, CueId)], loop_cue: bool) -> Arc<Plan> {
        let mut p = Plan::empty();
        for (c, k) in clips {
            p.clips[*c] = Some(k.clone());
        }
        for (a, b) in next {
            p.next[*a] = Some(*b);
        }
        p.loop_cue = loop_cue;
        Arc::new(p)
    }

    #[test]
    fn fire_plays_immediately_and_stops_at_end() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(200, 16384))], &[], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 300);
        assert!(out[0] >= 0.0 && out[0] < 0.01, "short fade-in, no click");
        assert!((out[100] - 0.5).abs() < 1e-3, "full level after the ramp");
        assert_eq!(out[250], 0.0, "silent after the end");
        assert_eq!(st.snapshot().cue, None);
        assert_eq!(st.starts.load(Relaxed), 1);
    }

    #[test]
    fn linked_cue_follows_gaplessly() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(100, 8192)), (1, clip(100, 16384))], &[(0, 1)], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 150);
        assert!((out[99] - 0.25).abs() < 1e-3);
        assert!((out[100] - 0.5).abs() < 1e-3, "follower at full level on the next sample");
        assert_eq!(st.snapshot().cue, Some(1));
        assert_eq!(st.starts.load(Relaxed), 2);
    }

    #[test]
    fn loop_cue_repeats() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(100, 8192))], &[], true))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 350);
        assert!((out[320] - 0.25).abs() < 1e-3);
        assert_eq!(st.snapshot().cue, Some(0));
    }

    #[test]
    fn stop_fades_then_second_stop_cuts() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(10_000, 16384))], &[], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 100);
        tx.push(Cmd::Stop { fade_frames: 1000 }).ok();
        let out = run(&mut m, 500);
        assert!(out[250] < 0.5 && out[250] > 0.3, "fading: {}", out[250]);
        assert_eq!(st.snapshot().cue, None);
        tx.push(Cmd::Stop { fade_frames: 1000 }).ok();
        let out = run(&mut m, 20);
        assert_eq!(out[10], 0.0, "panic stop cuts within 5 ms");
    }

    #[test]
    fn new_cue_replaces_old_with_declick() {
        let (mut m, mut tx, mut g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(1000, 8192)), (1, clip(1000, 16384))], &[], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 100);
        tx.push(Cmd::Fire(1)).ok();
        let out = run(&mut m, 100);
        assert!((out[50] - 0.5).abs() < 1e-3, "old cue gone after the declick");
        assert_eq!(st.snapshot().cue, Some(1));
        // Old voice and old plan are freed on the UI side, not in the callback.
        let mut freed = 0;
        while g.pop().is_ok() {
            freed += 1;
        }
        assert!(freed >= 2);
    }

    #[test]
    fn many_long_stops_never_grow_the_fading_list() {
        let (mut m, mut tx, mut g, _st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(100_000, 16384))], &[], false))).ok();
        for _ in 0..20 {
            tx.push(Cmd::Fire(0)).ok();
            run(&mut m, 5); // audible before the stop, so it really has to fade
            tx.push(Cmd::Stop { fade_frames: 50_000 }).ok();
            run(&mut m, 1);
            while g.pop().is_ok() {}
        }
        assert_eq!(m.fading.len(), MAX_FADING);
        assert_eq!(m.fading.capacity(), MAX_FADING, "no reallocation in the callback");
    }

    #[test]
    fn pause_freezes_position_and_resumes() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(0, clip(1000, 16384))], &[], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 100);
        tx.push(Cmd::TogglePause).ok();
        run(&mut m, 20);
        let pos = st.pos.load(Relaxed);
        let out = run(&mut m, 100);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(st.pos.load(Relaxed), pos);
        assert!(st.snapshot().paused);
        tx.push(Cmd::TogglePause).ok();
        let out = run(&mut m, 50);
        assert!((out[40] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn armed_autostart_fires() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(&[(3, clip(1000, 16384))], &[], false))).ok();
        tx.push(Cmd::Arm(Some((SystemTime::now(), 3)))).ok();
        run(&mut m, 10);
        assert_eq!(st.snapshot().cue, Some(3));
        assert_eq!(st.autostarted.load(Relaxed), 1);
    }
}
