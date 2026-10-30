//! Turns the newest microphone samples into equalizer bar levels.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::RATE;

/// 64 ms window at 16 kHz.
pub const FFT_LEN: usize = 1024;
const F_LO: f32 = 90.0;
const F_HI: f32 = 7000.0;
/// Loudness span from an empty bar to a full one.
const RANGE_DB: f32 = 36.0;
/// The scale never zooms in further than this.
const MIN_REFERENCE_DB: f32 = -30.0;
/// Bands quieter than this are room noise and stay dots (speech peaks at -30..-20 dB).
const NOISE_GATE_DB: f32 = -60.0;
/// How fast the scale recovers after a loud sound.
const REFERENCE_DECAY_DB_PER_S: f32 = 12.0;

pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// FFT bin range per bar plus a tilt (dB) that lifts the quieter high frequencies.
    bands: Vec<(usize, usize, f32)>,
    reference: f32,
}

impl Analyzer {
    pub fn new(bars: usize) -> Analyzer {
        let fft = FftPlanner::new().plan_fft_forward(FFT_LEN);
        let window = (0..FFT_LEN)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_LEN as f32).cos())
            .collect();
        let bin_hz = RATE as f32 / FFT_LEN as f32;
        let mut bands = Vec::with_capacity(bars);
        let mut lo = (F_LO / bin_hz).round() as usize;
        for k in 1..=bars {
            let f = F_LO * (F_HI / F_LO).powf(k as f32 / bars as f32);
            let hi = ((f / bin_hz).round() as usize).max(lo + 1);
            let center_hz = (lo + hi) as f32 * 0.5 * bin_hz;
            bands.push((lo, hi, 3.0 * (center_hz / 500.0).log2()));
            lo = hi;
        }
        Analyzer {
            scratch: vec![Complex::default(); fft.get_inplace_scratch_len()],
            fft,
            window,
            buf: vec![Complex::default(); FFT_LEN],
            bands,
            reference: MIN_REFERENCE_DB,
        }
    }

    /// Converts `FFT_LEN` samples into one level in 0..=1 per bar.
    pub fn analyze(&mut self, samples: &[f32], dt: f32, out: &mut [f32]) {
        for ((b, &s), &w) in self.buf.iter_mut().zip(samples).zip(&self.window) {
            *b = Complex::new(s * w, 0.0);
        }
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);

        // A full-scale sine reads 0 dB: Hann window gain 1/2, one-sided spectrum ×2.
        let norm = (4.0 / FFT_LEN as f32).powi(2);
        let mut loudest = f32::MIN;
        for (o, &(lo, hi, tilt)) in out.iter_mut().zip(&self.bands) {
            let peak = self.buf[lo..hi].iter().map(|c| c.norm_sqr()).fold(0.0, f32::max);
            *o = 10.0 * (peak * norm + 1e-12).log10() + tilt;
            loudest = loudest.max(*o);
        }
        self.reference = (self.reference - REFERENCE_DECAY_DB_PER_S * dt).max(loudest).max(MIN_REFERENCE_DB);
        let floor = (self.reference - RANGE_DB).max(NOISE_GATE_DB);
        let span = self.reference - floor;
        for o in out.iter_mut() {
            *o = ((*o - floor) / span).clamp(0.0, 1.0);
        }
    }
}
