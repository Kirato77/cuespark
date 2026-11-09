# Cue Spark

Instant playback of show sound cues for theatre and live events. It follows the
workflow of CueFire FREE (audiobytes, discontinued, does not run on Windows 11):
two modes, 5 banks of 10 cues on F1–F10, automatic preselection, linked cues.

Windows 11 first, Linux too.

## Why it's instant

Every clip is decoded into RAM at load time (and resampled to the sound card's rate), so firing
a cue never touches the disk or a decoder: the sound starts on the next audio buffer.
The audio callback never locks or allocates. Show-mode keys are taken from the raw OS events
before the UI runs, and a mouse click fires when the button goes down, not when it comes back up.
Measured on Linux/PipeWire: about 25 ms from key press to sound.

RAM use is about 10 MB per minute of audio (stereo 16-bit), so a 30-minute show uses about 300 MB.

## Modes

**Show Mode**: big clock (elapsed/remaining), bank total, progress bar, transport,
10 cue buttons per bank with link brackets, bank/playback switches, cue information
(notes of the preselected cue). While it's on, Windows can't sleep or blank the screen, and
quitting asks for confirmation first.

**Edit Mode** (Configuration Matrix): bank settings (Link Cues, Link Bank, Loop Bank, name,
length), playback settings (automatic preselection next/current, automatic start at a
wall-clock time), cue table (link, name, file, length, size, type, reset), notes, move up/down.
Drag audio files onto the table to fill cues from the row under the mouse.

## Keys

| Show Mode | |
|---|---|
| F1 … F10 | fire cue of the visible bank |
| Space / Enter | GO: fire the preselected cue |
| Esc | stop (with the stop fade from Preferences); press again to cut |
| P | pause / resume |
| Up / Down | preselect previous / next cue |
| Left / Right, 1 … 5 | change visible bank |
| Tab | elapsed / remaining |
| Right-click a cue | preselect it |

| Edit Mode | |
|---|---|
| Enter / double-click | preview the selected cue |
| Del | clear the selected cue |
| Alt+Up / Alt+Down | move the selected cue |
| Ctrl+N / O / S, Ctrl+Shift+S | new / open / save / save as |

Ctrl+E switches modes.

## Linking rules

A cue starts the next one when it ends if the bank's **Link Cues** is on *and* the cue's
**Link** box is ticked. At the end of a bank, **Loop Bank** goes back to the bank's first cue,
**Link Bank** continues with the next bank. **Loop Cue** repeats the playing cue. Linked cues
follow on the very next sample, with no gap.

## Show files

`.cuespark` is JSON. Audio paths are saved relative to the show file when the audio lives in
the same folder or below it, so a show folder can be copied to another machine. If a path is missing,
the file name is looked up next to the show file and one folder below.

Formats: WAV, AIFF, MP3, FLAC, OGG Vorbis, AAC/M4A, ALAC, CAF, MKA.

## Build

Toolchain via mise (`mise.toml`: Rust stable + Zig).

```sh
cargo test
cargo build --release                                            # Linux: target/release/cuespark
cargo zigbuild --release --target x86_64-pc-windows-gnu          # Windows: target/x86_64-pc-windows-gnu/release/cuespark.exe
```

One-time setup for the Windows build: `rustup target add x86_64-pc-windows-gnu` and `cargo install cargo-zigbuild`.
The Windows exe is self-contained (no installer, no runtime to install).

Linux needs ALSA headers (`alsa-lib`) to build; it plays through PipeWire/Pulse via ALSA.

## Known limits

- One output device for the whole show (Preferences → Audio output), no per-bank output.
- No ASIO; WASAPI shared mode on Windows.
- No per-cue volume or fade-in; Stop can fade out.
- Surround files: only front L/R are played.
- Screen keep-awake is Windows-only.
