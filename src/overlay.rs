//! The equalizer overlay (a layer-shell surface at the top of the screen) and
//! the session's event loop: Wayland, toggles from the socket, worker events.

use std::error::Error;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, poll};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

use crate::asr::Worker;
use crate::audio::{RATE, Recorder};
use crate::paste::Paster;
use crate::scene::{self, Mode, Scene};
use crate::spectrum::FFT_LEN;
use crate::{Event, ipc};

/// Time for the focus to get back to the window before Shift+Insert is sent.
const PASTE_DELAY: Duration = Duration::from_millis(150);
/// Hide anyway if frame callbacks stop (e.g. the screen is off).
const HIDE_DEADLINE: Duration = Duration::from_millis(1000);

const KEY_ESC: u32 = 1;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Recording,
    Transcribing,
    Leaving(Instant),
    Gone,
}

pub struct State {
    qh: QueueHandle<State>,
    surface: Option<wl_surface::WlSurface>,
    layer: Option<ZwlrLayerSurfaceV1>,
    configured: bool,
    needs_draw: bool,
    shm: ShmBuffers,
    pub(crate) paster: Paster,

    phase: Phase,
    started: Instant,
    last_frame: Instant,
    model_ready: bool,
    keyboard_released: Option<Instant>,
    pending_text: Option<String>,
    recorder: Option<Recorder>,
    worker: Worker,
    listener: Option<UnixListener>,

    scene: Scene,
    samples: Vec<f32>,
    seen_samples: usize,
    last_analysis: Instant,
    exit: bool,
}

pub fn run(
    listener: UnixListener,
    recorder: Recorder,
    worker: Worker,
    events: Receiver<Event>,
    wake: OwnedFd,
) -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let layer_shell: ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ())?;

    let surface = compositor.create_surface(&qh, ());
    let layer = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "voicetotextn".into(),
        &qh,
        (),
    );
    layer.set_size(scene::WIDTH, scene::HEIGHT);
    layer.set_anchor(Anchor::Top);
    layer.set_exclusive_zone(-1); // from the very screen edge, over the bar
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive); // for Escape
    let region = compositor.create_region(&qh, ()); // empty: clicks pass through
    surface.set_input_region(Some(&region));
    region.destroy();
    surface.commit();

    let now = Instant::now();
    let mut state = State {
        paster: Paster::new(&globals, &qh, seat),
        shm: ShmBuffers::new(&shm, &qh, scene::WIDTH, scene::HEIGHT)?,
        qh,
        surface: Some(surface),
        layer: Some(layer),
        configured: false,
        needs_draw: false,
        phase: Phase::Recording,
        started: now,
        last_frame: now,
        model_ready: false,
        keyboard_released: None,
        pending_text: None,
        recorder: Some(recorder),
        worker,
        listener: Some(listener),
        scene: Scene::new(),
        samples: vec![0.0; FFT_LEN],
        seen_samples: 0,
        last_analysis: now,
        exit: false,
    };

    loop {
        queue.dispatch_pending(&mut state)?;
        let timeout = state.tick();
        queue.flush()?;
        if state.exit {
            return Ok(());
        }
        let Some(guard) = queue.prepare_read() else { continue };
        let (wl_ready, wake_ready, toggle_ready) = {
            let wl_fd = guard.connection_fd();
            let mut fds = vec![PollFd::new(&wl_fd, PollFlags::IN), PollFd::new(&wake, PollFlags::IN)];
            if let Some(listener) = &state.listener {
                fds.push(PollFd::new(listener, PollFlags::IN));
            }
            let timeout = timeout.map(|d| rustix::time::Timespec {
                tv_sec: d.as_secs() as i64,
                tv_nsec: d.subsec_nanos() as i64,
            });
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            let ready = |fd: &PollFd| fd.revents().intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR);
            (ready(&fds[0]), ready(&fds[1]), fds.get(2).is_some_and(ready))
        };
        if wl_ready {
            if let Err(e) = guard.read() {
                if !matches!(&e, wayland_client::backend::WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock) {
                    return Err(e.into());
                }
            }
        } else {
            drop(guard);
        }
        if wake_ready {
            let mut buf = [0u8; 64];
            while matches!(rustix::io::read(&wake, &mut buf), Ok(n) if n > 0) {}
            while let Ok(event) = events.try_recv() {
                state.on_event(event);
            }
        }
        if toggle_ready {
            for _ in 0..state.listener.as_ref().map_or(0, ipc::drain) {
                state.toggle();
            }
        }
    }
}

impl State {
    /// Second Super+A: stop recording and transcribe.
    fn toggle(&mut self) {
        if self.phase != Phase::Recording {
            return;
        }
        let audio = self.recorder.take().map(Recorder::finish).unwrap_or_default();
        log!("stop: {:.1} s recorded", audio.len() as f32 / RATE as f32);
        self.worker.submit(audio);
        self.phase = Phase::Transcribing;
        self.release_keyboard();
    }

