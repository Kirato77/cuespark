//! Real-time playback. The audio callback never locks, allocates or frees:
//! commands arrive through a lock-free ring, state goes out through atomics,
//! and anything that must be freed is shipped back to the UI thread.

use crate::decode::Clip;
use crate::project::{CUE_COUNT, CueId};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::SystemTime;

pub const NONE: usize = usize::MAX;
const MAX_VOICES: usize = 16;
const MAX_CHANNELS: usize = 64;

/// One playable cue, already converted to this device's frames.
pub struct PlanCue {
    pub clip: Arc<Clip>,
    pub gain: f32,
    pub fade_in: u32,
    pub fade_out: u32,
    /// Play range in frames, `start < end <= clip.frames()`.
    pub start: usize,
    pub end: usize,
    /// Keep the cues already playing instead of replacing them.
    pub layer: bool,
    /// First output channel of the stereo pair.
    pub channel: usize,
    /// Cue that follows when this one ends (may live on another device).
    pub next: Option<CueId>,
}

/// What the audio thread needs to know about the show, rebuilt by the UI on every change.
/// Holds only the cues routed to this engine's device.
pub struct Plan {
    pub cues: Vec<Option<PlanCue>>,
    pub loop_cue: bool,
}

impl Plan {
    pub fn empty() -> Self {
        Self { cues: (0..CUE_COUNT).map(|_| None).collect(), loop_cue: false }
    }
}

pub enum Cmd {
    Fire(CueId),
    /// Fade out everything playing. Sent while fades are running, it cuts at once.
    Stop { fade_frames: u32 },
    /// Fade out everything playing, each cue with its own fade-out time.
    FadeOut,
    /// Silence this device now (a cue that replaces everything started on another device).
    Cut,
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
    /// Most recently started cue still playing.
    pub cue: AtomicUsize,
    pub pos: AtomicU64,
    pub len: AtomicU64,
    /// 0 stopped, 1 playing, 2 paused
    pub state: AtomicU8,
    /// Bumped every time a cue starts (manual, linked or automatic).
    pub starts: AtomicU64,
    pub autostarted: AtomicU64,
    pub stream_error: AtomicBool,
    /// A linked follower that lives on another device; the UI fires it.
    pub follow: AtomicUsize,
    /// Master volume, f32 bits.
    pub master: AtomicU32,
    /// Peak level since the UI last read it, f32 bits (left/even and right/odd channels).
    pub peak_l: AtomicU32,
    pub peak_r: AtomicU32,
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
        s.follow.store(NONE, Relaxed);
        s.master.store(1.0f32.to_bits(), Relaxed);
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

