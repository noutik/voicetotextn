//! Microphone capture through PipeWire's PulseAudio interface, 16 kHz mono f32
//! (the format Whisper wants, so PipeWire does the resampling).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::{Event, Notifier};

pub const RATE: u32 = 16_000;
/// 16 ms per read: small enough for a smooth equalizer.
const CHUNK: usize = 256;

pub struct Recorder {
    samples: Arc<Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
}

impl Recorder {
    pub fn start(notifier: Notifier) -> Recorder {
        let samples = Arc::new(Mutex::new(Vec::with_capacity(RATE as usize * 60)));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (samples, stop) = (samples.clone(), stop.clone());
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    if let Err(e) = capture(&samples, &stop) {
                        notifier.send(Event::Failed(format!("microphone: {e}")));
                    }
                })
                .expect("spawn audio thread");
        }
        Recorder { samples, stop }
    }

    /// Fills `out` with the newest samples (zeros before the recording started)
    /// and returns how many samples have been recorded in total.
    pub fn tail(&self, out: &mut [f32]) -> usize {
        let samples = self.samples.lock().unwrap();
        let n = samples.len().min(out.len());
        let split = out.len() - n;
        out[..split].fill(0.0);
        out[split..].copy_from_slice(&samples[samples.len() - n..]);
        samples.len()
    }

    /// Stops the microphone and returns everything recorded. The audio thread
    /// is not joined: a stalled device must not freeze the overlay.
    pub fn finish(self) -> Vec<f32> {
        std::mem::take(&mut *self.samples.lock().unwrap())
    }

    pub fn tap(&self) -> Tap {
        Tap(self.samples.clone())
    }
}

/// Lets another thread look at the recording while it goes on.
pub struct Tap(Arc<Mutex<Vec<f32>>>);

impl Tap {
    /// Everything recorded so far (nothing once the recording is finished).
    pub fn copy(&self) -> Vec<f32> {
        self.0.lock().unwrap().clone()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn capture(samples: &Mutex<Vec<f32>>, stop: &AtomicBool) -> Result<(), libpulse_binding::error::PAErr> {
    let spec = Spec { format: Format::F32le, channels: 1, rate: RATE };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: (CHUNK * 4) as u32,
    };
    let pa = Simple::new(None, "voicetotextn", Direction::Record, None, "dictation", &spec, None, Some(&attr))?;
    let mut bytes = [0u8; CHUNK * 4];
    while !stop.load(Ordering::Relaxed) {
        pa.read(&mut bytes)?;
        let chunk = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        samples.lock().unwrap().extend(chunk);
    }
    Ok(())
}
