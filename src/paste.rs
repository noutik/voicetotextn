//! Puts the text into the clipboard and the primary selection (wlr-data-control,
//! which works without keyboard focus) and presses Shift+Insert through a
//! virtual keyboard. Shift+Insert pastes the clipboard in browsers and GTK/Qt
//! apps and the primary selection in kitty, so both get the text.

use std::io::Write;
use std::os::fd::AsFd;

use wayland_client::globals::GlobalList;
use wayland_client::protocol::{wl_keyboard, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1;
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

use crate::overlay::State;

const MIME_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// A keymap with a single key, Insert (evdev 110 = xkb 118).
const KEYMAP: &str = "xkb_keymap {
xkb_keycodes \"(unnamed)\" { minimum = 8; maximum = 255; <INS> = 118; };
xkb_types \"(unnamed)\" { include \"complete\" };
xkb_compatibility \"(unnamed)\" { include \"complete\" };
xkb_symbols \"(unnamed)\" { key <INS> { [ Insert ] }; };
};
\0";
const KEY_INSERT: u32 = 110;
const SHIFT_MASK: u32 = 1;

pub struct Paster {
    seat: wl_seat::WlSeat,
    data_control: Option<ZwlrDataControlManagerV1>,
    keyboards: Option<ZwpVirtualKeyboardManagerV1>,
    device: Option<ZwlrDataControlDeviceV1>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
    text: Vec<u8>,
    /// Selections we still own: the process stays alive to serve them.
    live_sources: usize,
}

impl Paster {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<State>, seat: wl_seat::WlSeat) -> Paster {
        let data_control = globals.bind(qh, 1..=2, ()).ok();
        let keyboards = globals.bind(qh, 1..=1, ()).ok();
        if data_control.is_none() {
            log!("compositor has no wlr-data-control: cannot copy");
        }
        if keyboards.is_none() {
            log!("compositor has no virtual-keyboard: cannot paste");
        }
        Paster { seat, data_control, keyboards, device: None, keyboard: None, text: Vec::new(), live_sources: 0 }
    }

    /// True when no selection needs serving any more.
    pub fn idle(&self) -> bool {
        self.live_sources == 0
    }

    pub fn paste(&mut self, text: &str, qh: &QueueHandle<State>) {
        let Some(manager) = self.data_control.clone() else { return };
        self.text = text.as_bytes().to_vec();
        let device = self.device.get_or_insert_with(|| manager.get_data_device(&self.seat, qh, ())).clone();
        device.set_selection(Some(&self.offer(&manager, qh)));
        if manager.version() >= 2 {
            device.set_primary_selection(Some(&self.offer(&manager, qh)));
        }
        // Requests are handled in order, so the selections are set before the key arrives.
        self.press_shift_insert(qh);
    }

    fn offer(&mut self, manager: &ZwlrDataControlManagerV1, qh: &QueueHandle<State>) -> ZwlrDataControlSourceV1 {
        let source = manager.create_data_source(qh, ());
        for mime in MIME_TYPES {
            source.offer(mime.into());
        }
        self.live_sources += 1;
        source
    }

    fn press_shift_insert(&mut self, qh: &QueueHandle<State>) {
        let Some(manager) = &self.keyboards else { return };
        let keymap = match keymap_fd() {
            Ok(fd) => fd,
            Err(e) => return log!("cannot create keymap: {e}"),
        };
        let keyboard = manager.create_virtual_keyboard(&self.seat, qh, ());
        keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), keymap.as_fd(), KEYMAP.len() as u32);
        let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let ms = (time.tv_sec * 1000 + time.tv_nsec / 1_000_000) as u32;
        keyboard.modifiers(SHIFT_MASK, 0, 0, 0);
        keyboard.key(ms, KEY_INSERT, wl_keyboard::KeyState::Pressed.into());
        keyboard.key(ms + 1, KEY_INSERT, wl_keyboard::KeyState::Released.into());
        keyboard.modifiers(0, 0, 0, 0);
        self.keyboard = Some(keyboard);
    }
}

fn keymap_fd() -> std::io::Result<std::os::fd::OwnedFd> {
    let fd = rustix::fs::memfd_create("voicetotextn-keymap", rustix::fs::MemfdFlags::CLOEXEC)?;
    std::fs::File::from(fd.try_clone()?).write_all(KEYMAP.as_bytes())?;
    Ok(fd)
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // We never read other selections: drop their offers right away.
            zwlr_data_control_device_v1::Event::Selection { id: Some(offer) }
            | zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => offer.destroy(),
            zwlr_data_control_device_v1::Event::Finished => {
                device.destroy();
                state.paster.device = None;
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { fd, .. } => {
                let _ = std::fs::File::from(fd).write_all(&state.paster.text);
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                source.destroy();
                state.paster.live_sources -= 1;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ZwlrDataControlManagerV1);
delegate_noop!(State: ignore ZwlrDataControlOfferV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);
