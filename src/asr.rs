//! Speech recognition with whisper.cpp on the CPU.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, get_lang_id};

use crate::audio::{RATE, Tap};
use crate::{Event, Notifier};

pub struct Config {
    pub model: PathBuf,
    /// Small model that only decides the language.
    pub detector: PathBuf,
    pub threads: i32,
    /// Comma-separated candidates ("ru,en"): the small detector model picks one
    /// per dictation. A single code fixes the language. "auto" lets Whisper
    /// detect any language, which costs a second full pass of the big model.
    pub language: String,
    pub prompt: Option<String>,
    pub flash_attn: bool,
    /// Run a BF16 copy of the main model when whisper.cpp has AVX512-BF16
    /// kernels: faster than Q8_0, but 0.75 GB more memory.
    pub bf16: bool,
    /// Shortest encoder window in seconds; 30 keeps Whisper's full window.
    /// 10 s matched the full window on test dictations, 5–7 s duplicated or misspelled words.
    pub min_window: f32,
    /// Directory where every recording is saved as WAV (off when unset).
    pub save_dir: Option<PathBuf>,
}

impl Config {
    /// Defaults, overridable through VOICETOTEXTN_* environment variables.
    pub fn from_env() -> Config {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let physical_cores = std::thread::available_parallelism().map_or(4, |n| n.get() / 2).max(1);
        let models = home.join(".local/share/voicetotextn");
        Config {
            model: var("VOICETOTEXTN_MODEL")
                .map(PathBuf::from)
                .unwrap_or_else(|| models.join("ggml-large-v3-turbo-q8_0.bin")),
            detector: var("VOICETOTEXTN_DETECTOR").map(PathBuf::from).unwrap_or_else(|| models.join("ggml-tiny.bin")),
            threads: var("VOICETOTEXTN_THREADS").and_then(|v| v.parse().ok()).unwrap_or(physical_cores as i32),
            language: var("VOICETOTEXTN_LANG").unwrap_or_else(|| "ru,en".into()),
            prompt: var("VOICETOTEXTN_PROMPT"),
            flash_attn: var("VOICETOTEXTN_FLASH").is_none_or(|v| v != "0"),
            bf16: var("VOICETOTEXTN_BF16").is_none_or(|v| v != "0"),
            min_window: var("VOICETOTEXTN_MIN_WINDOW").and_then(|v| v.parse().ok()).unwrap_or(10.0),
            save_dir: var("VOICETOTEXTN_SAVE_DIR").map(PathBuf::from),
        }
    }
}

pub struct Models {
    main: WhisperContext,
    /// Present only when the language is chosen per dictation.
    detector: Option<WhisperContext>,
}

pub fn load(cfg: &Config) -> Result<Models, String> {
    whisper_rs::install_logging_hooks(); // routes whisper.cpp's chatter into the void
    let open = |path: &std::path::Path, bf16: bool| {
        let path = match bf16.then(|| crate::bf16::cached(path)) {
            Some(Ok(Some(copy))) => copy,
            Some(Err(e)) => {
                log!("no BF16 copy, using the model as is: {e}");
                path.to_path_buf()
            }
            _ => path.to_path_buf(),
        };
        let mut params = WhisperContextParameters::default();
        params.flash_attn(cfg.flash_attn);
        let path = path.to_str().ok_or("model path is not UTF-8")?;
        WhisperContext::new_with_params(path, params).map_err(|e| format!("cannot load {path}: {e}"))
    };
    // The detector is too small to gain anything from BF16.
    let detector = if cfg.language.contains(',') { Some(open(&cfg.detector, false)?) } else { None };
    Ok(Models { main: open(&cfg.model, cfg.bf16 && crate::bf16::supported())?, detector })
}

/// `language`: one already picked for this recording (see `Worker`).
pub fn transcribe(models: &Models, cfg: &Config, audio: &[f32], language: Option<&str>) -> Result<String, String> {
    let Some(audio) = speech(audio) else {
        return Ok(String::new());
    };
    let language = match (language, &models.detector) {
        (Some(language), _) => language,
        (None, Some(detector)) => {
            let t = Instant::now();
            let (language, scores) = pick_language(detector, cfg.threads, &audio, &cfg.language)?;
            log!("language {language} ({scores}) in {} ms", t.elapsed().as_millis());
            language
        }
        (None, None) => cfg.language.as_str(),
    };
    let mut state = models.main.create_state().map_err(|e| e.to_string())?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(cfg.threads);
    params.set_language(Some(language));
    params.set_translate(false);
    params.set_no_timestamps(true);
    params.set_no_context(true);
    params.set_suppress_blank(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // The encoder has 1500 positions per 30 s; a shorter window is proportionally cheaper.
    let seconds = (audio.len() as f32 / RATE as f32 + 1.0).clamp(cfg.min_window.min(30.0), 30.0);
    params.set_audio_ctx((seconds / 30.0 * 1500.0).ceil() as i32);
    if let Some(prompt) = &cfg.prompt {
        params.set_initial_prompt(prompt);
    }
    state.full(params, &audio).map_err(|e| e.to_string())?;

    let mut text = String::new();
    for segment in state.as_iter() {
        text.push_str(&segment.to_str_lossy().map_err(|e| e.to_string())?);
    }
    Ok(text.trim().to_string())
}

/// Picks the language of the speech in `audio`, with the scores for the log.
/// `None`: the language is fixed, or nothing was said.
pub fn pick<'a>(models: &Models, cfg: &'a Config, audio: &[f32]) -> Result<Option<(&'a str, String)>, String> {
    let (Some(detector), Some(speech)) = (&models.detector, speech(audio)) else {
        return Ok(None);
    };
    pick_language(detector, cfg.threads, &speech, &cfg.language).map(Some)
}

