//! Speech recognition with whisper.cpp, on the graphics card when it is free
//! and on the CPU otherwise.

use std::borrow::Cow;
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, get_lang_id};

use rustix::event::{PollFd, PollFlags, poll};
use rustix::pipe::PipeFlags;
use rustix::process::WaitOptions;

use crate::audio::{RATE, Tap};
use crate::{Event, Notifier};

pub struct Config {
    pub model: PathBuf,
    /// Small model that only decides the language.
    pub detector: PathBuf,
    pub threads: i32,
    /// Comma-separated candidates ("ru,en"): the small detector model picks one
    /// per dictation, the first unless it clearly hears another. A single code
    /// fixes the language. "auto" lets Whisper detect any language, which costs
    /// a second full pass of the big model.
    pub language: String,
    pub prompt: Option<String>,
    pub flash_attn: bool,
    pub gpu: crate::gpu::Mode,
    /// On the CPU, run a BF16 copy of the main model when whisper.cpp has
    /// AVX512-BF16 kernels: faster than Q8_0, but 0.75 GB more memory.
    pub bf16: bool,
    /// Shortest encoder window on the CPU, in seconds; 30 keeps Whisper's full
    /// window. Every cut costs accuracy: on 483 test clips even a 20 s window
    /// changed the text of 4 in 10, and shorter ones change more.
    pub min_window: f32,
    /// On the CPU, transcribe at every pause of the speech: if nothing more is
    /// said, the text is ready at the stop.
    pub ahead: bool,
    /// On the CPU, keep the text made at a pause for speech at least this many
    /// seconds long when more is said after it, so the stop waits only for
    /// the last part. Off when unset: parts cut off each other's context, and
    /// on 44 test recordings the words that differed from the full-window text
    /// went from 4.6% to 11.4% (10 s) and 13.5% (6 s), mostly false starts that
    /// Whisper leaves out when it hears what follows.
    pub parts: Option<f32>,
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
            gpu: crate::gpu::Mode::parse(var("VOICETOTEXTN_GPU").as_deref()),
            bf16: var("VOICETOTEXTN_BF16").is_none_or(|v| v != "0"),
            min_window: var("VOICETOTEXTN_MIN_WINDOW").and_then(|v| v.parse().ok()).unwrap_or(10.0),
            ahead: var("VOICETOTEXTN_AHEAD").is_none_or(|v| v != "0"),
            parts: var("VOICETOTEXTN_PARTS").and_then(|v| v.parse().ok()).filter(|&seconds| seconds > 0.0),
            save_dir: var("VOICETOTEXTN_SAVE_DIR").map(PathBuf::from),
        }
    }
}

pub struct Models {
    main: WhisperContext,
    /// Present only when the language is chosen per dictation.
    detector: Option<WhisperContext>,
    pub on_gpu: bool,
}

pub fn load(cfg: &Config) -> Result<Models, String> {
    whisper_rs::install_logging_hooks(); // routes whisper.cpp's chatter into the void
    if let Some(card) = crate::gpu::pick(cfg.gpu) {
        match open(cfg, Some(card)) {
            Ok(models) => return Ok(models),
            Err(e) => log!("the graphics card failed, using the CPU: {e}"),
        }
    }
    open(cfg, None)
}

/// Loads the models onto the graphics card `gpu`, or for the CPU.
fn open(cfg: &Config, gpu: Option<i32>) -> Result<Models, String> {
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
        params.use_gpu(gpu.is_some());
        params.gpu_device(gpu.unwrap_or(0));
        let path = path.to_str().ok_or("model path is not UTF-8")?;
        WhisperContext::new_with_params(path, params).map_err(|e| format!("cannot load {path}: {e}"))
    };
    // The detector is too small to gain anything from BF16, and BF16 is a CPU format.
    let detector = if cfg.language.contains(',') { Some(open(&cfg.detector, false)?) } else { None };
    let bf16 = gpu.is_none() && cfg.bf16 && crate::bf16::supported();
    Ok(Models { main: open(&cfg.model, bf16)?, detector, on_gpu: gpu.is_some() })
}