    /// Escape: drop the recording, paste nothing.
    fn cancel(&mut self) {
        if self.phase != Phase::Recording {
            return;
        }
        log!("cancelled");
        self.recorder = None;
        self.worker.cancel();
        self.release_keyboard();
        self.leave();
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::ModelReady => self.model_ready = true,
            Event::Text(text) if self.phase == Phase::Transcribing => {
                log!("recognized {} characters", text.chars().count());
                if text.is_empty() {
                    self.leave();
                } else {
                    self.pending_text = Some(text);
                }
            }
            Event::Text(_) => {}
            Event::Failed(e) => {
                log!("error: {e}");
                if matches!(self.phase, Phase::Recording | Phase::Transcribing) {
                    self.recorder = None;
                    self.worker.cancel();
                    self.release_keyboard();
                    self.leave();
                }
            }
        }
    }

    /// Hands the keyboard back so the focus returns to the window being dictated into.
    fn release_keyboard(&mut self) {
        if let (Some(layer), Some(surface)) = (&self.layer, &self.surface) {
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
        }
        self.keyboard_released = Some(Instant::now());
    }

    fn leave(&mut self) {
        self.phase = Phase::Leaving(Instant::now());
        // A new Super+A from now on starts a fresh session.
        self.listener = None;
    }

    fn hide(&mut self) {
        if let Some(layer) = self.layer.take() {
            layer.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        self.phase = Phase::Gone;
    }

    /// Time-based transitions that must not depend on frame callbacks.
    /// Returns how long the event loop may sleep.
    fn tick(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let mut wait = None;
        if self.pending_text.is_some() {
            let due = self.keyboard_released.unwrap_or(now) + PASTE_DELAY;
            if now >= due {
                let text = self.pending_text.take().unwrap();
                if std::env::var_os("VOICETOTEXTN_DRY_RUN").is_some() {
                    log!("dry run, not pasting: {text}");
                } else {
                    self.paster.paste(&text, &self.qh);
                }
                self.leave();
            } else {
                wait = Some(due - now);
            }
        }
        if let Phase::Leaving(since) = self.phase {
            let deadline = since + HIDE_DEADLINE;
            if now >= deadline {
                self.hide();
            } else {
                wait = Some(wait.map_or(deadline - now, |w: Duration| w.min(deadline - now)));
            }
        }
        if self.phase == Phase::Gone && self.paster.idle() {
            self.exit = true;
        }
        wait
    }

    fn draw(&mut self) {
        if !self.configured {
            return;
        }
        let Some(surface) = self.surface.clone() else { return };
        let Some(index) = self.shm.free() else {
            self.needs_draw = true; // retried when a buffer is released
            return;
        };
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        let mode = match self.phase {
            Phase::Recording if !self.model_ready => Mode::Loading,
            Phase::Recording => Mode::Listening,
            Phase::Transcribing => Mode::Transcribing,
            Phase::Leaving(since) => Mode::Leaving((now - since).as_secs_f32()),
            Phase::Gone => return,
        };
        if let (Mode::Listening, Some(recorder)) = (mode, &self.recorder) {
            let total = recorder.tail(&mut self.samples);
            if total != self.seen_samples {
                self.seen_samples = total;
                self.scene.listen(&self.samples, (now - self.last_analysis).as_secs_f32());
                self.last_analysis = now;
            }
        }
        if !self.scene.draw(mode, (now - self.started).as_secs_f32(), dt) {
            return self.hide();
        }

        self.shm.write(index, &self.scene.canvas.px);
        surface.attach(Some(&self.shm.buffers[index]), 0, 0);
        surface.damage_buffer(0, 0, scene::WIDTH as i32, scene::HEIGHT as i32);
        surface.frame(&self.qh, ());
        surface.commit();
    }
}

/// Two shared-memory frame buffers, so one can be drawn while the compositor reads the other.
struct ShmBuffers {
    _fd: OwnedFd,
    map: *mut u32,
    pixels: usize,
    buffers: Vec<wl_buffer::WlBuffer>,
    busy: [bool; 2],
}

impl ShmBuffers {
    fn new(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, w: u32, h: u32) -> std::io::Result<ShmBuffers> {
        let pixels = (w * h) as usize;
        let len = pixels * 4 * 2;
        let fd = rustix::fs::memfd_create("voicetotextn", rustix::fs::MemfdFlags::CLOEXEC)?;
        rustix::fs::ftruncate(&fd, len as u64)?;
        // SAFETY: a fresh private mapping of our own memfd, never unmapped while in use.
        let map = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )?
        } as *mut u32;
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffers = (0..2)
            .map(|i| {
                let offset = (i * pixels * 4) as i32;
                pool.create_buffer(offset, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, i)
            })
            .collect();
        pool.destroy();
        Ok(ShmBuffers { _fd: fd, map, pixels, buffers, busy: [false; 2] })
    }

    fn free(&self) -> Option<usize> {
        self.busy.iter().position(|busy| !busy)
    }

    fn write(&mut self, index: usize, px: &[u32]) {
        // SAFETY: `index` < 2 and each buffer holds exactly `pixels` u32s inside the mapping.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.map.add(index * self.pixels), self.pixels) };
        dst.copy_from_slice(px);
        self.busy[index] = true;
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ZwlrLayerShellV1);

impl Dispatch<wl_buffer::WlBuffer, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.shm.busy[index] = false;
            if std::mem::take(&mut state.needs_draw) {
                state.draw();
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.draw();
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                if !state.configured {
                    state.configured = true;
                    state.draw();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.cancel();
                state.hide();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key { key: KEY_ESC, state: WEnum::Value(wl_keyboard::KeyState::Pressed), .. } =
            event
        {
            state.cancel();
        }
    }
}