/// Asks the small model which of the `candidates` ("ru,en") the first 30 s
/// sound like. Also returns the scores, for the log.
fn pick_language<'a>(
    detector: &WhisperContext,
    threads: i32,
    audio: &[f32],
    candidates: &'a str,
) -> Result<(&'a str, String), String> {
    let mut state = detector.create_state().map_err(|e| e.to_string())?;
    state.pcm_to_mel(&audio[..audio.len().min(RATE as usize * 30)], threads as usize).map_err(|e| e.to_string())?;
    let (_, probs) = state.lang_detect(0, threads as usize).map_err(|e| e.to_string())?;
    let prob = |code: &str| get_lang_id(code).map_or(0.0, |id| probs[id as usize]);
    let candidates = candidates.split(',').map(str::trim);
    let best = candidates.clone().max_by(|a, b| prob(a).total_cmp(&prob(b))).unwrap_or("auto");
    let scores: Vec<String> = candidates.map(|c| format!("{c} {:.2}", prob(c))).collect();
    Ok((best, scores.join(", ")))
}

/// The speech in a recording, padded to the length whisper.cpp accepts.
/// `None` means nothing was said.
fn speech(audio: &[f32]) -> Option<Vec<f32>> {
    let mut speech = audio[speech_bounds(audio)?].to_vec();
    // whisper.cpp refuses clips shorter than one second.
    speech.resize(speech.len().max(RATE as usize * 11 / 10), 0.0);
    Some(speech)
}

/// Cuts leading and trailing silence (keeping a margin), so Whisper neither
/// wastes time on it nor invents text there. `None` means nothing was said.
fn speech_bounds(audio: &[f32]) -> Option<std::ops::Range<usize>> {
    const FRAME: usize = RATE as usize / 50; // 20 ms
    const MARGIN: usize = 15; // frames, 300 ms
    const MIN_SPEECH: usize = 5; // frames, 100 ms
    let rms: Vec<f32> = audio
        .chunks(FRAME)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let mut sorted = rms.clone();
    sorted.sort_by(f32::total_cmp);
    let noise = *sorted.get(sorted.len() / 10)?;
    let threshold = (noise * 4.0).max(0.004); // at least -48 dBFS
    if rms.iter().filter(|&&r| r > threshold).count() < MIN_SPEECH {
        return None;
    }
    let first = rms.iter().position(|&r| r > threshold)?;
    let last = rms.iter().rposition(|&r| r > threshold)?;
    Some(first.saturating_sub(MARGIN) * FRAME..((last + 1 + MARGIN) * FRAME).min(audio.len()))
}

/// Loads the model in the background, then waits for the recording. While it
/// waits, the detector picks the language of the speech heard so far, so the
/// stop usually only waits for the transcription.
pub struct Worker {
    audio_tx: Option<mpsc::Sender<Vec<f32>>>,
}

impl Worker {
    pub fn spawn(cfg: Config, notifier: Notifier, recording: Tap) -> Worker {
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
        std::thread::Builder::new()
            .name("asr".into())
            .spawn(move || {
                let t = Instant::now();
                let models = match load(&cfg) {
                    Ok(models) => models,
                    Err(e) => return notifier.send(Event::Failed(e)),
                };
                log!("models loaded in {} ms", t.elapsed().as_millis());
                notifier.send(Event::ModelReady);

                // The last pick: samples it heard, the language and its scores.
                // It stands while it has heard at least 3/4 of the recording.
                let mut early: Option<(usize, &str, String)> = None;
                let stands = |heard: usize, len: usize| 4 * heard >= 3 * len;
                let audio = loop {
                    match audio_rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(audio) => break audio,
                        Err(RecvTimeoutError::Disconnected) => return, // cancelled
                        Err(RecvTimeoutError::Timeout) if models.detector.is_none() => continue,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    let heard = recording.copy();
                    if early.as_ref().is_some_and(|e| stands(e.0, heard.len())) {
                        continue;
                    }
                    match pick(&models, &cfg, &heard) {
                        Ok(Some((language, scores))) => early = Some((heard.len(), language, scores)),
                        Ok(None) => {}
                        Err(e) => log!("language while recording: {e}"),
                    }
                };
                if let Some(dir) = &cfg.save_dir {
                    save(dir, &audio);
                }
                let language = early.filter(|e| stands(e.0, audio.len())).map(|(_, language, scores)| {
                    log!("language {language} ({scores}) picked while recording");
                    language
                });
                let t = Instant::now();
                let result = transcribe(&models, &cfg, &audio, language);
                log!(
                    "{:.1} s of audio transcribed in {} ms",
                    audio.len() as f32 / RATE as f32,
                    t.elapsed().as_millis()
                );
                drop(models); // give the memory back before the paste lingers
                notifier.send(match result {
                    Ok(text) => Event::Text(text),
                    Err(e) => Event::Failed(e),
                });
            })
            .expect("spawn asr thread");
        Worker { audio_tx: Some(audio_tx) }
    }

    pub fn submit(&mut self, audio: Vec<f32>) {
        if let Some(tx) = self.audio_tx.take() {
            let _ = tx.send(audio);
        }
    }

    pub fn cancel(&mut self) {
        self.audio_tx = None;
    }
}

fn save(dir: &std::path::Path, audio: &[f32]) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let path = dir.join(format!("{stamp}.wav"));
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| crate::wav::write(&path, audio)) {
        log!("cannot save {}: {e}", path.display());
    }
}