/// Transcribes a whole recording. `language`: one already picked for it.
pub fn transcribe(models: &Models, cfg: &Config, audio: &[f32], language: Option<&str>) -> Result<String, String> {
    let Some(speech) = speech(audio) else {
        return Ok(String::new());
    };
    let speech = &audio[speech];
    let language = match (language, &models.detector) {
        (Some(language), _) => language,
        (None, Some(detector)) => {
            let t = Instant::now();
            let (language, scores) = pick_language(detector, cfg.threads, speech, &cfg.language)?;
            log!("language {language} ({scores}) in {} ms", t.elapsed().as_millis());
            language
        }
        (None, None) => cfg.language.as_str(),
    };
    decode(models, cfg, speech, language)
}

/// Transcribes `speech`: a stretch of a recording with a little silence
/// around it.
fn decode(models: &Models, cfg: &Config, speech: &[f32], language: &str) -> Result<String, String> {
    let speech = padded(speech);
    let mut state = models.main.create_state().map_err(|e| e.to_string())?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(cfg.threads);
    params.set_language(Some(language));
    params.set_translate(false);
    // Without timestamps Whisper moves on by whole 30 s windows and drops the
    // words cut at their borders, so longer recordings are decoded with them.
    // Shorter ones do better without: the text has one place to end.
    params.set_no_timestamps(speech.len() <= RATE as usize * 30);
    params.set_no_context(true);
    params.set_suppress_blank(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    if !models.on_gpu {
        // The encoder has 1500 positions per 30 s, and the CPU pays for each: a
        // shorter window is proportionally cheaper. It keeps some silence after
        // the speech, or Whisper misses the end and repeats the last words.
        let seconds = (speech.len() as f32 / RATE as f32 + WINDOW_TAIL).clamp(cfg.min_window.min(30.0), 30.0);
        params.set_audio_ctx((seconds / 30.0 * 1500.0).ceil() as i32);
    }
    if let Some(prompt) = &cfg.prompt {
        params.set_initial_prompt(prompt);
    }
    state.full(params, &speech).map_err(|e| e.to_string())?;

    let mut text = String::new();
    for segment in state.as_iter() {
        text.push_str(&segment.to_str_lossy().map_err(|e| e.to_string())?);
    }
    Ok(collapse_repeats(text.trim()))
}

/// Seconds of silence a shortened encoder window keeps after the speech.
const WINDOW_TAIL: f32 = 3.0;

/// Drops the repeats of a phrase said over and over: three or more words three
/// times in a row, or five or more twice. Whisper loops like this when the
/// recording ends mid-word; a speaker hardly ever does.
fn collapse_repeats(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let keys: Vec<String> = words
        .iter()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect())
        .collect();
    let mut kept = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let rest = &keys[i..];
        let repeat = (3..=rest.len() / 2).find_map(|n| {
            let copies = rest.chunks_exact(n).take_while(|chunk| *chunk == &rest[..n]).count();
            (copies >= 3 || (copies == 2 && n >= 5)).then_some((n, copies))
        });
        let (n, copies) = repeat.unwrap_or((1, 1));
        kept.extend_from_slice(&words[i..i + n]);
        i += n * copies;
    }
    if kept.len() == words.len() { text.to_string() } else { kept.join(" ") }
}

/// Picks the language of the speech in `audio`, with the scores for the log.
/// `None`: the language is fixed, or nothing was said.
pub fn pick<'a>(models: &Models, cfg: &'a Config, audio: &[f32]) -> Result<Option<(&'a str, String)>, String> {
    let (Some(detector), Some(speech)) = (&models.detector, speech(audio)) else {
        return Ok(None);
    };
    pick_language(detector, cfg.threads, &audio[speech], &cfg.language).map(Some)
}

/// Asks the small model which of the `candidates` ("ru,en") the first 30 s
/// sound like. Also returns the scores, for the log.
///
/// The first candidate is the main language and wins unless another one is
/// more likely than not. On short or mumbled clips the small model scores
/// every candidate near zero, and the larger of two such scores is noise:
/// it sent 4% of Russian test clips to English.
fn pick_language<'a>(
    detector: &WhisperContext,
    threads: i32,
    audio: &[f32],
    candidates: &'a str,
) -> Result<(&'a str, String), String> {
    let audio = padded(&audio[..audio.len().min(RATE as usize * 30)]);
    let mut state = detector.create_state().map_err(|e| e.to_string())?;
    state.pcm_to_mel(&audio, threads as usize).map_err(|e| e.to_string())?;
    let (_, probs) = state.lang_detect(0, threads as usize).map_err(|e| e.to_string())?;
    let prob = |code: &str| get_lang_id(code).map_or(0.0, |id| probs[id as usize]);
    let scores: Vec<(&str, f32)> = candidates.split(',').map(|c| (c.trim(), prob(c.trim()))).collect();
    let log: Vec<String> = scores.iter().map(|(c, p)| format!("{c} {p:.2}")).collect();
    Ok((choose_language(&scores), log.join(", ")))
}

