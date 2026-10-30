# voicetotextn

Push-to-talk dictation for Wayland. Press **Super+A**, speak, press **Super+A** again — the text appears in the input field you are typing in. Everything runs offline on the CPU with [whisper.cpp](https://github.com/ggml-org/whisper.cpp).

<p align="center"><img src="docs/demo.gif" alt="The overlay slides down as a circle, unfolds, follows the voice, breathes while transcribing and flies away"></p>

[Русская версия](README.ru.md) · [Project page](https://noutik.github.io/voicetotextn/)

## Features

- **One hotkey.** The first press starts recording and shows a small black-and-white equalizer at the top of the screen. The second press transcribes, copies the text to the clipboard and pastes it.
- **Offline, CPU only.** Whisper large-v3-turbo (q8_0) through whisper.cpp. No audio or text leaves your computer.
- **Mixed languages.** A tiny Whisper model picks the language of every dictation from a short list (Russian or English by default) in about 0.25 s. English terms inside Russian speech stay in Latin script: *«Запусти Docker Compose и проверь Git статус»*.
- **Nothing in memory between dictations.** The program starts on the key press and loads the model while you are already speaking, so no words are lost.
- **Escape cancels** the recording.
- **Pastes almost everywhere.** The text goes into both the clipboard and the primary selection, then the program presses Shift+Insert: that pastes in browsers, GTK/Qt apps and terminals such as kitty.

## Speed

Time from the second key press to the pasted text, Ryzen 7 7800X3D (8 threads):

| Speech   | Wait    |
|----------|---------|
| 4–6 s    | ~2.3 s  |
| 11 s     | ~3 s    |
| 16 s     | ~4.5 s  |
| 25–30 s  | ~8 s    |

About 1 GB of RAM while dictating, nothing when idle.

Whisper normally encodes a fixed 30 s window, even for a two-word phrase. voicetotextn shrinks the window to the length of the speech but keeps at least 10 s: in tests, windows of 5–7 s started to duplicate or misspell words, 10 s gave the same text as the full window.

## Requirements

- Linux with a Wayland compositor that supports **wlr-layer-shell**, **wlr-data-control** and **virtual-keyboard-unstable-v1**. Developed and tested on [niri](https://github.com/YaLTeR/niri); Sway, Hyprland and other wlroots-based compositors provide the same protocols. GNOME is not supported.
- PipeWire (with pipewire-pulse) or PulseAudio.
- Output scale 1: the overlay is drawn at 1× and is not HiDPI-aware yet.

## Install

### 1. Build

Build dependencies: Rust 1.85+, cmake, a C/C++ compiler, clang (libclang), pkg-config and the libpulse headers.

```sh
# Arch
sudo pacman -S --needed rust cmake clang pkgconf libpulse
# Debian / Ubuntu (Rust via https://rustup.rs if the packaged one is older than 1.85)
sudo apt install cmake clang libclang-dev pkg-config libpulse-dev
```

```sh
git clone https://github.com/noutik/voicetotextn
cd voicetotextn
cargo build --release
install -Dm755 target/release/voicetotextn ~/.local/bin/voicetotextn
```

whisper.cpp is compiled with `-march=native`, so the binary is tuned for the CPU it was built on. A prebuilt binary is on the [Releases](https://github.com/noutik/voicetotextn/releases) page: x86-64 with AVX2, glibc 2.43+ (Arch and other rolling distributions); elsewhere build from source.

### 2. Models

```sh
mkdir -p ~/.local/share/voicetotextn && cd ~/.local/share/voicetotextn
curl -LO https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin  # 874 MB
curl -LO https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin                 # 78 MB
```

### 3. Hotkey

niri (`~/.config/niri/config.kdl`, inside `binds`):

```kdl
Mod+A hotkey-overlay-title="Voice to text" { spawn "voicetotextn"; }
```

Sway: `bindsym $mod+a exec voicetotextn` · Hyprland: `bind = SUPER, A, exec, voicetotextn`

Use the full path (`/home/you/.local/bin/voicetotextn`) if `~/.local/bin` is not in the compositor's `PATH`.

## Usage

| Key | Action |
|-----|--------|
| Super+A | start recording — you can speak right away |
| Super+A again | transcribe and paste |
| Escape (while recording) | cancel, nothing is pasted |

While recording, the overlay holds the keyboard so it can see Escape. After the second press the focus returns to your window and the text is pasted there.

## Configuration

Environment variables, for example in niri: `spawn "env" "VOICETOTEXTN_LANG=de,en" "voicetotextn";`

| Variable | Default | Meaning |
|----------|---------|---------|
| `VOICETOTEXTN_LANG` | `ru,en` | Comma-separated list: the tiny model picks one per dictation. A single code (`en`) fixes the language. `auto` lets Whisper detect any language, but costs a second full pass of the big model (about 2× slower). |
| `VOICETOTEXTN_MODEL` | `~/.local/share/voicetotextn/ggml-large-v3-turbo-q8_0.bin` | Main model. |
| `VOICETOTEXTN_DETECTOR` | `~/.local/share/voicetotextn/ggml-tiny.bin` | Language detector model. |
| `VOICETOTEXTN_MIN_WINDOW` | `10` | Shortest encoder window in seconds; `30` = Whisper's full window. |
| `VOICETOTEXTN_THREADS` | physical cores | whisper.cpp threads. |
| `VOICETOTEXTN_PROMPT` | — | Initial prompt for Whisper. |
| `VOICETOTEXTN_FLASH` | `1` | `0` turns flash attention off. |
| `VOICETOTEXTN_SAVE_DIR` | — | Save every recording there as WAV (for tests). |
| `VOICETOTEXTN_DRY_RUN` | — | Transcribe but do not paste. |

## How it works

- Every key press runs the binary. The first instance becomes the session and listens on an abstract unix socket; a later one only connects to it ("toggle") and exits.
- Audio: 16 kHz mono from the default PulseAudio/PipeWire source — the rate Whisper works at.
- Overlay: a layer-shell surface on the overlay layer, drawn in software (anti-aliased shapes) into shared memory. The bars are a 1024-point FFT spectrum, low frequencies on the left.
- Recognition: leading and trailing silence is trimmed, the tiny model picks the language, the turbo model transcribes.
- Paste: wlr-data-control sets the clipboard and the primary selection (no keyboard focus needed), then a virtual keyboard sends Shift+Insert. The process keeps serving the text (a few MB) until another program takes over the selection.
- Log: `~/.local/state/voicetotextn.log` (timings only, the recognized text is not logged).

Developer tools: `voicetotextn transcribe FILE.wav…` transcribes 16 kHz WAV files and prints timings; `voicetotextn demo WAV START SECONDS DIR` renders the animation above as frames.

## License

[MIT](LICENSE). Uses whisper.cpp (MIT) through [whisper-rs](https://codeberg.org/tazz4843/whisper-rs) and OpenAI Whisper models (MIT).