    /// Peak (left, right) since the previous call.
    pub fn take_peaks(&self) -> (f32, f32) {
        (f32::from_bits(self.peak_l.swap(0, Relaxed)), f32::from_bits(self.peak_r.swap(0, Relaxed)))
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
    start: usize,
    end: usize,
    level: f32,
    channel: usize,
    fade_out: u32,
    gain: f32,
    target: f32,
    step: f32,
    at_target: AtTarget,
}

impl Voice {
    fn new(cue: CueId, pc: &PlanCue, fade_in: u32) -> Self {
        let gain = if fade_in == 0 { 1.0 } else { 0.0 };
        let mut v = Voice {
            cue,
            clip: pc.clip.clone(),
            pos: pc.start,
            start: pc.start,
            end: pc.end.min(pc.clip.frames()),
            level: pc.gain,
            channel: pc.channel,
            fade_out: pc.fade_out,
            gain,
            target: 1.0,
            step: 1.0,
            at_target: AtTarget::Hold,
        };
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

    fn frozen(&self) -> bool {
        self.at_target == AtTarget::Pause && self.at_target()
    }

    fn tick_gain(&mut self) {
        if self.gain < self.target {
            self.gain = (self.gain + self.step).min(self.target);
        } else if self.gain > self.target {
            self.gain = (self.gain - self.step).max(self.target);
        }
    }

    /// Next stereo frame, advancing the position.
    fn next(&mut self) -> (f32, f32) {
        let i = self.pos * 2;
        let k = self.gain * self.level / 32768.0;
        let out = (self.clip.data[i] as f32 * k, self.clip.data[i + 1] as f32 * k);
        self.tick_gain();
        self.pos += 1;
        out
    }
}

pub struct Mixer {
    plan: Arc<Plan>,
    playing: Vec<Voice>,
    fading: Vec<Voice>,
    paused: bool,
    armed: Option<(SystemTime, CueId)>,
    declick: u32,
    fade_in: u32,
    master: f32,
    cmds: rtrb::Consumer<Cmd>,
    garbage: rtrb::Producer<Garbage>,
    status: Arc<Status>,
}

impl Mixer {
    fn new(rate: u32, cmds: rtrb::Consumer<Cmd>, garbage: rtrb::Producer<Garbage>, status: Arc<Status>) -> Self {
        Self {
            plan: Arc::new(Plan::empty()),
            playing: Vec::with_capacity(MAX_VOICES),
            fading: Vec::with_capacity(MAX_VOICES),
            paused: false,
            armed: None,
            declick: rate / 200,           // 5 ms: inaudible as a fade, removes the click of a hard cut
            fade_in: (rate / 1000).max(1), // 1 ms: keeps transients, avoids a click on non-zero starts
            master: 1.0,
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
    fn push_fading(&mut self, mut v: Voice, frames: u32) {
        v.ramp(0.0, frames.max(self.declick), AtTarget::Kill);
        if self.fading.len() == MAX_VOICES {
            let oldest = self.fading.remove(0);
            self.retire(oldest);
        }
        self.fading.push(v);
    }

    fn fade_all(&mut self, frames: impl Fn(&Voice) -> u32) {
        while let Some(v) = self.playing.pop() {
            // A paused voice is already silent: drop it at once.
            let f = if v.frozen() { 1 } else { frames(&v) };
            self.push_fading(v, f);
        }
        self.paused = false;
        self.publish();
    }

    fn start(&mut self, cue: CueId, manual_fade_in: bool) {
        let plan = self.plan.clone();
        let Some(pc) = plan.cues.get(cue).and_then(|c| c.as_ref()) else { return };
        if !pc.layer {
            let d = self.declick;
            self.fade_all(|_| d);
        } else if self.paused {
            self.toggle_pause(); // a layered cue resumes what it plays over
        }
        if self.playing.len() == MAX_VOICES {
            let oldest = self.playing.remove(0);
            let d = self.declick;
            self.push_fading(oldest, d);
        }
        // Linked followers start at full level (gapless) unless the cue asks for a fade-in.
        let fade_in = if pc.fade_in > 0 { pc.fade_in } else if manual_fade_in { self.fade_in } else { 0 };
        self.playing.push(Voice::new(cue, pc, fade_in));
        self.status.starts.fetch_add(1, Relaxed);
        self.publish();
    }

    fn toggle_pause(&mut self) {
        let d = self.declick;
        self.paused = !self.paused && !self.playing.is_empty();
        for v in &mut self.playing {
            if self.paused {
                v.ramp(0.0, d, AtTarget::Pause);
            } else {
                v.ramp(1.0, d, AtTarget::Hold);
            }
        }
        self.publish();
    }

    /// Mirror the newest playing voice into the status the UI reads.
    fn publish(&self) {
        match self.playing.last() {
            Some(v) => {
                self.status.cue.store(v.cue, Relaxed);
                self.status.len.store((v.end - v.start) as u64, Relaxed);
                self.status.pos.store((v.pos - v.start) as u64, Relaxed);
                self.status.state.store(if self.paused { PAUSED } else { PLAYING }, Relaxed);
            }
            None => {
                self.status.cue.store(NONE, Relaxed);
                self.status.state.store(STOPPED, Relaxed);
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Fire(cue) => self.start(cue, true),
            Cmd::Stop { fade_frames } => {
                if self.playing.is_empty() {
                    let d = self.declick;
                    for v in &mut self.fading {
                        v.ramp(0.0, d, AtTarget::Kill);
                    }
                } else {
                    self.fade_all(|_| fade_frames);
                }
            }
            Cmd::FadeOut => self.fade_all(|v| v.fade_out),
            Cmd::Cut => {
                let d = self.declick;
                self.fade_all(|_| d);
            }
            Cmd::TogglePause => self.toggle_pause(),
            Cmd::SetPlan(plan) => {
                let old = std::mem::replace(&mut self.plan, plan);
                self.trash(Garbage::Plan(old));
            }
            Cmd::Arm(a) => self.armed = a,
        }
    }

    /// Voice `i` reached its end point: loop it, follow its link, or drop it.
    fn on_end(&mut self, i: usize) {
        if self.plan.loop_cue {
            let v = &mut self.playing[i];
            v.pos = v.start;
            return;
        }
        let v = self.playing.remove(i);
        let next = self.plan.cues.get(v.cue).and_then(|c| c.as_ref()).and_then(|c| c.next);
        self.retire(v);
        match next {
            Some(n) if self.plan.cues.get(n).is_some_and(|c| c.is_some()) => self.start(n, false),
            Some(n) => {
                self.status.follow.store(n, Relaxed); // on another device
                self.publish();
            }
            None => self.publish(),
        }
    }

    pub fn render<T: Sample + FromSample<f32>>(&mut self, out: &mut [T], channels: usize) {
        while let Ok(cmd) = self.cmds.pop() {
            self.handle(cmd);
        }
        if let Some((at, cue)) = self.armed
            && SystemTime::now() >= at
        {
            // Wall clock, not a monotonic one: survives suspend and NTP corrections.
            self.armed = None;
            self.start(cue, true);
            self.status.autostarted.fetch_add(1, Relaxed);
        }
        let master_target = f32::from_bits(self.status.master.load(Relaxed));
        let master_step = 1.0 / 2048.0;
        let width = channels.min(MAX_CHANNELS);
        let (mut peak_l, mut peak_r) = (0.0f32, 0.0f32);

        for frame in out.chunks_mut(channels) {
            let mut acc = [0.0f32; MAX_CHANNELS];
            let mix = |v: &mut Voice, acc: &mut [f32; MAX_CHANNELS]| {
                let (l, r) = v.next();
                if width == 1 {
                    acc[0] += (l + r) * 0.5;
                } else {
                    let c = if v.channel + 1 < width { v.channel } else { 0 };
                    acc[c] += l;
                    acc[c + 1] += r;
                }
            };

            // Ends are handled after mixing, so a follower starts on the next frame, not this one.
            let mut ended = [0usize; MAX_VOICES];
            let mut n_ended = 0;
            for (i, v) in self.playing.iter_mut().enumerate() {
                if !v.frozen() {
                    mix(v, &mut acc);
                    if v.pos >= v.end {
                        ended[n_ended] = i;
                        n_ended += 1;
                    }
                }
            }
            // Highest index first: removing it keeps the lower indices valid. A follower that
            // replaces everything may already have cleared the list, so re-check each one.
            for &i in ended[..n_ended].iter().rev() {
                if self.playing.get(i).is_some_and(|v| v.pos >= v.end) {
                    self.on_end(i);
                }
            }
            let mut i = 0;
            while i < self.fading.len() {
                let v = &mut self.fading[i];
                if v.pos < v.end {
                    mix(v, &mut acc);
                }
                if (v.at_target() && v.at_target == AtTarget::Kill) || v.pos >= v.end {
                    let v = self.fading.swap_remove(i);
                    self.retire(v);
                } else {
                    i += 1;
                }
            }

            // Smoothed master: no zipper noise when the fader moves.
            self.master += (master_target - self.master).clamp(-master_step, master_step);
            for (c, s) in frame.iter_mut().enumerate() {
                let v = if c < width { (acc[c] * self.master).clamp(-1.0, 1.0) } else { 0.0 };
                if c % 2 == 0 { peak_l = peak_l.max(v.abs()) } else { peak_r = peak_r.max(v.abs()) }
                *s = T::from_sample(v);
            }
        }
        if width == 1 {
            peak_r = peak_l;
        }
        // Positive floats order like their bit patterns, so fetch_max keeps the loudest.
        self.status.peak_l.fetch_max(peak_l.to_bits(), Relaxed);
        self.status.peak_r.fetch_max(peak_r.to_bits(), Relaxed);
        if let Some(v) = self.playing.last() {
            self.status.pos.store((v.pos - v.start) as u64, Relaxed);
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
    pub channels: usize,
    pub device_name: String,
    /// Stable, unique id: two identical cards share a name, never an id.
    pub device_id: String,
}

/// (id, name) of every output device.
pub fn output_devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| Some((d.id().ok()?.to_string(), d.description().ok()?.name().to_string()))).collect())
        .unwrap_or_default()
}

impl Engine {
    /// `device`: an id from `output_devices()` (or a name, as older shows stored), or
    /// None / not found for the system default.
    pub fn open(device: Option<&str>) -> Result<Engine, String> {
        let host = cpal::default_host();
        let wanted = device.and_then(|key| {
            key.parse::<cpal::DeviceId>().ok().and_then(|id| host.device_by_id(&id)).or_else(|| {
                host.output_devices().ok()?.find(|d| d.description().map(|x| x.name() == key).unwrap_or(false))
            })
        });
        let dev = wanted.or_else(|| host.default_output_device()).ok_or("no audio output device found")?;
        let device_name = dev.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "Default".into());
        let device_id = dev.id().map(|i| i.to_string()).unwrap_or_default();
        let supported = dev.default_output_config().map_err(|e| format!("{device_name}: {e}"))?;
        let rate = supported.sample_rate();
        let format = supported.sample_format();
        let base: cpal::StreamConfig = supported.into();
        let channels = base.channels as usize;

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
                    return Ok(Engine { _stream: stream, tx, garbage: grx, status, rate, channels, device_name, device_id });
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

    const RATE: u32 = 1000; // declick = 5 frames, manual fade-in = 1 frame

    fn clip(frames: usize, value: i16) -> Arc<Clip> {
        Arc::new(Clip { data: vec![value; frames * 2], peaks: Vec::new() })
    }

    fn cue(c: Arc<Clip>) -> PlanCue {
        let end = c.frames();
        PlanCue { clip: c, gain: 1.0, fade_in: 0, fade_out: 0, start: 0, end, layer: false, channel: 0, next: None }
    }

    fn rig() -> (Mixer, rtrb::Producer<Cmd>, rtrb::Consumer<Garbage>, Arc<Status>) {
        let (tx, rx) = rtrb::RingBuffer::new(64);
        let (gtx, grx) = rtrb::RingBuffer::new(64);
        let status = Arc::new(Status::new());
        (Mixer::new(RATE, rx, gtx, status.clone()), tx, grx, status)
    }

    fn run_ch(m: &mut Mixer, frames: usize, channels: usize) -> Vec<Vec<f32>> {
        let mut out = vec![0.0f32; frames * channels];
        m.render(&mut out, channels);
        (0..channels).map(|c| out.iter().skip(c).step_by(channels).copied().collect()).collect()
    }

    fn run(m: &mut Mixer, frames: usize) -> Vec<f32> {
        run_ch(m, frames, 2).remove(0)
    }

    fn plan(cues: Vec<(CueId, PlanCue)>, loop_cue: bool) -> Arc<Plan> {
        let mut p = Plan::empty();
        for (c, pc) in cues {
            p.cues[c] = Some(pc);
        }
        p.loop_cue = loop_cue;
        Arc::new(p)
    }

    #[test]
    fn fire_plays_immediately_and_stops_at_end() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(200, 16384)))], false))).ok();
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
        let mut a = cue(clip(100, 8192));
        a.next = Some(1);
        tx.push(Cmd::SetPlan(plan(vec![(0, a), (1, cue(clip(100, 16384)))], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 150);
        assert!((out[99] - 0.25).abs() < 1e-3);
        assert!((out[100] - 0.5).abs() < 1e-3, "follower at full level on the next sample");
        assert_eq!(st.snapshot().cue, Some(1));
        assert_eq!(st.starts.load(Relaxed), 2);
    }

    #[test]
    fn follower_on_another_device_is_handed_to_the_ui() {
        let (mut m, mut tx, _g, st) = rig();
        let mut a = cue(clip(50, 8192));
        a.next = Some(7);
        tx.push(Cmd::SetPlan(plan(vec![(0, a)], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 60);
        assert_eq!(st.follow.load(Relaxed), 7);
        assert_eq!(st.snapshot().cue, None);
    }

    #[test]
    fn loop_cue_repeats() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(100, 8192)))], true))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 350);
        assert!((out[320] - 0.25).abs() < 1e-3);
        assert_eq!(st.snapshot().cue, Some(0));
    }

    #[test]
    fn stop_fades_then_second_stop_cuts() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(10_000, 16384)))], false))).ok();
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
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(1000, 8192))), (1, cue(clip(1000, 16384)))], false))).ok();
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
    fn layered_cue_plays_over_the_current_one() {
        let (mut m, mut tx, _g, st) = rig();
        let mut over = cue(clip(1000, 4096));
        over.layer = true;
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(1000, 8192))), (1, over)], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 50);
        tx.push(Cmd::Fire(1)).ok();
        let out = run(&mut m, 50);
        assert!((out[20] - 0.375).abs() < 1e-3, "both mixed: {}", out[20]);
        assert_eq!(st.snapshot().cue, Some(1), "display follows the newest cue");
    }

    #[test]
    fn two_cues_ending_together_with_a_replacing_follower() {
        let (mut m, mut tx, _g, st) = rig();
        let mut a = cue(clip(100, 4096));
        a.next = Some(2);
        let mut b = cue(clip(100, 4096));
        b.layer = true;
        tx.push(Cmd::SetPlan(plan(vec![(0, a), (1, b), (2, cue(clip(100, 16384)))], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        tx.push(Cmd::Fire(1)).ok();
        let out = run(&mut m, 150); // both end on frame 100; must not panic
        assert!((out[120] - 0.5).abs() < 1e-3);
        assert_eq!(st.snapshot().cue, Some(2));
    }

    #[test]
    fn cue_gain_fades_and_in_out_points() {
        let (mut m, mut tx, _g, st) = rig();
        let mut c = cue(clip(1000, 16384));
        c.gain = 0.5;
        c.fade_in = 100;
        c.fade_out = 200;
        c.start = 300;
        c.end = 700;
        tx.push(Cmd::SetPlan(plan(vec![(0, c)], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let out = run(&mut m, 150);
        assert!((out[50] - 0.125).abs() < 0.01, "half way through the fade-in: {}", out[50]);
        assert!((out[120] - 0.25).abs() < 1e-3, "gain 0.5 of 0.5");
        assert_eq!(st.snapshot().len, 400, "length is the in/out range");
        tx.push(Cmd::FadeOut).ok();
        let out = run(&mut m, 300);
        assert!((out[100] - 0.125).abs() < 0.01, "own fade-out time: {}", out[100]);
        assert_eq!(out[250], 0.0);
        tx.push(Cmd::Fire(0)).ok();
        run(&mut m, 500);
        assert_eq!(st.snapshot().cue, None, "stops at the out point (400 frames)");
    }

    #[test]
    fn channel_pair_routing_and_master_meter() {
        let (mut m, mut tx, _g, st) = rig();
        let mut c = cue(clip(10_000, 16384));
        c.channel = 2;
        tx.push(Cmd::SetPlan(plan(vec![(0, c)], false))).ok();
        tx.push(Cmd::Fire(0)).ok();
        let ch = run_ch(&mut m, 100, 4);
        assert_eq!(ch[0][50], 0.0);
        assert!((ch[2][50] - 0.5).abs() < 1e-3 && (ch[3][50] - 0.5).abs() < 1e-3, "on outputs 3-4");
        let (l, r) = st.take_peaks();
        assert!((l - 0.5).abs() < 1e-3 && (r - 0.5).abs() < 1e-3);
        assert_eq!(st.take_peaks(), (0.0, 0.0), "reset after reading");
        st.master.store(0.0f32.to_bits(), Relaxed);
        let ch = run_ch(&mut m, 3000, 4);
        assert_eq!(ch[2][2999], 0.0, "master fader reaches silence smoothly");
    }

    #[test]
    fn many_long_stops_never_grow_the_fading_list() {
        let (mut m, mut tx, mut g, _st) = rig();
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(100_000, 16384)))], false))).ok();
        for _ in 0..40 {
            tx.push(Cmd::Fire(0)).ok();
            run(&mut m, 5); // audible before the stop, so it really has to fade
            tx.push(Cmd::Stop { fade_frames: 50_000 }).ok();
            run(&mut m, 1);
            while g.pop().is_ok() {}
        }
        assert_eq!(m.fading.len(), MAX_VOICES);
        assert_eq!(m.fading.capacity(), MAX_VOICES, "no reallocation in the callback");
    }

    #[test]
    fn pause_freezes_position_and_resumes() {
        let (mut m, mut tx, _g, st) = rig();
        tx.push(Cmd::SetPlan(plan(vec![(0, cue(clip(1000, 16384)))], false))).ok();
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
        tx.push(Cmd::SetPlan(plan(vec![(3, cue(clip(1000, 16384)))], false))).ok();
        tx.push(Cmd::Arm(Some((SystemTime::now(), 3)))).ok();
        run(&mut m, 10);
        assert_eq!(st.snapshot().cue, Some(3));
        assert_eq!(st.autostarted.load(Relaxed), 1);
    }
}
