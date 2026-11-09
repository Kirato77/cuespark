//! Decode a whole audio file into RAM at the output device rate, so firing a cue
//! never touches the disk or a decoder: playback starts on the next audio buffer.

use rubato::audioadapter_buffers::number_to_float::InterleavedNumbers;
use rubato::{Fft, FixedSync, Resampler};
use std::path::Path;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Stereo, interleaved, at the device sample rate.
/// i16 halves RAM versus f32; 16 bits is below what any show PA can reproduce.
pub struct Clip {
    pub data: Vec<i16>,
}

impl Clip {
    pub fn frames(&self) -> usize {
        self.data.len() / 2
    }
}

/// Peak memory stays near 12 bytes per stereo frame (i16 in + f32 out while resampling),
/// so long background tracks load without exhausting RAM.
pub fn decode_file(path: &Path, out_rate: u32) -> Result<Clip, String> {
    let (stereo, in_rate) = decode_to_stereo(path)?;
    if stereo.is_empty() {
        return Err("file contains no audio".into());
    }
    if in_rate == out_rate {
        return Ok(Clip { data: stereo });
    }
    let resampled = resample(&stereo, in_rate, out_rate)?;
    drop(stereo);
    Ok(Clip { data: resampled.into_iter().map(to_i16).collect() })
}

fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

/// Converted to i16 packet by packet: the whole file never exists as f32.
fn decode_to_stereo(path: &Path) -> Result<(Vec<i16>, u32), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("cannot open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("unsupported format: {e}"))?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("no audio codec parameters")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| format!("unsupported codec: {e}"))?;

    let mut out = Vec::new();
    let mut rate = 0;
    let mut buf: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // Truncated files end with an IO error: keep what was decoded.
            Err(SymError::IoError(_)) => break,
            Err(e) => return Err(format!("read error: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(a) => a,
            Err(SymError::DecodeError(_)) => continue, // skip a corrupt frame, like every player does
            Err(e) => return Err(format!("decode error: {e}")),
        };
        let channels = audio.spec().channels().count().max(1);
        rate = audio.spec().rate();
        buf.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut buf);
        out.reserve(buf.len() / channels * 2);
        for frame in buf.chunks_exact(channels) {
            let (l, r) = match channels {
                1 => (frame[0], frame[0]),
                2 => (frame[0], frame[1]),
                // More than 2 channels: front L/R only, no surround downmix.
                _ => (frame[0], frame[1]),
            };
            out.push(to_i16(l));
            out.push(to_i16(r));
        }
    }
    if rate == 0 {
        return Err("file contains no audio".into());
    }
    Ok((out, rate))
}

fn resample(stereo: &[i16], from: u32, to: u32) -> Result<Vec<f32>, String> {
    let frames = stereo.len() / 2;
    let input = InterleavedNumbers::<_, f32>::new(stereo, 2, frames).map_err(|e| e.to_string())?;
    let mut rs = Fft::<f32>::new(from as usize, to as usize, 1024, 2, FixedSync::Input).map_err(|e| e.to_string())?;
    let out = rs.process_all(&input, frames, None).map_err(|e| e.to_string())?;
    Ok(out.take_data())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Minimal 16-bit PCM WAV writer for tests.
    pub fn write_wav(path: &Path, rate: u32, channels: u16, samples: &[i16]) {
        let data_len = (samples.len() * 2) as u32;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        b.extend_from_slice(&(channels * 2).to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            b.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    fn sine(rate: u32, secs: f32, hz: f32) -> Vec<i16> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| ((i as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * 16000.0) as i16)
            .collect()
    }

    #[test]
    fn mono_44k_to_stereo_48k() {
        let path = std::env::temp_dir().join(format!("cuespark-dec-{}.wav", std::process::id()));
        write_wav(&path, 44100, 1, &sine(44100, 1.0, 1000.0));
        let clip = decode_file(&path, 48000).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!((clip.frames() as i64 - 48000).abs() < 50, "length kept: {}", clip.frames());
        // Same signal on both channels, amplitude preserved through the resampler.
        let mid = &clip.data[48000..48000 + 2000];
        assert!(mid.chunks(2).all(|f| f[0] == f[1]));
        let peak = mid.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!((15000..17500).contains(&peak), "peak {peak}");
        // 1 kHz -> 48 zero crossings per 24 ms... count over 0.5 s: ~1000 crossings.
        let left: Vec<i16> = clip.data.iter().step_by(2).copied().collect();
        let crossings = left[10000..34000].windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!((990..1010).contains(&crossings), "crossings {crossings}");
    }

    #[test]
    fn missing_and_garbage_files_error_cleanly() {
        assert!(decode_file(Path::new("/nonexistent/x.mp3"), 48000).is_err());
        let path = std::env::temp_dir().join(format!("cuespark-bad-{}.mp3", std::process::id()));
        std::fs::write(&path, b"definitely not audio").unwrap();
        assert!(decode_file(&path, 48000).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
