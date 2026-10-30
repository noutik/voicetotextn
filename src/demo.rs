//! Renders the demo animation for the README: the overlay appears, listens to
//! a piece of a recording, transcribes and flies away. Frames are written as
//! PPM images on a neutral background; ffmpeg turns them into a GIF.

use std::io::Write;
use std::path::Path;

use crate::audio::RATE;
use crate::scene::{self, Mode, Scene};
use crate::spectrum::FFT_LEN;

const FPS: f32 = 50.0;
const BACKGROUND: [f32; 3] = [38.0, 38.0, 42.0];
const MARGIN_X: usize = 60;
const MARGIN_BOTTOM: usize = 12;
const LOADING: f32 = 0.9;
const TRANSCRIBING: f32 = 1.6;
/// Slide-out plus a moment of empty screen before the GIF loops.
const LEAVING: f32 = 0.6;

/// Uses `seconds` of `wav` starting at `start` for the listening part.
pub fn render(wav: &Path, start: f32, seconds: f32, out_dir: &Path) -> Result<usize, String> {
    let audio = crate::wav::read(wav)?;
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let first = (start * RATE as f32) as usize;
    let (w, h) = (scene::WIDTH as usize + 2 * MARGIN_X, scene::HEIGHT as usize + MARGIN_BOTTOM);
    let dt = 1.0 / FPS;
    let frames = ((LOADING + seconds + TRANSCRIBING + LEAVING) * FPS) as usize;

    let mut scene = Scene::new();
    let mut window = vec![0.0; FFT_LEN];
    let mut ppm = Vec::with_capacity(w * h * 3 + 32);
    for n in 0..frames {
        let t = n as f32 * dt;
        let mode = if t < LOADING {
            Mode::Loading
        } else if t < LOADING + seconds {
            Mode::Listening
        } else if t < LOADING + seconds + TRANSCRIBING {
            Mode::Transcribing
        } else {
            Mode::Leaving(t - LOADING - seconds - TRANSCRIBING)
        };
        if mode == Mode::Listening {
            let end = first + ((t - LOADING) * RATE as f32) as usize;
            for (i, s) in window.iter_mut().enumerate() {
                *s = (end + i).checked_sub(FFT_LEN).and_then(|j| audio.get(j)).copied().unwrap_or(0.0);
            }
            scene.listen(&window, dt);
        }
        let visible = scene.draw(mode, t, dt);

        ppm.clear();
        write!(ppm, "P6\n{w} {h}\n255\n").unwrap();
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = (x.wrapping_sub(MARGIN_X), y);
                let px = if visible && sx < scene::WIDTH as usize && sy < scene::HEIGHT as usize {
                    scene.canvas.px[sy * scene.canvas.w + sx]
                } else {
                    0
                };
                // Premultiplied source over the background.
                let keep = 1.0 - (px >> 24) as f32 / 255.0;
                for (c, shift) in [16, 8, 0].into_iter().enumerate() {
                    let src = ((px >> shift) & 0xff) as f32;
                    ppm.push((src + BACKGROUND[c] * keep).round().min(255.0) as u8);
                }
            }
        }
        std::fs::write(out_dir.join(format!("{n:04}.ppm")), &ppm).map_err(|e| e.to_string())?;
    }
    Ok(frames)
}
