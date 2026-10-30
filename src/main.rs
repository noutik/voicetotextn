//! voicetotextn: press Super+A to start dictating, press it again to paste the text.
//!
//! Every key press spawns this binary. The first instance becomes the session
//! (records, shows the overlay, transcribes, pastes); a later instance only
//! pokes the running session over a unix socket and exits.

#[macro_use]
mod log;
mod asr;
mod audio;
mod demo;
mod ipc;
mod overlay;
mod paste;
mod render;
mod scene;
mod spectrum;
mod wav;

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

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
    wake: std::sync::Arc<std::os::fd::OwnedFd>,
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
        Some("transcribe") if args.len() > 1 => transcribe_files(&args[1..]),
        Some("demo") if args.len() == 5 => demo(&args[1..]),
        _ => {
            eprintln!("usage: voicetotextn                             start or stop dictation");
            eprintln!("       voicetotextn transcribe FILE…            transcribe 16 kHz WAV files and print timings");
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
    let notifier = Notifier { tx, wake: std::sync::Arc::new(wake_w) };

    // Start the microphone first so no words are lost while the model loads.
    let recorder = audio::Recorder::start(notifier.clone());
    let worker = asr::Worker::spawn(asr::Config::from_env(), notifier);

    match overlay::run(listener, recorder, worker, rx, wake_r) {
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
    println!("model {} loaded in {:.2} s, {} threads", cfg.model.display(), t.elapsed().as_secs_f32(), cfg.threads);
    for file in files {
        let audio = match wav::read(file.as_ref()) {
            Ok(audio) => audio,
            Err(e) => {
                eprintln!("{file}: {e}");
                continue;
            }
        };
        let t = Instant::now();
        match asr::transcribe(&models, &cfg, &audio) {
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