fn choose_language<'a>(scores: &[(&'a str, f32)]) -> &'a str {
    let confident = scores.iter().find(|(_, p)| *p >= 0.5);
    confident.or(scores.first()).map_or("auto", |(language, _)| language)
}

/// 20 ms: the step in which loudness is measured.
const FRAME: usize = RATE as usize / 50;
/// Frames of silence kept around the speech, 300 ms.
const MARGIN: usize = 15;
/// Fewer loud frames than this (100 ms) are a click, not a word.
const MIN_SPEECH: usize = 5;
/// Frames of silence after which the speech so far is transcribed ahead, 400 ms.
const PAUSE: usize = 20;
/// Less speech than this (1 s of loud frames) at the end of a recording is
/// transcribed together with the part before it.
const MIN_TAIL: usize = 50;

/// How loud every frame of a recording is.
fn frame_rms(audio: &[f32]) -> Vec<f32> {
    audio.chunks(FRAME).map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt()).collect()
}

/// The loudness above which a frame counts as speech: well above the
/// background of the recording, and at least -48 dBFS.
fn speech_threshold(rms: &[f32]) -> f32 {
    let mut sorted = rms.to_vec();
    sorted.sort_by(f32::total_cmp);
    let noise = sorted.get(sorted.len() / 10).copied().unwrap_or(0.0);
    (noise * 4.0).max(0.004)
}

/// The frames from the first loud one to the last. `None`: nothing was said.
fn spoken(loud: &[bool]) -> Option<Range<usize>> {
    if loud.iter().filter(|&&l| l).count() < MIN_SPEECH {
        return None;
    }
    Some(loud.iter().position(|&l| l)?..loud.iter().rposition(|&l| l)? + 1)
}

/// The samples of the frames `speech` with a margin of silence, so Whisper
/// neither wastes time on the rest of it nor invents text there. The margin
/// does not reach before the frame `floor`.
fn with_margin(speech: &Range<usize>, floor: usize, samples: usize) -> Range<usize> {
    (speech.start.saturating_sub(MARGIN).max(floor) * FRAME).min(samples)..((speech.end + MARGIN) * FRAME).min(samples)
}

/// The samples of a recording that hold its speech. `None`: nothing was said.
fn speech(audio: &[f32]) -> Option<Range<usize>> {
    let rms = frame_rms(audio);
    let threshold = speech_threshold(&rms);
    let loud: Vec<bool> = rms.iter().map(|&r| r > threshold).collect();
    Some(with_margin(&spoken(&loud)?, 0, audio.len()))
}

/// whisper.cpp refuses clips shorter than one second.
fn padded(speech: &[f32]) -> Cow<'_, [f32]> {
    let shortest = RATE as usize * 11 / 10;
    if speech.len() >= shortest {
        return Cow::Borrowed(speech);
    }
    let mut speech = speech.to_vec();
    speech.resize(shortest, 0.0);
    Cow::Owned(speech)
}

/// A stretch of speech, in frames of the recording, and its text.
struct Part {
    speech: Range<usize>,
    text: String,
}

/// The text of a recording as it grows. On the CPU the worker does not wait
/// for the end: at every pause it transcribes the speech not yet settled.
struct Draft {
    /// Settled: the speech went on after them.
    parts: Vec<Part>,
    /// Made at a pause: the last part, if nothing more is said.
    ahead: Option<Part>,
    /// The frames of speech (with its pauses) from which a part made at a
    /// pause is settled when more is said after it. Shorter speech is
    /// transcribed again together with what follows.
    min_part: usize,
}

