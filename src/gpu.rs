//! Decides whether a dictation runs on the graphics card. A card that is busy
//! or short of memory is left alone: the CPU is slower, but dictation must not
//! fight a game or a training run, and a model that does not fit would spill
//! into system memory or fail to load.

use std::ffi::CStr;
use std::path::PathBuf;
use std::time::Duration;

use whisper_rs::whisper_rs_sys as sys;

/// Both models with their buffers take about 1.2 GB; the rest is headroom.
const NEEDED_BYTES: usize = 2 << 30;
/// A card at least this busy in every sample is taken for busy.
const BUSY_PERCENT: u32 = 90;
const BUSY_SAMPLES: usize = 3;
const BUSY_SAMPLE_GAP: Duration = Duration::from_millis(30);

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// The card when it is free, the CPU otherwise.
    Auto,
    /// The card whatever its load.
    Always,
    Never,
}

impl Mode {
    pub fn parse(value: Option<&str>) -> Mode {
        match value {
            Some("0" | "off" | "never") => Mode::Never,
            Some("1" | "on" | "always") => Mode::Always,
            _ => Mode::Auto,
        }
    }
}

/// The card to run on, as whisper.cpp numbers its GPU devices. `None`: use the CPU.
pub fn pick(mode: Mode) -> Option<i32> {
    if mode == Mode::Never {
        return None;
    }
    let mut index = 0; // counts discrete and integrated cards, like whisper.cpp does
    // SAFETY: the registry calls only read ggml's static list of devices, and
    // `dev` comes from that list.
    for dev in (0..unsafe { sys::ggml_backend_dev_count() }).map(|i| unsafe { sys::ggml_backend_dev_get(i) }) {
        let discrete = match unsafe { sys::ggml_backend_dev_type(dev) } {
            sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU => true,
            sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU => false,
            _ => continue,
        };
        let this = index;
        index += 1;
        if mode == Mode::Always {
            return Some(this);
        }
        if !discrete {
            continue; // an integrated card is no faster than the CPU kernels
        }
        let name = unsafe { CStr::from_ptr(sys::ggml_backend_dev_description(dev)) }.to_string_lossy();
        let (mut free, mut total) = (0, 0);
        unsafe { sys::ggml_backend_dev_memory(dev, &mut free, &mut total) };
        let gb = |bytes: usize| bytes as f32 / (1u64 << 30) as f32;
        if free < NEEDED_BYTES {
            log!("{name}: {:.1} of {:.1} GB free, using the CPU", gb(free), gb(total));
            continue;
        }
        if let Some(busy) = busy_percent() {
            if busy >= BUSY_PERCENT {
                log!("{name}: {busy}% busy, using the CPU");
                continue;
            }
            log!("{name}: {:.1} GB free, {busy}% busy", gb(free));
        } else {
            log!("{name}: {:.1} GB free", gb(free));
        }
        return Some(this);
    }
    None
}

/// The lowest of a few load samples of the card with the most memory, so a
/// short spike does not count as busy. `None` when the driver does not report
/// the load (only amdgpu does); the memory check still applies then.
fn busy_percent() -> Option<u32> {
    let read = |path: PathBuf| std::fs::read_to_string(path).ok()?.trim().parse::<u64>().ok();
    let card = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path().join("device")))
        .filter(|device| device.join("gpu_busy_percent").exists())
        .max_by_key(|device| read(device.join("mem_info_vram_total")).unwrap_or(0))?;
    let mut lowest = None;
    for sample in 0..BUSY_SAMPLES {
        if sample > 0 {
            std::thread::sleep(BUSY_SAMPLE_GAP);
        }
        let busy = read(card.join("gpu_busy_percent"))? as u32;
        lowest = Some(lowest.map_or(busy, |l: u32| l.min(busy)));
    }
    lowest
}
