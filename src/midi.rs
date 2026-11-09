//! MIDI remote: a controller or MIDI foot switch triggers GO / Stop / Fade.

use midir::MidiInput;
use std::sync::mpsc;

/// A learned trigger: message kind (0x90 note-on, 0xB0 control change) and note/controller number.
pub type Trigger = [u8; 2];

pub const ACTIONS: [&str; 3] = ["GO", "Stop", "Fade out"];

pub fn port_names() -> Vec<String> {
    MidiInput::new("cuespark").map(|m| m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect()).unwrap_or_default()
}

pub struct Midi {
    _conn: midir::MidiInputConnection<()>,
    pub rx: mpsc::Receiver<Trigger>,
}

pub fn connect(name: &str, ctx: eframe::egui::Context) -> Result<Midi, String> {
    let input = MidiInput::new("cuespark").map_err(|e| e.to_string())?;
    let port = input
        .ports()
        .into_iter()
        .find(|p| input.port_name(p).ok().as_deref() == Some(name))
        .ok_or_else(|| format!("MIDI device {name} not found"))?;
    let (tx, rx) = mpsc::channel();
    let conn = input
        .connect(
            &port,
            "cuespark-in",
            move |_, msg, _| {
                if let Some(t) = trigger(msg) {
                    let _ = tx.send(t);
                    ctx.request_repaint(); // wakes the UI thread, which fires the cue
                }
            },
            (),
        )
        .map_err(|e| e.to_string())?;
    Ok(Midi { _conn: conn, rx })
}

/// A note pressed or a controller pushed (value > 0), on any channel.
fn trigger(msg: &[u8]) -> Option<Trigger> {
    match msg {
        [status, number, value, ..] if *value > 0 && matches!(status & 0xF0, 0x90 | 0xB0) => Some([status & 0xF0, *number]),
        _ => None,
    }
}

pub fn describe(t: Trigger) -> String {
    if t[0] == 0x90 { format!("Note {}", t[1]) } else { format!("CC {}", t[1]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real ALSA round trip through the kernel's "Midi Through" port: `cargo test -- --ignored`
    #[test]
    #[ignore]
    #[cfg(target_os = "linux")]
    fn live_midi_through() {
        let name = port_names().into_iter().find(|n| n.contains("Midi Through")).expect("Midi Through port");
        let midi = connect(&name, eframe::egui::Context::default()).unwrap();
        let ok = std::process::Command::new("aseqsend").args(["-p", "14:0", "93", "3c", "64"]).status().unwrap();
        assert!(ok.success());
        let got = midi.rx.recv_timeout(std::time::Duration::from_secs(2)).expect("trigger received");
        assert_eq!(got, [0x90, 60]);
    }

    #[test]
    fn triggers() {
        assert_eq!(trigger(&[0x93, 60, 100]), Some([0x90, 60]), "note on, any channel");
        assert_eq!(trigger(&[0x90, 60, 0]), None, "velocity 0 is a note off");
        assert_eq!(trigger(&[0x80, 60, 64]), None, "note off");
        assert_eq!(trigger(&[0xB0, 64, 127]), Some([0xB0, 64]), "sustain pedal down");
        assert_eq!(trigger(&[0xB0, 64, 0]), None, "pedal up");
        assert_eq!(trigger(&[0xF8]), None, "clock");
    }
}