impl Draft {
    /// The speech to transcribe now, if any. `loud`: the frames recorded so
    /// far, `last`: the recording is over.
    fn next(&mut self, loud: &[bool], last: bool) -> Option<Range<usize>> {
        if let Some(part) = self.ahead.take() {
            let after = loud_after(loud, part.speech.end);
            if after < MIN_SPEECH {
                // Nothing was said since: it waits for the end, or is the end.
                if last {
                    self.parts.push(part)
                } else {
                    self.ahead = Some(part)
                }
                return None;
            }
            if !outrun(part.speech.len(), after, last, self.min_part) {
                self.parts.push(part);
            }
        }
        let from = self.parts.last().map_or(0, |part| part.speech.end);
        let rest = spoken(loud.get(from..)?)?;
        let rest = from + rest.start..from + rest.end;
        if !last {
            return (loud.len() - rest.end >= PAUSE).then_some(rest);
        }
        if loud_after(loud, from) < MIN_TAIL {
            if let Some(before) = self.parts.pop() {
                return Some(before.speech.start..rest.end);
            }
        }
        Some(rest)
    }

    fn text(&self) -> String {
        let texts: Vec<&str> =
            self.parts.iter().map(|part| part.text.as_str()).filter(|text| !text.is_empty()).collect();
        texts.join(" ")
    }
}

fn loud_after(loud: &[bool], frame: usize) -> usize {
    loud.get(frame..).map_or(0, |rest| rest.iter().filter(|&&l| l).count())
}

/// Whether a part made at a pause is of no use: `after` loud frames followed
/// its `speech` frames, and it is too short to stand apart from them, or they
/// are too few to stand apart from it.
fn outrun(speech: usize, after: usize, last: bool, min_part: usize) -> bool {
    after >= MIN_SPEECH && (speech < min_part || (last && after < MIN_TAIL))
}

/// What the session tells the worker.
#[derive(Default)]
struct Shared {
    /// The whole recording and the time it was handed over, once it is over.
    end: OnceLock<(Vec<f32>, Instant)>,
    cancelled: AtomicBool,
}

/// A pass made while the recording goes on. It runs in a forked copy of the
/// process, because whisper.cpp cannot stop in the middle of its encoder,
/// which is nearly all of a pass: a pass the speech has outrun would hold the
/// CPU for seconds, and the stop would wait for it. A process can be killed.
struct Flight {
    child: Option<rustix::process::Pid>,
    /// The child writes the text here and exits.
    pipe: OwnedFd,
    text: Vec<u8>,
    /// The speech the pass transcribes, in frames.
    speech: Range<usize>,
    seconds: f32,
    started: Instant,
}

impl Flight {
    fn start(
        models: &Models,
        cfg: &Config,
        samples: &[f32],
        language: &str,
        speech: Range<usize>,
    ) -> Result<Flight, String> {
        let (pipe, child_pipe) =
            rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).map_err(|e| format!("pipe: {e}"))?;
        // SAFETY: the child has only this thread, and all it does is compute
        // on memory this thread owns or only reads (the models, the samples),
        // write to its pipe and leave without running any destructor. glibc
        // keeps malloc usable in the child of a threaded process.
        match unsafe { libc::fork() } {
            -1 => Err(format!("fork: {}", std::io::Error::last_os_error())),
            0 => {
                // Dies with the worker thread, whatever happens to it.
                let _ = rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL));
                let done = decode(models, cfg, samples, language).is_ok_and(|text| {
                    text.as_bytes().chunks(4096).all(|chunk| rustix::io::write(&child_pipe, chunk).is_ok())
                });
                // SAFETY: ends the child at once, with none of the session's cleanup.
                unsafe { libc::_exit(if done { 0 } else { 1 }) }
            }
            pid => Ok(Flight {
                child: rustix::process::Pid::from_raw(pid),
                pipe,
                text: Vec::new(),
                speech,
                seconds: samples.len() as f32 / RATE as f32,
                started: Instant::now(),
            }),
        }
    }

    /// Waits up to `timeout` for the pass. `None`: it is still under way;
    /// `Some(None)`: it failed.
    fn land(&mut self, timeout: Duration) -> Option<Option<String>> {
        let timeout = rustix::time::Timespec { tv_sec: 0, tv_nsec: timeout.subsec_nanos() as _ };
        loop {
            match poll(&mut [PollFd::new(&self.pipe, PollFlags::IN)], Some(&timeout)) {
                Ok(0) => return None,
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(_) => return Some(None),
            }
            let mut chunk = [0u8; 4096];
            match rustix::io::read(&self.pipe, &mut chunk) {
                Ok(0) => break, // the child has left
                Ok(n) => self.text.extend_from_slice(&chunk[..n]),
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => {}
                Err(_) => return Some(None),
            }
        }
        let status =
            self.child.take().and_then(|child| rustix::process::waitpid(Some(child), WaitOptions::empty()).ok()?);
        let done = status.is_some_and(|(_, status)| status.exit_status() == Some(0));
        Some(done.then(|| String::from_utf8_lossy(&self.text).into_owned()))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = rustix::process::kill_process(child, rustix::process::Signal::KILL);
            // Reaped aside: the kernel takes a while to free the child's memory.
            std::thread::spawn(move || rustix::process::waitpid(Some(child), WaitOptions::empty()));
        }
    }
}

