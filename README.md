# voicetotextn

Push-to-talk dictation for Wayland. Press **Super+A**, speak, press **Super+A** again — the text appears in the input field you are typing in. Everything runs offline, on the graphics card or the CPU, with [whisper.cpp](https://github.com/ggml-org/whisper.cpp).

<p align="center"><img src="docs/demo.gif" alt="The overlay slides down as a circle, unfolds, follows the voice, breathes while transcribing and flies away"></p>

[Русская версия](README.ru.md) · [Project page](https://noutik.github.io/voicetotextn/)

## Features

- **One hotkey.** The first press starts recording and shows a small black-and-white equalizer at the top of the screen. The second press transcribes, copies the text to the clipboard and pastes it.
- **Offline.** Whisper large-v3-turbo (q8_0) through whisper.cpp, on the graphics card (Vulkan) when it is free and on the CPU otherwise. No audio or text leaves your computer.
- **Mixed languages.** A tiny Whisper model picks the language of every dictation from a short list (Russian or English by default) while you speak; the first language of the list wins unless the model clearly hears another. English terms inside Russian speech stay in Latin script: *«Запусти Docker Compose и проверь Git статус»*.
- **Nothing in memory between dictations.** The program starts on the key press and loads the model while you are already speaking, so no words are lost.
- **Escape cancels** the recording.
- **Pastes almost everywhere.** The text goes into both the clipboard and the primary selection, then the program presses Shift+Insert: that pastes in browsers, GTK/Qt apps and terminals such as kitty.

## Speed

Transcription time (`voicetotextn transcribe`), Radeon RX 9070 XT and Ryzen 7 7800X3D (8 threads). The microphone stays open for 0.25 s after the second key press to catch the end of the last word, so on the graphics card the wait is that much longer. On the CPU the transcription starts at a pause, before the key, and the wait is usually shorter than in the table (see below).

| Speech | Graphics card | CPU     |
|--------|---------------|---------|
| 5 s    | 0.2–0.4 s     | ~1.1 s  |
| 10 s   | 0.2–0.4 s     | ~1.9 s  |
| 13 s   | 0.2–0.4 s     | ~2.6 s  |
| 20 s   | 0.2–0.4 s     | ~4.1 s  |
| 27 s   | 0.2–0.4 s     | ~6.2 s  |

On the graphics card: about 1.2 GB of video memory while dictating. On the CPU: about 1.9 GB of RAM (1.1 GB with `VOICETOTEXTN_BF16=0`). Nothing when idle.

On the CPU the transcription does not wait for the key: it starts after 0.4 s of silence, so a pause before the second key press comes off the wait. Played back as dictations (`transcribe --live`), 15 test recordings of 14–30 s gave the same text as before and waited 3.4 s on average instead of 3.8 s: about 1 s less after a pause of over a second, nothing at all after a pause of 4 s, the same without a pause.

`VOICETOTEXTN_PARTS=10` goes further: for speech of 10 s or more the text made at a pause is kept even when you go on speaking, and after the stop only the last part is transcribed. The average wait on the same recordings fell to 1.8 s. But the parts are transcribed without each other's context, and 11.7% of the words differed from the full-window text instead of 2.9%, mostly false starts that Whisper leaves out when it hears what follows. That is why it is off by default.

Whisper normally encodes a fixed 30 s window, even for a two-word phrase. The graphics card does exactly that: the full window costs it a fraction of a second and gives the best text. On the CPU the full window takes 5–6 s, so voicetotextn shrinks it to the speech plus 3 s of silence, but not below 10 s. That costs accuracy: on 483 test clips the text more often differed from the full-window one the shorter the window was, and with only 1 s of silence Whisper appended invented or repeated words to 15% of the clips.

## Requirements

- Linux with a Wayland compositor that supports **wlr-layer-shell**, **wlr-data-control** and **virtual-keyboard-unstable-v1**. Developed and tested on [niri](https://github.com/YaLTeR/niri); Sway, Hyprland and other wlroots-based compositors provide the same protocols. GNOME is not supported.
- PipeWire (with pipewire-pulse) or PulseAudio.
- Output scale 1: the overlay is drawn at 1× and is not HiDPI-aware yet.

## Install

### 1. Build

Build dependencies: Rust 1.85+, cmake, a C/C++ compiler, clang (libclang), pkg-config, the libpulse headers and, for the graphics card, the Vulkan headers and the `glslc` shader compiler.

```sh
# Arch
sudo pacman -S --needed rust cmake clang pkgconf libpulse vulkan-headers vulkan-icd-loader shaderc
# Debian / Ubuntu (Rust via https://rustup.rs if the packaged one is older than 1.85)
sudo apt install cmake clang libclang-dev pkg-config libpulse-dev libvulkan-dev glslc
```

```sh
git clone https://github.com/noutik/voicetotextn
cd voicetotextn
cargo build --release
install -Dm755 target/release/voicetotextn ~/.local/bin/voicetotextn
```

To build without the graphics card support (no Vulkan needed to build or to run, always the CPU): `cargo build --release --no-default-features`.

whisper.cpp is compiled with `-march=native`, so the binary is tuned for the CPU it was built on. Prebuilt CPU-only binaries are on the [Releases](https://github.com/noutik/voicetotextn/releases) page, both for glibc 2.43+ (Arch and other rolling distributions): `x86_64-linux` for any x86-64 CPU with AVX2, and `x86_64-avx512bf16-linux` for CPUs with AVX512-BF16 (AMD Zen 4 and newer, Intel Xeon since Sapphire Rapids), about 1.4× faster than the first one. Elsewhere build from source.

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
| `VOICETOTEXTN_LANG` | `ru,en` | Comma-separated list: the tiny model picks one per dictation, the first unless it gives another at least 0.5. A single code (`en`) fixes the language. `auto` lets Whisper detect any language, but costs a second full pass of the big model (about 2× slower). |
| `VOICETOTEXTN_MODEL` | `~/.local/share/voicetotextn/ggml-large-v3-turbo-q8_0.bin` | Main model. |
| `VOICETOTEXTN_DETECTOR` | `~/.local/share/voicetotextn/ggml-tiny.bin` | Language detector model. |
| `VOICETOTEXTN_GPU` | `auto` | `auto`: the graphics card when it is free, the CPU otherwise. `0`: always the CPU. `1`: the card whatever its load. |
| `VOICETOTEXTN_MIN_WINDOW` | `10` | Shortest encoder window on the CPU in seconds; `30` = Whisper's full window. |
| `VOICETOTEXTN_AHEAD` | `1` | On the CPU the transcription starts at every pause of 0.4 s, and its text is used if nothing more is said before the stop. `0`: it starts only after the stop. |
| `VOICETOTEXTN_PARTS` | — | On the CPU, the seconds of speech (for example `10`) from which the text made at a pause is kept when you go on speaking, so the stop waits only for the last part. Faster for long dictations, less accurate: see [Speed](#speed). |
| `VOICETOTEXTN_THREADS` | physical cores | whisper.cpp threads. |
| `VOICETOTEXTN_PROMPT` | — | Initial prompt for Whisper. |
| `VOICETOTEXTN_FLASH` | `1` | `0` turns flash attention off. |
| `VOICETOTEXTN_BF16` | `1` | `0` runs the turbo model on the CPU as it is, without the BF16 copy: 1.1 GB of RAM instead of 1.9 GB, slower. |
| `VOICETOTEXTN_SAVE_DIR` | — | Save every recording there as WAV (for tests). |
| `VOICETOTEXTN_DRY_RUN` | — | Transcribe but do not paste. |

## How it works

- Every key press runs the binary. The first instance becomes the session and listens on an abstract unix socket; a later one only connects to it ("toggle") and exits.
- Audio: 16 kHz mono from the default PulseAudio/PipeWire source — the rate Whisper works at.
- Overlay: a layer-shell surface on the overlay layer, drawn in software (anti-aliased shapes) into shared memory. The bars are a 1024-point FFT spectrum, low frequencies on the left.
- Recognition: while you speak, the tiny model picks the language of what has been recorded so far. After the second press the microphone stays open for another 0.25 s (the key usually lands on the tail of the last word), leading and trailing silence is trimmed and the turbo model transcribes. On the CPU it does not wait for the key: after 0.4 s of silence it transcribes what has been said so far. If you go on speaking, that pass is stopped and its text dropped; if you do not, the text is the same as it would be after the stop, only sooner. The pass runs in a forked copy of the process, because whisper.cpp cannot stop in the middle of its encoder, and a process can be killed at once. Recordings longer than 30 s are decoded with timestamps, so no words are lost between Whisper's 30 s windows. A phrase that Whisper repeats over and over (three or more words three times in a row, or five or more twice) is kept once.
- Graphics card: a discrete card is used through Vulkan when it has at least 2 GB of free memory and is less than 90% busy (the load is known only for AMD cards, from amdgpu); otherwise, or if the card fails, the CPU does the work. The models take about 1.2 GB of video memory while dictating.
- BF16: on CPUs with AVX512-BF16 (AMD Zen 4 and newer, Intel Xeon since Sapphire Rapids) whisper.cpp multiplies BF16 matrices faster than Q8_0 ones (the encoder takes 1.25–1.4× less time on Zen 4), so the first run converts the turbo model to BF16 into `~/.cache/voicetotextn` (1.6 GB, about a second). Of the prebuilt binaries only `x86_64-avx512bf16-linux` has these kernels; the AVX2 one runs the models as they are.
- whisper.cpp is built with llamafile's matrix kernels (`GGML_LLAMAFILE`, set in `.cargo/config.toml`).
- Paste: wlr-data-control sets the clipboard and the primary selection (no keyboard focus needed), then a virtual keyboard sends Shift+Insert. The process keeps serving the text (a few MB) until another program takes over the selection.
- Log: `~/.local/state/voicetotextn.log` (timings only, the recognized text is not logged).

Developer tools: `voicetotextn transcribe FILE.wav…` transcribes 16 kHz WAV files and prints timings (the language is picked before the clock starts, as the session does while you speak); with `--live` before the files it plays them at the speed of speech, as a dictation goes, and prints the wait after the end; `voicetotextn demo WAV START SECONDS DIR` renders the animation above as frames.

## License

[MIT](LICENSE). Uses whisper.cpp (MIT) through [whisper-rs](https://codeberg.org/tazz4843/whisper-rs) and OpenAI Whisper models (MIT).
