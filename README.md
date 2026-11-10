# Cue Spark

<img src="assets/icon.png" width="96" align="right" alt="Cue Spark logo">

Instant playback of show sound cues for theatre and live events. It follows the
workflow of CueFire FREE (discontinued, does not run on Windows 11):
two modes, 5 banks of 10 cues on F1–F10, automatic preselection, linked cues.

Windows 11 first, Linux too. English or French interface (Preferences → Language).

## Download

[Latest release](https://github.com/Kirato77/cuespark/releases/latest):

- **Windows**: `CueSpark.exe`, one self-contained file, no installer. Windows SmartScreen may warn the
  first time because the exe is not signed: *More info → Run anyway*.
- **Linux** (x86_64): `cuespark-linux-x86_64`, make it executable. For a menu entry, copy
  `packaging/cuespark.desktop` to `~/.local/share/applications/` and `assets/icon.png` to
  `~/.local/share/icons/hicolor/256x256/apps/cuespark.png`, with the binary in your PATH as `cuespark`.

## Updates

At startup the app checks GitHub for a newer release (Preferences → *Check for updates at startup*,
or Help → *Check for updates*). An **Update to x.y.z** button then appears in the Edit Mode toolbar:
it downloads the new version, checks its size and SHA-256, replaces the program file, and offers a
restart that reopens the current show. Nothing ever appears in Show Mode, and updating is refused
while a cue plays or the show has unsaved changes. The version is shown in the title bar, the
status bar (Edit Mode) and Help → About.

## Why it's instant

Every clip is decoded into RAM at load time (and resampled to the sound card's rate), so firing
a cue never touches the disk or a decoder: the sound starts on the next audio buffer.
The audio callback never locks or allocates. Show-mode keys are taken from the raw OS events
before the UI runs, and a mouse click fires when the button goes down, not when it comes back up.
Measured on Linux/PipeWire: about 25 ms from key press to sound.

RAM use is about 10 MB per minute of audio (stereo 16-bit), so a 30-minute show uses about 300 MB.

## Modes

**Show Mode** is read-only, so nothing can be changed by mistake during a show:
- big clock (elapsed/remaining) that turns orange 10 s and red 5 s before the end, bank total,
  progress bar, NEXT banner with the cue that GO will fire;
- transport: Stop, Fade, Pause, previous/next preselection;
- 10 cue buttons per bank with link brackets and a progress strip on the playing cue;
- level meter, master volume, bank/playback switches, cue information (notes of the
  preselected cue).

While it's on, Windows can't sleep or blank the screen, and quitting asks for confirmation first.

**Edit Mode** (Configuration Matrix):
- banks: Link Cues, Link Bank, Loop Bank, name, **output device and stereo pair** (several sound
  cards at once), length;
- playback: automatic preselection (next/current), automatic start at a wall-clock time;
- cue table: link, name, file, length, size, type, reset; drag audio files onto it to fill
  cues from the row under the mouse;
- selected cue: volume (dB), fade in, fade out, *play over other cues*, in/out points dragged on
  the waveform, notes;
- undo/redo, collect all audio into the show folder, find missing files, export the cue list
  (printable page or CSV);
- MIDI remote: learn a pad, key or pedal for GO, Stop and Fade out (Preferences → MIDI remote);
- dark (default) or light theme.

## Keys

| Show Mode | |
|---|---|
| F1 … F10 | fire cue of the visible bank |
| Space / Enter | GO: fire the preselected cue |
| Esc | stop (with the stop fade from Preferences); press again to cut |
| F | fade out: the cue's own fade-out time, else the default from Preferences (3 s) |
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
| Ctrl+Z / Ctrl+Y | undo / redo |
| Ctrl+N / O / S, Ctrl+Shift+S | new / open / save / save as |

Ctrl+E switches modes.

## Linking rules

A cue starts the next one when it ends if the bank's **Link Cues** is on *and* the cue's
**Link** box is ticked. At the end of a bank, **Loop Bank** goes back to the bank's first cue,
**Link Bank** continues with the next bank. **Loop Cue** repeats the playing cue. Linked cues
follow on the very next sample, with no gap (on the same output device; across devices the
UI starts the follower within a frame).

A new cue replaces what is playing (a 5 ms anti-click fade), on every device, unless it is set to
*play over other cues*.

## Show files

`.cuespark` is JSON. Audio paths are saved relative to the show file when the audio lives in
the same folder or below it, so a show folder can be copied to another machine. If a path is missing,
the file name is looked up next to the show file and one folder below.

Formats: WAV, AIFF, MP3, FLAC, OGG Vorbis, AAC/M4A, ALAC, CAF, MKA.

## Build

Toolchain via mise (`mise.toml`: Rust stable + Zig). The Windows exe gets its icon and version info
from `build.rs`, compiled with `zig rc`.

```sh
cargo test
cargo build --release                                            # Linux: target/release/cuespark
cargo zigbuild --release --target x86_64-pc-windows-gnu          # Windows: target/x86_64-pc-windows-gnu/release/cuespark.exe
```

One-time setup for the Windows build: `rustup target add x86_64-pc-windows-gnu` and `cargo install cargo-zigbuild`.
The Windows exe is self-contained (no installer, no runtime to install).

Linux needs ALSA headers (`alsa-lib`) to build; it plays through PipeWire/Pulse via ALSA.
The Linux binary is built against the build machine's glibc, so it needs a distro at least that recent.

## Publishing a release

1. Bump `version` in `Cargo.toml`, commit.
2. `./release.sh`: runs the tests, builds Linux and Windows, tags `vX.Y.Z`, pushes, and creates the
   GitHub release with both binaries. Installed copies offer the update on their next start.

Needs `gh` (logged in), `cargo-zigbuild` and `cargo-about` (`cargo install cargo-about --features cli`).
`THIRD_PARTY_LICENSES.txt` lists the licenses of every dependency; it is built into the app
(Help → About → Third-party licenses) and regenerated by the release script.

## Known limits

- WASAPI shared mode on Windows, no ASIO yet: the ASIO SDK is now GPLv3-compatible, but its
  Rust binding only builds with the MSVC toolchain, not with this cross-compiled MinGW build.
- Surround files: only front L/R are played.
- Screen keep-awake is Windows-only.
- The exe is not code-signed (SmartScreen warning on first run).

## License

GPL-3.0-or-later, see `LICENSE`. Dependencies keep their own licenses (`THIRD_PARTY_LICENSES.txt`).

Cue Spark is an independent project. It is not affiliated with or endorsed by audiobytes, the
makers of CueFire; no code or assets from CueFire are used.