/// Loads the models in the background and transcribes the recording. While
/// the recording goes on, it picks the language and, on the CPU, transcribes
/// ahead at every pause, so the stop waits for little or nothing.
pub struct Worker {
    shared: Arc<Shared>,
    wake: Option<mpsc::Sender<()>>,
}

impl Worker {
    pub fn spawn(cfg: Config, notifier: Notifier, recording: Tap) -> Worker {
        let shared = Arc::new(Shared::default());
        let (wake, woken) = mpsc::channel();
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("asr".into())
                .spawn(move || {
                    if let Err(e) = work(&cfg, &notifier, &recording, &shared, &woken) {
                        notifier.send(Event::Failed(e));
                    }
                })
                .expect("spawn asr thread");
        }
        Worker { shared, wake: Some(wake) }
    }

    /// The recording is over: `audio` is all of it.
    pub fn submit(&mut self, audio: Vec<f32>) {
        if let Some(wake) = self.wake.take() {
            let _ = self.shared.end.set((audio, Instant::now()));
            let _ = wake.send(());
        }
    }

    pub fn cancel(&mut self) {
        self.shared.cancelled.store(true, Ordering::Relaxed);
        self.wake = None;
    }
}

/// The worker thread: sends the text, unless the dictation is cancelled.
fn work(
    cfg: &Config,
    notifier: &Notifier,
    recording: &Tap,
    shared: &Shared,
    woken: &mpsc::Receiver<()>,
) -> Result<(), String> {
    let t = Instant::now();
    let mut models = load(cfg)?;
    log!("models loaded in {} ms", t.elapsed().as_millis());
    notifier.send(Event::ModelReady);

    // The card is fast enough to do everything after the stop.
    let mut ahead = cfg.ahead && !models.on_gpu;
    let mut flight: Option<Flight> = None;
    let mut heard = Vec::new();
    let min_part = cfg.parts.map_or(usize::MAX, |seconds| (seconds * 50.0) as usize);
    let mut draft = Draft { parts: Vec::new(), ahead: None, min_part };
    // The last pick: the frames of speech it heard, the language, its scores.
    let mut picked: Option<(usize, &str, String)> = None;
    let mut saved = false;
    loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let end = shared.end.get();
        let last = end.is_some();
        let audio: &[f32] = match end {
            Some((audio, _)) => audio,
            None => {
                recording.extend(&mut heard);
                &heard
            }
        };
        if let (true, false, Some(dir)) = (last, saved, &cfg.save_dir) {
            save(dir, audio);
            saved = true;
        }
        let rms = frame_rms(audio);
        let threshold = speech_threshold(&rms);
        let loud: Vec<bool> = rms.iter().map(|&r| r > threshold).collect();

        // The pick stands while it has heard at least 3/4 of the speech, and
        // for good once a part of the text is settled.
        if let (Some(detector), Some(speech)) = (&models.detector, spoken(&loud)) {
            let stands = picked.as_ref().is_some_and(|pick| 4 * pick.0 >= 3 * speech.end);
            if !stands && draft.parts.is_empty() {
                let speech_audio = &audio[with_margin(&speech, 0, audio.len())];
                match pick_language(detector, cfg.threads, speech_audio, &cfg.language) {
                    Ok((language, scores)) => {
                        if picked.as_ref().is_some_and(|pick| pick.1 != language) {
                            draft.ahead = None;
                            flight = None;
                        }
                        picked = Some((speech.end, language, scores));
                    }
                    Err(e) if last => return Err(e),
                    Err(e) => log!("language while recording: {e}"),
                }
            }
        }
        let language = match (&models.detector, &picked) {
            (None, _) => Some(cfg.language.as_str()),
            (Some(_), pick) => pick.as_ref().map(|pick| pick.1),
        };

        // The pass under way: stopped when the speech has outrun it.
        if let Some(pass) = &mut flight {
            if outrun(pass.speech.len(), loud_after(&loud, pass.speech.end), last, min_part) {
                log!(
                    "{:.1} s of speech: pass stopped after {} ms, more was said",
                    pass.seconds,
                    pass.started.elapsed().as_millis()
                );
                flight = None;
            } else if pass.started.elapsed() > STUCK + Duration::from_secs_f32(3.0 * pass.seconds) {
                log!("{:.1} s of speech: pass stuck, the rest waits for the stop", pass.seconds);
                ahead = false;
                flight = None;
            } else {
                match pass.land(TICK / 2) {
                    None => continue,
                    Some(Some(text)) => {
                        log!(
                            "{:.1} s of speech transcribed in {} ms, started while recording",
                            pass.seconds,
                            pass.started.elapsed().as_millis()
                        );
                        draft.ahead = Some(Part { speech: pass.speech.clone(), text });
                    }
                    Some(None) => {
                        log!("a pass while recording failed, the rest waits for the stop");
                        ahead = false;
                    }
                }
                flight = None;
            }
        }

        let next =
            if ahead || last { language.and_then(|language| Some((draft.next(&loud, last)?, language))) } else { None };
        let Some((speech, language)) = next else {
            if last {
                break;
            }
            if woken.recv_timeout(TICK) == Err(RecvTimeoutError::Disconnected) && shared.end.get().is_none() {
                return Ok(());
            }
            continue;
        };
        let floor = draft.parts.last().map_or(0, |part| part.speech.end);
        let samples = &audio[with_margin(&speech, floor, audio.len())];
        if !last {
            match Flight::start(&models, cfg, samples, language, speech) {
                Ok(pass) => flight = Some(pass),
                Err(e) => {
                    log!("no pass while recording, the rest waits for the stop: {e}");
                    ahead = false;
                }
            }
            continue;
        }
        let t = Instant::now();
        let mut result = decode(&models, cfg, samples, language);
        if let (Err(e), true) = (&result, models.on_gpu) {
            log!("the graphics card failed, transcribing on the CPU: {e}");
            drop(models);
            models = open(cfg, None)?;
            result = decode(&models, cfg, samples, language);
        }
        let text = result?;
        log!("{:.1} s of speech transcribed in {} ms", samples.len() as f32 / RATE as f32, t.elapsed().as_millis());
        draft.parts.push(Part { speech, text });
    }
    if let Some((_, language, scores)) = &picked {
        log!("language {language} ({scores})");
    }
    if let Some((audio, stopped)) = shared.end.get() {
        log!(
            "{:.1} s of audio, text ready {} ms after the stop, on the {}",
            audio.len() as f32 / RATE as f32,
            stopped.elapsed().as_millis(),
            if models.on_gpu { "graphics card" } else { "CPU" }
        );
    }
    // Sent before the models go: giving back their memory takes a while.
    notifier.send(Event::Text(draft.text()));
    Ok(())
}

