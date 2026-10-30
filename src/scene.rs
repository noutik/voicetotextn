//! What the overlay looks like at any moment: where the pill is, how wide it
//! is and how tall every bar is. Pure drawing without Wayland, so the same
//! code also renders the demo animation.

use crate::render::Canvas;
use crate::spectrum::Analyzer;

// Geometry in pixels (the output scale is 1).
const BARS: usize = 21;
const BAR_W: f32 = 4.0;
const BAR_GAP: f32 = 4.0;
const PAD_X: f32 = 18.0;
const PILL_W: f32 = BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP + 2.0 * PAD_X;
const PILL_H: f32 = 44.0;
const BAR_MAX: f32 = 26.0;
/// A silent bar is a round dot.
const DOT: f32 = BAR_W;
/// Resting distance between the pill and the top screen edge.
const SHOWN_Y: f32 = 40.0;
const HIDDEN_Y: f32 = -PILL_H - 2.0;
pub const WIDTH: u32 = PILL_W as u32;
/// Extra room below the pill for the overshoot of the slide animations.
pub const HEIGHT: u32 = (SHOWN_Y + PILL_H + 8.0) as u32;

/// The pill slides down as a circle, then unfolds to full width.
const SLIDE_IN: f32 = 0.30;
const UNFOLD: f32 = 0.28;
const SLIDE_OUT: f32 = 0.26;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// Recording, model still loading: a wave runs along the dots.
    Loading,
    /// Recording: the bars follow the microphone.
    Listening,
    /// The bars breathe as a slow wave.
    Transcribing,
    /// Seconds since the pill started flying up.
    Leaving(f32),
}

pub struct Scene {
    analyzer: Analyzer,
    levels: [f32; BARS],
    /// Current (height, brightness) of every bar, eased towards the targets.
    bars: [(f32, f32); BARS],
    /// Current pill width: starts as a circle, unfolds to `PILL_W`.
    width: f32,
    /// The full-width pill, drawn once and copied every frame.
    pill: Canvas,
    pub canvas: Canvas,
}

impl Scene {
    pub fn new() -> Scene {
        let mut pill = Canvas::new(WIDTH as usize, PILL_H as usize);
        draw_pill(&mut pill, 0.0, 0.0, PILL_W);
        Scene {
            analyzer: Analyzer::new(BARS),
            levels: [0.0; BARS],
            bars: [(DOT, 0.35); BARS],
            width: PILL_H,
            pill,
            canvas: Canvas::new(WIDTH as usize, HEIGHT as usize),
        }
    }

    /// Updates the bar levels from the newest `FFT_LEN` microphone samples;
    /// `dt` is the time since the previous call.
    pub fn listen(&mut self, samples: &[f32], dt: f32) {
        self.analyzer.analyze(samples, dt, &mut self.levels);
    }

    /// Draws the frame `t` seconds after the start, `dt` after the previous
    /// frame. Returns false once the pill has flown away.
    pub fn draw(&mut self, mode: Mode, t: f32, dt: f32) -> bool {
        let y = match mode {
            // Flies up at whatever width it has.
            Mode::Leaving(since) => {
                if since >= SLIDE_OUT {
                    return false;
                }
                SHOWN_Y + (HIDDEN_Y - SHOWN_Y) * ease_in_back(since / SLIDE_OUT)
            }
            _ => {
                let unfold = ease_out_cubic(((t - SLIDE_IN) / UNFOLD).clamp(0.0, 1.0));
                self.width = PILL_H + (PILL_W - PILL_H) * unfold;
                HIDDEN_Y + (SHOWN_Y - HIDDEN_Y) * ease_out_back((t / SLIDE_IN).min(1.0))
            }
        };
        self.update_bars(mode, t, dt);

        self.canvas.clear();
        let top = y.round() as i32;
        if self.width >= PILL_W {
            self.canvas.blit_rows(&self.pill, top);
        } else {
            draw_pill(&mut self.canvas, (PILL_W - self.width) / 2.0, top as f32, self.width);
        }
        let center_y = top as f32 + PILL_H / 2.0;
        // While the pill unfolds, the bars appear from the centre outwards.
        let reveal = self.width / 2.0 - (PAD_X + BAR_W / 2.0);
        for (i, &(h, brightness)) in self.bars.iter().enumerate() {
            let x = PAD_X + i as f32 * (BAR_W + BAR_GAP);
            let offset = (x + BAR_W / 2.0 - PILL_W / 2.0).abs();
            let visible = ((reveal - offset) / 6.0 + 1.0).clamp(0.0, 1.0);
            if visible > 0.0 {
                self.canvas.round_rect(x, center_y - h / 2.0, BAR_W, h, BAR_W / 2.0, 1.0, brightness * visible);
            }
        }
        true
    }

    fn update_bars(&mut self, mode: Mode, t: f32, dt: f32) {
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let fi = i as f32;
            let (h, brightness) = match mode {
                Mode::Loading => {
                    let pos = (t / 1.1).fract() * (BARS as f32 + 6.0) - 3.0;
                    let g = (-((fi - pos) / 1.8).powi(2)).exp();
                    (DOT + 8.0 * g, 0.35 + 0.65 * g)
                }
                Mode::Listening => (DOT + self.levels[i] * (BAR_MAX - DOT), 1.0),
                Mode::Transcribing => {
                    let w = 0.5 + 0.5 * (t * std::f32::consts::TAU / 1.3 - fi * 0.42).sin();
                    (DOT + (BAR_MAX - DOT) * (0.1 + 0.32 * w), 0.55 + 0.45 * w)
                }
                Mode::Leaving(_) => (DOT, 1.0),
            };
            let tau = match (mode == Mode::Listening, h > bar.0) {
                (true, true) => 0.025, // jump up fast
                (true, false) => 0.10, // fall slower
                _ => 0.08,
            };
            let k = 1.0 - (-dt / tau).exp();
            bar.0 += (h - bar.0) * k;
            bar.1 += (brightness - bar.1) * k;
        }
    }
}

/// Black pill with a faint white outline; `x`, `y` is the top-left corner.
fn draw_pill(canvas: &mut Canvas, x: f32, y: f32, w: f32) {
    canvas.round_rect(x, y, w, PILL_H, PILL_H / 2.0, 1.0, 0.16);
    canvas.round_rect(x + 1.0, y + 1.0, w - 2.0, PILL_H - 2.0, PILL_H / 2.0 - 1.0, 0.0, 1.0);
}

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_out_back(t: f32) -> f32 {
    const C1: f32 = 1.70158;
    1.0 + (C1 + 1.0) * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

fn ease_in_back(t: f32) -> f32 {
    const C1: f32 = 1.70158;
    (C1 + 1.0) * t.powi(3) - C1 * t.powi(2)
}
