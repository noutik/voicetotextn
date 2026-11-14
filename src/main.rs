//! voicetotextn: press Super+A to start dictating, press it again to paste the text.
//!
//! Every key press spawns this binary. The first instance becomes the session
//! (records, shows the overlay, transcribes, pastes); a later instance only
//! pokes the running session over a unix socket and exits.

#[macro_use]
mod log;
mod asr;
mod audio;
mod bf16;
mod demo;
mod gpu;
mod ipc;
mod overlay;
mod paste;
mod render;
mod scene;
mod spectrum;
mod wav;

use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Messages from the worker threads to the main (Wayland) thread.
pub enum Event {
    ModelReady,
    Text(String),
    Failed(String),
}

/// Sends events to the main thread and wakes its poll loop through a pipe.
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::Sender<Event>,
    wake: Arc<std::os::fd::OwnedFd>,
}

impl Notifier {
    pub fn send(&self, event: Event) {
        if self.tx.send(event).is_ok() {
            let _ = rustix::io::write(&*self.wake, &[1]);
        }
    }
}

fn main() -> ExitCode {
    log::start();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => session(),
        Some("transcribe") if args.len() > 2 && args[1] == "--live" => dictate_files(&args[2..]),
        Some("transcribe") if args.len() > 1 => transcribe_files(&args[1..]),
        Some("demo") if args.len() == 5 => demo(&args[1..]),
        _ => {
            eprintln!("usage: voicetotextn                             start or stop dictation");
            eprintln!("       voicetotextn transcribe FILE…            transcribe 16 kHz WAV files and print timings");
            eprintln!(
                "       voicetotextn transcribe --live FILE…     the same as a dictation: each file plays at the speed of speech"
            );
            eprintln!("       voicetotextn demo WAV START SECONDS DIR  render the overlay animation as PPM frames");
            ExitCode::FAILURE
        }
    }
}

fn session() -> ExitCode {
    let listener = match ipc::claim() {
        Ok(Some(listener)) => listener,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            log!("cannot open the control socket: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (tx, rx) = mpsc::channel();
    let (wake_r, wake_w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK)
        .expect("pipe");
    let notifier = Notifier { tx, wake: Arc::new(wake_w) };

    // Start the microphone first so no words are lost while the model loads.
    let recorder = audio::Recorder::start(notifier.clone());
    let worker = asr::Worker::spawn(asr::Config::from_env(), notifier.clone(), recorder.tap());

    let result = overlay::run(listener, recorder, worker, rx, wake_r);
    // Kept until here: once the worker threads finish, a pipe with no writer
    // left would poll as hung up forever, and the session, which lingers to
    // serve the clipboard, would spin on a whole core.
    drop(notifier);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log!("overlay: {e}");
            ExitCode::FAILURE
        }
    }
}

fn transcribe_files(files: &[String]) -> ExitCode {
    let cfg = asr::Config::from_env();
    let t = Instant::now();
    let models = match asr::load(&cfg) {
        Ok(models) => models,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "model {} loaded in {:.2} s, {}",
        cfg.model.display(),
        t.elapsed().as_secs_f32(),
        if models.on_gpu { "graphics card".to_string() } else { format!("CPU, {} threads", cfg.threads) }
    );
    for file in files {
        let audio = match wav::read(file.as_ref()) {
            Ok(audio) => audio,
            Err(e) => {
                eprintln!("{file}: {e}");
                continue;
            }
        };
        // Before the clock starts: the session picks the language while the user speaks.
        let language = match asr::pick(&models, &cfg, &audio) {
            Ok(language) => language.map(|(language, _)| language),
            Err(e) => {
                eprintln!("{file}: {e}");
                continue;
            }
        };
        let t = Instant::now();
        match asr::transcribe(&models, &cfg, &audio, language) {
            Ok(text) => println!(
                "{file} ({:.1} s audio, {:.2} s): {text}",
                audio.len() as f32 / audio::RATE as f32,
                t.elapsed().as_secs_f32()
            ),
            Err(e) => eprintln!("{file}: {e}"),
        }
    }
    ExitCode::SUCCESS
}

/// Plays every file to a worker at the speed of speech, as the microphone
/// would, and prints how long the text took after the end of the file.
fn dictate_files(files: &[String]) -> ExitCode {
    for file in files {
        let audio = match wav::read(file.as_ref()) {
            Ok(audio) => audio,
            Err(e) => {
                eprintln!("{file}: {e}");
                continue;
            }
        };
        let (tx, rx) = mpsc::channel();
        let (_wake_r, wake_w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK).expect("pipe");
        let notifier = Notifier { tx, wake: Arc::new(wake_w) };
        let recording = Arc::new(Mutex::new(Vec::new()));
        let mut worker = asr::Worker::spawn(asr::Config::from_env(), notifier, audio::Tap(recording.clone()));
        let start = Instant::now();
        let mut played = 0;
        for chunk in audio.chunks(256) {
            played += chunk.len();
            let due = start + Duration::from_secs_f64(played as f64 / audio::RATE as f64);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            recording.lock().unwrap().extend_from_slice(chunk);
        }
        let t = Instant::now();
        let seconds = audio.len() as f32 / audio::RATE as f32;
        worker.submit(audio);
        loop {
            match rx.recv() {
                Ok(Event::ModelReady) => {}
                Ok(Event::Text(text)) => {
                    break println!("{file} ({seconds:.1} s audio, {:.2} s): {text}", t.elapsed().as_secs_f32());
                }
                Ok(Event::Failed(e)) => break eprintln!("{file}: {e}"),
                Err(_) => break,
            }
        }
    }
    ExitCode::SUCCESS
}

fn demo(args: &[String]) -> ExitCode {
    let (Ok(start), Ok(seconds)) = (args[1].parse(), args[2].parse()) else {
        eprintln!("START and SECONDS must be numbers");
        return ExitCode::FAILURE;
    };
    match demo::render(args[0].as_ref(), start, seconds, args[3].as_ref()) {
        Ok(frames) => {
            println!("{frames} frames in {}", args[3]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