/// How often the worker looks at the recording.
const TICK: Duration = Duration::from_millis(50);
/// A pass made while recording that takes this long, plus three times its
/// speech, is taken for stuck: its process is a copy of a threaded one and
/// would wait forever for a lock another thread held at that moment.
const STUCK: Duration = Duration::from_secs(30);

fn save(dir: &std::path::Path, audio: &[f32]) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let path = dir.join(format!("{stamp}.wav"));
    if let Err(e) = std::fs::create_dir_all(dir).and_then(|_| crate::wav::write(&path, audio)) {
        log!("cannot save {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::{Draft, Part, choose_language, collapse_repeats};

    /// Frames: `(true, 100)` is 2 s of speech, `(false, 20)` 0.4 s of silence.
    fn frames(runs: &[(bool, usize)]) -> Vec<bool> {
        runs.iter().flat_map(|&(loud, n)| std::iter::repeat_n(loud, n)).collect()
    }

    fn part(speech: std::ops::Range<usize>) -> Option<Part> {
        Some(Part { speech, text: String::new() })
    }

    /// `min_part`: frames, `usize::MAX` when no text is settled at pauses.
    fn draft(min_part: usize) -> Draft {
        Draft { parts: Vec::new(), ahead: None, min_part }
    }

    fn spans(draft: &Draft) -> Vec<std::ops::Range<usize>> {
        draft.parts.iter().map(|part| part.speech.clone()).collect()
    }

    #[test]
    fn a_pause_starts_a_pass_and_its_text_stands_if_nothing_follows() {
        let mut draft = draft(usize::MAX);
        assert_eq!(draft.next(&frames(&[(false, 10), (true, 100), (false, 19)]), false), None);
        assert_eq!(draft.next(&frames(&[(false, 10), (true, 100), (false, 20)]), false), Some(10..110));
        draft.ahead = part(10..110);
        assert_eq!(draft.next(&frames(&[(false, 10), (true, 100), (false, 40)]), false), None);
        // A click of the key is not speech.
        assert_eq!(draft.next(&frames(&[(false, 10), (true, 100), (false, 40), (true, 3), (false, 9)]), true), None);
        assert_eq!(spans(&draft), [10..110]);
    }

    #[test]
    fn short_speech_is_transcribed_again_with_what_follows() {
        for min_part in [300, usize::MAX] {
            let mut draft = draft(min_part);
            draft.ahead = part(0..100);
            let loud = frames(&[(true, 100), (false, 25), (true, 100), (false, 20)]);
            assert_eq!(draft.next(&loud, false), Some(0..225));
            assert!(draft.parts.is_empty());
        }
    }

    #[test]
    fn nothing_is_settled_at_pauses_unless_asked() {
        let mut draft = draft(usize::MAX);
        draft.ahead = part(0..400);
        let loud = frames(&[(true, 400), (false, 25), (true, 100), (false, 20)]);
        assert_eq!(draft.next(&loud, false), Some(0..525));
        draft.ahead = part(0..525);
        assert_eq!(
            draft.next(&frames(&[(true, 400), (false, 25), (true, 100), (false, 60), (true, 30)]), true),
            Some(0..615)
        );
        assert!(draft.parts.is_empty());
    }

    #[test]
    fn long_speech_keeps_its_text_when_more_follows() {
        let mut draft = draft(300);
        draft.ahead = part(0..400);
        let mut loud = frames(&[(true, 400), (false, 25), (true, 100), (false, 10)]);
        assert_eq!(draft.next(&loud, false), None); // no pause yet
        assert_eq!(spans(&draft), [0..400]);
        loud.extend([false; 10]);
        assert_eq!(draft.next(&loud, false), Some(425..525));
        draft.ahead = part(425..525);
        assert_eq!(draft.next(&loud, true), None);
        assert_eq!(spans(&draft), [0..400, 425..525]);
    }

    #[test]
    fn a_few_last_words_join_the_part_before_them() {
        let loud = frames(&[(true, 400), (false, 30), (true, 20), (false, 30)]);
        let mut settled = draft(300);
        settled.parts.extend(part(0..400));
        assert_eq!(settled.next(&loud, true), Some(0..450));
        assert!(settled.parts.is_empty());
        let mut in_flight = draft(300);
        in_flight.ahead = part(0..400);
        assert_eq!(in_flight.next(&loud, true), Some(0..450));
    }

    #[test]
    fn the_end_without_a_pause_is_transcribed_after_the_stop() {
        let mut draft = draft(usize::MAX);
        let loud = frames(&[(false, 5), (true, 100), (false, 12)]);
        assert_eq!(draft.next(&loud, false), None);
        assert_eq!(draft.next(&loud, true), Some(5..105));
        assert_eq!(draft.next(&frames(&[(false, 200)]), true), None);
    }

    #[test]
    fn repeats_collapse_to_the_first_copy() {
        let looped = "Вот так, но на деле это... Но на деле это... Но на деле это... и всё.";
        assert_eq!(collapse_repeats(looped), "Вот так, но на деле это... и всё.");
        let twice = "Он пришёл домой поздно вечером. Он пришёл домой поздно вечером.";
        assert_eq!(collapse_repeats(twice), "Он пришёл домой поздно вечером.");
    }

    #[test]
    fn ordinary_repeats_stay() {
        for text in
            ["да, да, да, конечно", "я не знаю, я не знаю", "очень очень очень хорошо", "раз два три, раз два три"]
        {
            assert_eq!(collapse_repeats(text), text);
        }
    }

    #[test]
    fn first_language_wins_unless_another_is_likely() {
        assert_eq!(choose_language(&[("ru", 0.01), ("en", 0.02)]), "ru");
        assert_eq!(choose_language(&[("ru", 0.30), ("en", 0.45)]), "ru");
        assert_eq!(choose_language(&[("ru", 0.02), ("en", 0.95)]), "en");
        assert_eq!(choose_language(&[("ru", 0.98), ("en", 0.00)]), "ru");
    }
}
