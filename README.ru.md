# voicetotextn

Голосовой ввод для Wayland. Нажми **Super+A**, говори, нажми **Super+A** ещё раз — текст появится в поле ввода, где стоит курсор. Всё работает без интернета, на процессоре, через [whisper.cpp](https://github.com/ggml-org/whisper.cpp).

<p align="center"><img src="docs/demo.gif" alt="Плашка выезжает кругом, раскрывается, слушает голос, «дышит» во время распознавания и улетает"></p>

[English version](README.md) · [Страница проекта](https://noutik.github.io/voicetotextn/)

## Возможности

- **Одна клавиша.** Первое нажатие начинает запись и показывает сверху экрана маленький чёрно-белый эквалайзер. Второе — распознаёт речь, копирует текст в буфер обмена и вставляет его.
- **Без интернета, только CPU.** Whisper large-v3-turbo (q8_0) через whisper.cpp. Ни звук, ни текст не покидают компьютер.
- **Смесь языков.** Крошечная модель Whisper за ~0.25 с выбирает язык каждой диктовки из короткого списка (по умолчанию русский или английский). Английские слова внутри русской речи остаются латиницей: *«Запусти Docker Compose и проверь Git статус»*.
- **Ничего не висит в памяти между диктовками.** Программа запускается по нажатию и грузит модель, пока ты уже говоришь, — слова не теряются.
- **Escape отменяет** запись.
- **Вставляет почти везде.** Текст кладётся и в буфер обмена, и в «выделение» (primary selection), потом программа нажимает Shift+Insert: так вставка работает в браузерах, GTK/Qt-программах и терминалах вроде kitty.

## Скорость

Время от второго нажатия до вставленного текста, Ryzen 7 7800X3D (8 потоков):

| Речь     | Ожидание |
|----------|----------|
| 4–6 с    | ~2.3 с   |
| 11 с     | ~3 с     |
| 16 с     | ~4.5 с   |
| 25–30 с  | ~8 с     |

Около 1 ГБ памяти во время диктовки, ноль в остальное время.

Обычно Whisper всегда обрабатывает окно в 30 с, даже для фразы из двух слов. voicetotextn сужает окно до длины речи, но не меньше 10 с: на тестах окна 5–7 с начинали задваивать и искажать слова, а с 10 с текст совпал с полным окном.

## Что нужно

- Linux с Wayland-композитором, который поддерживает **wlr-layer-shell**, **wlr-data-control** и **virtual-keyboard-unstable-v1**. Разработано и проверено на [niri](https://github.com/YaLTeR/niri); Sway, Hyprland и другие композиторы на wlroots дают те же протоколы. GNOME не поддерживается.
- PipeWire (с pipewire-pulse) или PulseAudio.
- Масштаб экрана 1: плашка рисуется в 1× и пока не учитывает HiDPI.

## Установка

### 1. Сборка

Нужны: Rust 1.85+, cmake, компилятор C/C++, clang (libclang), pkg-config и заголовки libpulse.

```sh
# Arch
sudo pacman -S --needed rust cmake clang pkgconf libpulse
# Debian / Ubuntu (Rust через https://rustup.rs, если в пакетах старше 1.85)
sudo apt install cmake clang libclang-dev pkg-config libpulse-dev
```

```sh
git clone https://github.com/noutik/voicetotextn
cd voicetotextn
cargo build --release
install -Dm755 target/release/voicetotextn ~/.local/bin/voicetotextn
```

whisper.cpp собирается с `-march=native`, то есть под процессор, на котором идёт сборка. Готовая сборка — на странице [Releases](https://github.com/noutik/voicetotextn/releases): x86-64 с AVX2, glibc 2.43+ (Arch и другие «плавающие» дистрибутивы); на остальных собирай из исходников.

### 2. Модели

```sh
mkdir -p ~/.local/share/voicetotextn && cd ~/.local/share/voicetotextn
curl -LO https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q8_0.bin  # 874 МБ
curl -LO https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin                 # 78 МБ
```

### 3. Горячая клавиша

niri (`~/.config/niri/config.kdl`, внутри `binds`):

```kdl
Mod+A hotkey-overlay-title="Voice to text" { spawn "voicetotextn"; }
```

Sway: `bindsym $mod+a exec voicetotextn` · Hyprland: `bind = SUPER, A, exec, voicetotextn`

Если `~/.local/bin` нет в `PATH` композитора, укажи полный путь (`/home/ты/.local/bin/voicetotextn`).

## Как пользоваться

| Клавиша | Действие |
|---------|----------|
| Super+A | начать запись — говорить можно сразу |
| Super+A ещё раз | распознать и вставить |
| Escape (во время записи) | отменить, ничего не вставится |

Во время записи плашка держит клавиатуру, чтобы поймать Escape. После второго нажатия фокус возвращается в твоё окно, и текст вставляется туда.

## Настройки

Переменные окружения, например в niri: `spawn "env" "VOICETOTEXTN_LANG=de,en" "voicetotextn";`

| Переменная | По умолчанию | Что делает |
|------------|--------------|------------|
| `VOICETOTEXTN_LANG` | `ru,en` | Список через запятую: маленькая модель выбирает один язык на диктовку. Один код (`ru`) — язык всегда один. `auto` — Whisper сам определяет любой язык, но это лишний полный проход большой модели (примерно в 2 раза медленнее). |
| `VOICETOTEXTN_MODEL` | `~/.local/share/voicetotextn/ggml-large-v3-turbo-q8_0.bin` | Основная модель. |
| `VOICETOTEXTN_DETECTOR` | `~/.local/share/voicetotextn/ggml-tiny.bin` | Модель для выбора языка. |
| `VOICETOTEXTN_MIN_WINDOW` | `10` | Наименьшее окно в секундах; `30` — полное окно Whisper. |
| `VOICETOTEXTN_THREADS` | число физических ядер | Потоки whisper.cpp. |
| `VOICETOTEXTN_PROMPT` | — | Подсказка (initial prompt) для Whisper. |
| `VOICETOTEXTN_FLASH` | `1` | `0` выключает flash attention. |
| `VOICETOTEXTN_SAVE_DIR` | — | Сохранять каждую запись туда в WAV (для тестов). |
| `VOICETOTEXTN_DRY_RUN` | — | Распознать, но не вставлять. |

## Как устроено

- Каждое нажатие запускает программу. Первый экземпляр становится сессией и слушает абстрактный unix-сокет; следующий только подключается к нему («переключить») и выходит.
- Звук: 16 кГц моно с микрофона по умолчанию (PulseAudio/PipeWire) — ровно то, с чем работает Whisper.
- Плашка: поверхность layer-shell на верхнем слое, рисуется программно (сглаженные фигуры) в общую память. Столбики — спектр по FFT на 1024 точки, низкие частоты слева.
- Распознавание: тишина в начале и в конце обрезается, маленькая модель выбирает язык, turbo-модель распознаёт.
- Вставка: wlr-data-control кладёт текст в буфер обмена и в «выделение» (фокус клавиатуры не нужен), виртуальная клавиатура нажимает Shift+Insert. Процесс продолжает отдавать текст (несколько МБ памяти), пока другая программа не займёт буфер.
- Журнал: `~/.local/state/voicetotextn.log` (только тайминги, распознанный текст не пишется).

Инструменты для разработки: `voicetotextn transcribe FILE.wav…` распознаёт WAV 16 кГц и печатает время; `voicetotextn demo WAV START SECONDS DIR` рисует кадры анимации, что выше.

## Лицензия

[MIT](LICENSE). Использует whisper.cpp (MIT) через [whisper-rs](https://codeberg.org/tazz4843/whisper-rs) и модели OpenAI Whisper (MIT).
