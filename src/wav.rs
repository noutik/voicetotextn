//! Minimal 16-bit PCM WAV reading and writing at 16 kHz, for saved recordings and benchmarks.

use std::io::{self, Write};
use std::path::Path;

use crate::audio::RATE;

pub fn write(path: &Path, audio: &[f32]) -> io::Result<()> {
    let data_len = (audio.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in audio {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::File::create(path)?.write_all(&out)
}

/// Reads a 16 kHz 16-bit PCM WAV; stereo is mixed down to mono.
pub fn read(path: &Path) -> Result<Vec<f32>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut channels, mut rate, mut bits) = (0u16, 0u32, 0u16);
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            if rate != RATE || bits != 16 || channels == 0 {
                return Err(format!("need 16 kHz 16-bit PCM, got {rate} Hz {bits}-bit (convert with ffmpeg -ar 16000)"));
            }
            let frames = body.chunks_exact(2 * channels as usize);
            return Ok(frames
                .map(|f| {
                    let sum: f32 = f.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32).sum();
                    sum / channels as f32 / 32768.0
                })
                .collect());
        }
        pos += 8 + len + (len & 1);
    }
    Err("no data chunk".into())
}
