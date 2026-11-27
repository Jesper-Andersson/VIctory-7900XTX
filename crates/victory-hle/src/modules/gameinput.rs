//! `gameinput.dll`: input devices from the host. Gamepads come from XInput
//! (host slots 0-3); while none is connected, the keyboard drives one
//! stand-in gamepad (`CLASSIC`, or `VICTORY_BINDS=rockstar`), which the mouse
//! can steer too (`VICTORY_MOUSE_LOOK=1`). The keyboard is a device of its own
//! too, the mouse one with `VICTORY_MOUSE_DEVICE=1`. A poll thread turns host
//! state into readings (a new one per change) and runs device callbacks on
//! connect and disconnect.
#![allow(non_snake_case)]
// Title code calls these through the objects, passing pointers the real API
// would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use victory_core::pad::{
    Bind, Binds, PadControl, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};

use super::xasync::{E_INVALIDARG, E_NOTIMPL};
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleExport, HleModule};

/// The documented errors.
pub const DEVICE_DISCONNECTED: HRESULT = 0x838A_0001_u32 as i32;
pub const DEVICE_NOT_FOUND: HRESULT = 0x838A_0002_u32 as i32;
pub const READING_NOT_FOUND: HRESULT = 0x838A_0003_u32 as i32;
pub const REFERENCE_TOO_OLD: HRESULT = 0x838A_0004_u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;

/// Input kinds (bits) our devices have.
const KEYBOARD_KIND: u32 = 0x10;
const MOUSE_KIND: u32 = 0x20;
const GAMEPAD_KIND: u32 = 0x4_0000;
/// Device status of a connected device: connected, input enabled.
const STATUS_CONNECTED: u32 = 0x3;
const ASYNC_ENUMERATION: u32 = 1;

/// Gamepad buttons; the title maps each bit to one of its pad buttons.
const MENU: u32 = 0x1;
const VIEW: u32 = 0x2;
const A: u32 = 0x4;
const B: u32 = 0x8;
const X: u32 = 0x10;
const Y: u32 = 0x20;
const UP: u32 = 0x40;
const DOWN: u32 = 0x80;
const LEFT: u32 = 0x100;
const RIGHT: u32 = 0x200;
const LEFT_SHOULDER: u32 = 0x400;
const RIGHT_SHOULDER: u32 = 0x800;
const LEFT_STICK: u32 = 0x1000;
const RIGHT_STICK: u32 = 0x2000;

/// Devices: host pad slots, the keyboard's stand-in pad, the keyboard, the
/// mouse (connected only with `VICTORY_MOUSE_DEVICE=1`).
const PADS: usize = 4;
const FALLBACK: usize = PADS;
const KEYBOARD: usize = PADS + 1;
const MOUSE: usize = PADS + 2;
const DEVICES: usize = PADS + 3;

/// Readings kept per device for next/previous lookups.
const HISTORY: usize = 256;
const POLL: Duration = Duration::from_millis(8);

/// Mouse buttons in GameInput's bit order (left, right, middle, 4, 5), as
/// the bind tables name them.
const MOUSE_VKS: [u8; 5] = [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON, VK_XBUTTON1, VK_XBUTTON2];

/// The bind table in `VICTORY_BINDS` ([`Binds`]; classic when unset).
fn binds_from_env() -> Binds {
    Binds::from_env().unwrap_or_else(|value| {
        tracing::warn!(value, "VICTORY_BINDS unknown (classic or rockstar); classic");
        Binds::Classic
    })
}

/// The stand-in pad of the keys (and mouse buttons) held, by a table.
fn stand_in<'a>(table: impl Iterator<Item = &'a (u8, Bind)>, held: &[u8]) -> PadState {
    use PadControl as P;
    let mut pad = PadState::default();
    let add = |v: &mut f32, by: f32| *v = (*v + by).clamp(-1.0, 1.0);
    for (_, bind) in table.filter(|(vk, _)| held.contains(vk)) {
        let Bind::Control(control) = *bind else {
            continue;
        };
        let button = match control {
            P::Menu => MENU,
            P::View => VIEW,
            P::A => A,
            P::B => B,
            P::X => X,
            P::Y => Y,
            P::Up => UP,
            P::Down => DOWN,
            P::Left => LEFT,
            P::Right => RIGHT,
            P::LeftShoulder => LEFT_SHOULDER,
            P::RightShoulder => RIGHT_SHOULDER,
            P::LeftStick => LEFT_STICK,
            P::RightStick => RIGHT_STICK,
            P::LeftTrigger => {
                pad.left_trigger = 1.0;
                0
            }
            P::RightTrigger => {
                pad.right_trigger = 1.0;
                0
            }
            P::LeftStickUp => {
                add(&mut pad.left_y, 1.0);
                0
            }
            P::LeftStickDown => {
                add(&mut pad.left_y, -1.0);
                0
            }
            P::LeftStickLeft => {
                add(&mut pad.left_x, -1.0);
                0
            }
            P::LeftStickRight => {
                add(&mut pad.left_x, 1.0);
                0
            }
            P::RightStickUp => {
                add(&mut pad.right_y, 1.0);
                0
            }
            P::RightStickDown => {
                add(&mut pad.right_y, -1.0);
                0
            }
            P::RightStickLeft => {
                add(&mut pad.right_x, -1.0);
                0
            }
            P::RightStickRight => {
                add(&mut pad.right_x, 1.0);
                0
            }
        };
        pad.buttons |= button;
    }
    pad
}

/// Mouse travel as a right stick (`VICTORY_MOUSE_LOOK=1`). A stick sets a
/// turning speed, the mouse a distance: travel per second over `speed` is
/// the deflection, smoothed over `smooth` seconds so the stick comes back
/// to rest within a few polls of the mouse stopping. Any deflection starts
/// at `dead`, where the title's own dead zone ends, so slow aiming moves.
#[derive(Clone, Debug)]
struct Look {
    /// Counts per second for a full deflection.
    speed: f32,
    invert: bool,
    dead: f32,
    smooth: f32,
    x: f32,
    y: f32,
}

impl Look {
    fn new(sensitivity: f32, invert: bool, dead: f32) -> Look {
        Look {
            speed: 1000.0 / sensitivity.max(0.01),
            invert,
            dead: dead.clamp(0.0, 0.9),
            smooth: 0.03,
            x: 0.0,
            y: 0.0,
        }
    }

    fn from_env() -> Option<Look> {
        if std::env::var_os("VICTORY_MOUSE_LOOK").is_none_or(|v| v != "1") {
            return None;
        }
        let number = |name: &str, default: f32| {
            std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        };
        let invert = std::env::var_os("VICTORY_MOUSE_INVERT").is_some_and(|v| v == "1");
        Some(Look::new(number("VICTORY_MOUSE_SENS", 1.0), invert, number("VICTORY_MOUSE_DEAD", 0.2)))
    }

    /// The stick after `dx`, `dy` counts (right, down positive) in `dt`
    /// seconds.
    fn step(&mut self, dx: i64, dy: i64, dt: f32) -> (f32, f32) {
        let dt = dt.max(0.001);
        let up = if self.invert { 1.0 } else { -1.0 };
        let (tx, ty) = (dx as f32 / dt / self.speed, up * dy as f32 / dt / self.speed);
        let blend = 1.0 - (-dt / self.smooth).exp();
        self.x += (tx - self.x) * blend;
        self.y += (ty - self.y) * blend;
        let length = self.x.hypot(self.y);
        // At rest once the mouse stopped and the stick has mostly come back:
        // the lift above would otherwise hold it past the dead zone.
        if length == 0.0 || (length < 0.03 && dx == 0 && dy == 0) {
            (self.x, self.y) = (0.0, 0.0);
            return (0.0, 0.0);
        }
        let out = (self.dead + (1.0 - self.dead) * length).min(1.0);
        (self.x / length * out, self.y / length * out)
    }
}

/// Mouse input since the last poll.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MouseInput {
    /// Travel in counts, right and down positive.
    dx: i64,
    dy: i64,
    /// Wheel travel, 120 a notch, away from the user positive.
    wheel: i64,
    /// Held, in GameInput's bit order (left, right, middle, 4, 5).
    buttons: u32,
    /// The pointer is ours (mouse look): buttons and travel play.
    captured: bool,
}

/// A mouse reading: buttons, then travel and wheel summed since the start.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MouseState {
    buttons: u32,
    x: i64,
    y: i64,
    wheel_x: i64,
    wheel_y: i64,
}

const _: () = assert!(size_of::<MouseState>() == 40);

/// Turns keys and the mouse into the stand-in pad.
#[derive(Clone, Debug, Default)]
struct Controls {
    binds: Binds,
    look: Option<Look>,
    /// Report a GameInput mouse device (`VICTORY_MOUSE_DEVICE=1`).
    mouse_device: bool,
    driving: bool,
    held: Vec<u8>,
    last: Option<u64>,
    mouse: MouseState,
}

impl Controls {
    fn from_env() -> Controls {
        let controls = Controls {
            binds: binds_from_env(),
            look: Look::from_env(),
            mouse_device: std::env::var_os("VICTORY_MOUSE_DEVICE").is_some_and(|v| v == "1"),
            ..Default::default()
        };
        tracing::info!(
            binds = ?controls.binds,
            mouse_look = ?controls.look,
            mouse_device = controls.mouse_device,
            "input controls"
        );
        controls
    }

    /// Whether the host should read the mouse at all.
    fn wants_mouse(&self) -> bool {
        self.look.is_some() || self.mouse_device
    }

    /// The stand-in pad at `now` (µs).
    fn pad(&mut self, keys: &[KeyState], mouse: MouseInput, now: u64) -> PadState {
        let mut held: Vec<u8> = keys.iter().map(|k| k.virtual_key).collect();
        if self.look.is_some() && mouse.captured {
            held.extend(
                MOUSE_VKS
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mouse.buttons & (1 << bit) != 0)
                    .map(|(_, vk)| *vk),
            );
        }
        if self.binds == Binds::Rockstar {
            let pressed = |vk: &u8| held.contains(vk) && !self.held.contains(vk);
            let flips = self
                .binds
                .table(self.driving)
                .filter(|(vk, bind)| *bind == Bind::SwitchLayout && pressed(vk))
                .count();
            if flips % 2 == 1 {
                self.driving = !self.driving;
                victory_core::pad::set_driving(self.driving);
                tracing::info!(driving = self.driving, "rockstar binds layout");
            }
        }
        let mut pad = stand_in(self.binds.table(self.driving), &held);
        self.held = held;
        let dt = self.last.map_or(0.008, |last| now.saturating_sub(last) as f32 / 1e6);
        self.last = Some(now);
        if let Some(look) = &mut self.look {
            let (dx, dy) = if mouse.captured { (mouse.dx, mouse.dy) } else { (0, 0) };
            let (x, y) = look.step(dx, dy, dt);
            pad.right_x = (pad.right_x + x).clamp(-1.0, 1.0);
            pad.right_y = (pad.right_y + y).clamp(-1.0, 1.0);
        }
        pad
    }

    /// The mouse device's reading, `None` while it is not reported.
    fn mouse_state(&mut self, mouse: MouseInput) -> Option<MouseState> {
        if !self.mouse_device {
            return None;
        }
        self.mouse.buttons = mouse.buttons;
        self.mouse.x += mouse.dx;
        self.mouse.y += mouse.dy;
        self.mouse.wheel_y += mouse.wheel;
        Some(self.mouse)
    }
}

/// A gamepad reading: triggers 0 to 1, sticks -1 to 1 (up positive).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadState {
    buttons: u32,
    left_trigger: f32,
    right_trigger: f32,
    left_x: f32,
    left_y: f32,
    right_x: f32,
    right_y: f32,
}

/// One key held down: scan code (0xE0xx when extended), character, key.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct KeyState {
    scan_code: u32,
    code_point: u32,
    virtual_key: u8,
    dead_key: bool,
}

const _: () = assert!(size_of::<KeyState>() == 12 && size_of::<PadState>() == 28);

/// Host input, or a fake in tests.
trait Source: Send {
    /// The pad in host slot `slot`, `None` when the slot is empty.
    fn pad(&mut self, slot: usize) -> Option<PadState>;
    /// Keys held down; none while the title's window lacks focus.
    fn keys(&mut self) -> Vec<KeyState>;
    /// Motor speeds: low frequency (left), high frequency (right).
    fn rumble(&mut self, slot: usize, low: u16, high: u16);
    /// Mouse travel since the last call; asked only with a mouse switch on.
    /// `capture`: mouse look is on, so the pointer is taken while the
    /// title's window has focus.
    fn mouse(&mut self, _capture: bool) -> MouseInput {
        MouseInput::default()
    }
}

/// Two pads as one: buttons of both, and of each axis and trigger the one
/// pushed further.
fn merged(a: PadState, b: PadState) -> PadState {
    let further = |x: f32, y: f32| if y.abs() > x.abs() { y } else { x };
    PadState {
        buttons: a.buttons | b.buttons,
        left_trigger: further(a.left_trigger, b.left_trigger),
        right_trigger: further(a.right_trigger, b.right_trigger),
        left_x: further(a.left_x, b.left_x),
        left_y: further(a.left_y, b.left_y),
        right_x: further(a.right_x, b.right_x),
        right_y: further(a.right_y, b.right_y),
    }
}

/// Whether the title sees one pad that is always there, fed by the keyboard
/// and by every host pad (on by default; `VICTORY_ONE_PAD=0` shows each host
/// pad as a device of its own, and the stand-in only while there is none).
/// The title keeps to the pad it started with: with the stand-in taken away
/// when a pad was switched on, it showed its controller-disconnected screen
/// until the pad was off again. It also keeps a pad the host lists twice (its
/// own slot and a remapper's) from being two players.
fn one_pad() -> bool {
    std::env::var_os("VICTORY_ONE_PAD").is_none_or(|v| v != "0")
}

#[repr(C)]
struct Table<const N: usize>([*const (); N]);
// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Table<N> {}

/// The input object; one per hub.
#[repr(C)]
pub struct Object {
    vtable: &'static Table<22>,
    hub: *const Hub,
}

#[repr(C)]
pub struct Device {
    vtable: &'static Table<21>,
    hub: *const Hub,
    index: usize,
    info: DeviceInfo,
}

/// What a device says about itself. The title reads `id` (+0x20); the rest
/// follows the public layout and is zero where we have nothing to say.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by title code
struct DeviceInfo {
    size: u32,
    vendor: u16,
    product: u16,
    revision: u16,
    interface: u8,
    collection: u8,
    usage: [u16; 2],
    versions: [u16; 8],
    id: [u8; 32],
    root_id: [u8; 32],
    family: i32,
    capabilities: u32,
    supported_input: u32,
    rumble_motors: u32,
    /// Counts and optional detail pointers.
    rest: [u64; 26],
}

const _: () = assert!(std::mem::offset_of!(DeviceInfo, id) == 0x20);

fn kind(index: usize) -> u32 {
    match index {
        KEYBOARD => KEYBOARD_KIND,
        MOUSE => MOUSE_KIND,
        _ => GAMEPAD_KIND,
    }
}

impl Device {
    fn new(index: usize) -> Device {
        let mut id = [0; 32];
        id[..7].copy_from_slice(b"victory");
        id[31] = index as u8 + 1;
        let info = DeviceInfo {
            size: size_of::<DeviceInfo>() as u32,
            id,
            root_id: id,
            supported_input: kind(index),
            // Low and high frequency.
            rumble_motors: if index <= FALLBACK { 0x3 } else { 0 },
            ..Default::default()
        };
        Device {
            vtable: &DEVICE,
            hub: std::ptr::null(),
            index,
            info,
        }
    }

    fn hub(&self) -> &'static Hub {
        // SAFETY: set by `Hub::new`; hubs are never freed.
        unsafe { &*self.hub }
    }
}

#[derive(PartialEq)]
enum Value {
    Pad(PadState),
    Keys(Vec<KeyState>),
    Mouse(MouseState),
}

#[repr(C)]
pub struct Reading {
    vtable: &'static Table<25>,
    refs: AtomicU32,
    device: *const Device,
    sequence: u64,
    timestamp: u64,
    value: Value,
}

impl Reading {
    fn kind(&self) -> u32 {
        match self.value {
            Value::Pad(_) => GAMEPAD_KIND,
            Value::Keys(_) => KEYBOARD_KIND,
            Value::Mouse(_) => MOUSE_KIND,
        }
    }
}

/// A counted reference to a reading.
struct Ref(NonNull<Reading>);

impl Ref {
    fn new(reading: Reading) -> Ref {
        Ref(NonNull::from(Box::leak(Box::new(reading))))
    }

    /// Another reference, for the title.
    fn share(&self) -> *const Reading {
        reading_add_ref(self.0.as_ptr());
        self.0.as_ptr()
    }
}

impl Deref for Ref {
    type Target = Reading;
    fn deref(&self) -> &Reading {
        // SAFETY: we hold a reference, so it is alive.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for Ref {
    fn drop(&mut self) {
        reading_release(self.0.as_ptr());
    }
}

type DeviceCallback = extern "system" fn(u64, *mut c_void, *const Device, u64, u32, u32);

#[derive(Clone, Copy)]
struct Registration {
    token: u64,
    /// One device's address, or 0 for any.
    device: usize,
    kind: u32,
    status: u32,
    context: usize,
    callback: DeviceCallback,
}

impl Registration {
    fn matches(&self, devices: &[Device; DEVICES], index: usize) -> bool {
        kind(index) & self.kind != 0
            && (self.device == 0 || self.device == &devices[index] as *const Device as usize)
    }

    fn event(&self, device: usize, timestamp: u64, current: u32, previous: u32) -> Event {
        Event {
            registration: *self,
            device,
            timestamp,
            current,
            previous,
        }
    }
}

struct Event {
    registration: Registration,
    device: usize,
    timestamp: u64,
    current: u32,
    previous: u32,
}

#[derive(Default)]
struct Slot {
    connected: bool,
    sequence: u64,
    history: VecDeque<Ref>,
}

struct State {
    slots: [Slot; DEVICES],
    callbacks: Vec<Registration>,
    /// Asynchronous enumerations, delivered by the next poll.
    pending: Vec<Event>,
    /// Device callbacks running now: token and thread.
    running: Vec<(u64, ThreadId)>,
    controls: Controls,
}

pub struct Hub {
    object: Object,
    devices: [Device; DEVICES],
    /// Apart from the state: XInput can be slow, and the title reads
    /// readings every frame.
    source: Mutex<Box<dyn Source>>,
    /// The title sees one pad ([`one_pad`]).
    one_pad: bool,
    /// The source is asked for the mouse ([`Controls::wants_mouse`]).
    wants_mouse: bool,
    /// Mouse look is on: the source takes the pointer.
    capture_mouse: bool,
    state: Mutex<State>,
    /// A device callback returned.
    returned: Condvar,
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// SAFETY: the back pointers are set before the hub is shared and never
// change; what changes sits behind the mutex or is atomic.
unsafe impl Send for Hub {}
// SAFETY: as above.
unsafe impl Sync for Hub {}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Microseconds, the clock of every timestamp we hand out.
fn now() -> u64 {
    START.elapsed().as_micros() as u64
}

impl Hub {
    #[cfg(test)]
    fn new(source: Box<dyn Source>, one_pad: bool) -> &'static Hub {
        Hub::with(source, one_pad, Controls::default())
    }

    fn with(source: Box<dyn Source>, one_pad: bool, controls: Controls) -> &'static Hub {
        let wants_mouse = controls.wants_mouse();
        let capture_mouse = controls.look.is_some();
        let hub = Box::into_raw(Box::new(Hub {
            object: Object {
                vtable: &INPUT,
                hub: std::ptr::null(),
            },
            devices: std::array::from_fn(Device::new),
            source: Mutex::new(source),
            one_pad,
            wants_mouse,
            capture_mouse,
            state: Mutex::new(State {
                slots: Default::default(),
                callbacks: Vec::new(),
                pending: Vec::new(),
                running: Vec::new(),
                controls,
            }),
            returned: Condvar::new(),
        }));
        // SAFETY: just allocated and never freed; nothing else sees it yet.
        unsafe {
            (*hub).object.hub = hub;
            for device in &mut (*hub).devices {
                device.hub = hub;
            }
            &*hub
        }
    }

    /// A hub over the host's input, polled from now on.
    fn start(source: Box<dyn Source>) -> &'static Hub {
        let hub = Hub::with(source, one_pad(), Controls::from_env());
        hub.poll();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL);
                hub.poll();
            }
        });
        hub
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Reads the host once; device callbacks run on the calling thread.
    fn poll(&self) {
        let (pads, keys, mouse) = {
            let mut source = lock(&self.source);
            let pads: [_; PADS] = std::array::from_fn(|slot| source.pad(slot));
            let mouse = if self.wants_mouse {
                source.mouse(self.capture_mouse)
            } else {
                MouseInput::default()
            };
            (pads, source.keys(), mouse)
        };
        let events = self.state().poll(&self.devices, self.one_pad, pads, keys, mouse);
        self.deliver(events);
    }

    /// Runs callbacks, never under our lock: they call back into us. One
    /// unregistered since the event was taken does not run, and
    /// `UnregisterCallback` waits for one that runs.
    fn deliver(&self, events: Vec<Event>) {
        let me = std::thread::current().id();
        for e in events {
            let r = e.registration;
            {
                let mut state = self.state();
                if !state.callbacks.iter().any(|c| c.token == r.token) {
                    continue;
                }
                state.running.push((r.token, me));
            }
            (r.callback)(
                r.token,
                r.context as *mut c_void,
                &self.devices[e.device],
                e.timestamp,
                e.current,
                e.previous,
            );
            let mut state = self.state();
            if let Some(i) = state.running.iter().position(|&x| x == (r.token, me)) {
                state.running.swap_remove(i);
            }
            self.returned.notify_all();
        }
    }

    fn index(&self, device: *const Device) -> Option<usize> {
        self.devices.iter().position(|d| std::ptr::eq(d, device))
    }
}

impl State {
    fn poll(
        &mut self,
        devices: &[Device; DEVICES],
        one_pad: bool,
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        mouse: MouseInput,
    ) -> Vec<Event> {
        let now = now();
        let mut events = std::mem::take(&mut self.pending);
        let stand_in = self.controls.pad(&keys, mouse, now);
        let mouse = self.controls.mouse_state(mouse).map(Value::Mouse);
        if one_pad {
            let pad = pads.into_iter().flatten().fold(stand_in, merged);
            self.update(devices, FALLBACK, Some(Value::Pad(pad)), now, &mut events);
        } else {
            let any_pad = pads.iter().any(Option::is_some);
            for (slot, pad) in pads.into_iter().enumerate() {
                self.update(devices, slot, pad.map(Value::Pad), now, &mut events);
            }
            let fallback = (!any_pad).then_some(Value::Pad(stand_in));
            self.update(devices, FALLBACK, fallback, now, &mut events);
        }
        self.update(devices, KEYBOARD, Some(Value::Keys(keys)), now, &mut events);
        self.update(devices, MOUSE, mouse, now, &mut events);
        events
    }

    /// Records a device's state: a reading when it changed, callbacks when
    /// the device came or went.
    fn update(
        &mut self,
        devices: &[Device; DEVICES],
        index: usize,
        value: Option<Value>,
        now: u64,
        events: &mut Vec<Event>,
    ) {
        let slot = &mut self.slots[index];
        let was = slot.connected;
        slot.connected = value.is_some();
        if let Some(value) = value
            && (!was || slot.history.back().is_none_or(|r| r.value != value))
        {
            slot.sequence += 1;
            if slot.history.len() == HISTORY {
                slot.history.pop_front();
            }
            slot.history.push_back(Ref::new(Reading {
                vtable: &READING,
                refs: AtomicU32::new(1),
                device: &devices[index],
                sequence: slot.sequence,
                timestamp: now,
                value,
            }));
        }
        if was != slot.connected {
            tracing::info!(device = index, connected = !was, "input device");
            let (current, previous) = if was {
                (0, STATUS_CONNECTED)
            } else {
                (STATUS_CONNECTED, 0)
            };
            events.extend(
                self.callbacks
                    .iter()
                    .filter(|r| r.matches(devices, index) && r.status & STATUS_CONNECTED != 0)
                    .map(|r| r.event(index, now, current, previous)),
            );
        }
    }
}

#[cfg(windows)]
fn host() -> Box<dyn Source> {
    Box::new(host::Host::default())
}

#[cfg(not(windows))]
fn host() -> Box<dyn Source> {
    struct Nothing;
    impl Source for Nothing {
        fn pad(&mut self, _: usize) -> Option<PadState> {
            None
        }
        fn keys(&mut self) -> Vec<KeyState> {
            Vec::new()
        }
        fn rumble(&mut self, _: usize, _: u16, _: u16) {}
    }
    Box::new(Nothing)
}

static HUB: LazyLock<&'static Hub> = LazyLock::new(|| Hub::start(host()));

/// Mouse look holds the pointer: the title's window shows none.
pub(crate) fn mouse_captured() -> bool {
    #[cfg(windows)]
    return host::CAPTURED.load(Ordering::Relaxed);
    #[cfg(not(windows))]
    false
}

type This = *const Object;
type Out = *mut *const c_void;

/// The DLL's only export (ordinal 1): the input object, static and shared.
pub extern "system" fn GameInputCreate(out: *mut This) -> HRESULT {
    tracing::info!("GameInputCreate");
    if out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a pointer.
    unsafe { out.write(&HUB.object) };
    S_OK
}

fn hub(this: This) -> &'static Hub {
    // SAFETY: `this` is a hub's object; hubs are never freed.
    unsafe { &*(*this).hub }
}

/// Clears a COM out pointer and returns `hr`.
fn fail(out: Out, hr: HRESULT) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    hr
}

/// Gives the title a reference to `reading`, or fails as with none.
fn hand(out: Out, reading: Option<&Ref>) -> HRESULT {
    match reading {
        _ if out.is_null() => E_INVALIDARG,
        Some(reading) => {
            // SAFETY: checked non-null; the title passes room for a pointer.
            unsafe { out.write(reading.share().cast()) };
            S_OK
        }
        None => fail(out, READING_NOT_FOUND),
    }
}

extern "system" fn no_interface(_: *const c_void, _iid: *const c_void, out: Out) -> HRESULT {
    tracing::warn!("input QueryInterface not implemented");
    fail(out, E_NOINTERFACE)
}
/// The input object and devices live as long as the process.
extern "system" fn add_ref(_: *const c_void) -> u32 {
    1
}

extern "system" fn timestamp(_: This) -> u64 {
    now()
}

extern "system" fn current_reading(
    this: This,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    asked(kind, "current reading");
    let hub = hub(this);
    let state = hub.state();
    let newest = |i: usize| {
        state.slots[i]
            .history
            .back()
            .filter(|r| r.kind() & kind != 0)
    };
    let found = if device.is_null() {
        (0..DEVICES)
            .filter(|&i| state.slots[i].connected)
            .filter_map(newest)
            .max_by_key(|r| r.timestamp)
    } else {
        match hub.index(device) {
            None => return fail(out, E_INVALIDARG),
            Some(i) if !state.slots[i].connected => return fail(out, DEVICE_DISCONNECTED),
            Some(i) => newest(i),
        }
    };
    tracing::trace!(
        kind = format_args!("{kind:#x}"),
        device = ?hub.index(device),
        sequence = found.map(|r| r.sequence),
        "current reading"
    );
    hand(out, found)
}

/// Logs the first time the title asks for an input kind: whether it ever
/// asks for a mouse (0x20) says whether it has a mouse path of its own.
fn asked(kind: u32, how: &'static str) {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    if SEEN.fetch_or(kind, Ordering::Relaxed) & kind != kind {
        tracing::info!(kind = format_args!("{kind:#x}"), how, "title asks for input kind");
    }
}

/// The reading after (`forward`) or before `reference` on its device.
fn relative(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
    forward: bool,
) -> HRESULT {
    if reference.is_null() {
        return fail(out, E_INVALIDARG);
    }
    asked(kind, "next or previous reading");
    let hub = hub(this);
    // SAFETY: a reading we handed out and the title still holds.
    let reference = unsafe { &*reference };
    // shortcut: with no device given only the reference's device is walked,
    // not every device's readings in time order; the title walks
    // only keyboard readings (one device) and the current pad's previous one.
    let Some(index) = hub.index(reference.device) else {
        return fail(out, E_INVALIDARG);
    };
    if !device.is_null() && !std::ptr::eq(device, reference.device) {
        return fail(out, E_INVALIDARG);
    }
    let state = hub.state();
    let history = &state.slots[index].history;
    if history
        .front()
        .is_none_or(|oldest| reference.sequence < oldest.sequence)
    {
        return fail(out, REFERENCE_TOO_OLD);
    }
    let wanted = |r: &&Ref| r.kind() & kind != 0;
    let found = if forward {
        history
            .iter()
            .filter(|r| r.sequence > reference.sequence)
            .find(wanted)
    } else {
        history
            .iter()
            .rev()
            .filter(|r| r.sequence < reference.sequence)
            .find(wanted)
    };
    hand(out, found)
}
extern "system" fn next_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, true)
}
extern "system" fn previous_reading(
    this: This,
    reference: *const Reading,
    kind: u32,
    device: *const Device,
    out: Out,
) -> HRESULT {
    relative(this, reference, kind, device, out, false)
}
/// The device's reading at `time`, among those still kept.
extern "system" fn temporal_reading(
    this: This,
    time: u64,
    device: *const Device,
    out: Out,
) -> HRESULT {
    let hub = hub(this);
    let Some(index) = hub.index(device) else {
        return fail(out, E_INVALIDARG);
    };
    let state = hub.state();
    let history = &state.slots[index].history;
    hand(out, history.iter().rev().find(|r| r.timestamp <= time))
}

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn token(out: *mut u64) -> HRESULT {
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)) };
    }
    S_OK
}

// shortcut: reading callbacks never run; the title registers none
// (it polls).
extern "system" fn register_reading(
    _: This,
    _device: *const c_void,
    _kind: u32,
    _threshold: f32,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    tracing::warn!("input reading callbacks are never called");
    token(out)
}

/// Device callbacks run on our poll thread when a device comes or goes.
/// Enumeration reports the devices already there: blocking on the caller's
/// thread before we return (the title registers holding a lock its callback
/// takes again), asynchronous on the next poll.
extern "system" fn register_device(
    this: This,
    device: *const Device,
    kind: u32,
    status: u32,
    enumeration: u32,
    context: *mut c_void,
    callback: Option<DeviceCallback>,
    out: *mut u64,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    let hub = hub(this);
    let registration = Registration {
        token: NEXT_TOKEN.fetch_add(1, Ordering::Relaxed),
        device: device as usize,
        kind,
        status,
        context: context as usize,
        callback,
    };
    let mut found = Vec::new();
    {
        let mut state = hub.state();
        if enumeration != 0 && status & STATUS_CONNECTED != 0 {
            let now = now();
            found = (0..DEVICES)
                .filter(|&i| state.slots[i].connected && registration.matches(&hub.devices, i))
                .map(|i| registration.event(i, now, STATUS_CONNECTED, 0))
                .collect();
        }
        tracing::info!(
            kind = format_args!("{kind:#x}"),
            enumeration,
            devices = found.len(),
            "input device callback"
        );
        state.callbacks.push(registration);
        if enumeration == ASYNC_ENUMERATION {
            state.pending.append(&mut found);
        }
    }
    if !out.is_null() {
        // SAFETY: checked non-null.
        unsafe { out.write(registration.token) };
    }
    hub.deliver(found);
    S_OK
}
/// Guide button and keyboard layout callbacks: neither ever changes here.
extern "system" fn register_other(
    _: This,
    _device: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    token(out)
}

fn forget(this: This, token: u64) {
    let mut state = hub(this).state();
    state.callbacks.retain(|r| r.token != token);
    state.pending.retain(|e| e.registration.token != token);
}
extern "system" fn stop_callback(this: This, token: u64) {
    forget(this, token);
}
/// Stops the callback, then waits up to `timeout` µs for a call of it that
/// is running; false on time-out, or from inside that callback (it cannot
/// wait for itself), as documented. Either way it is never called again.
extern "system" fn unregister_callback(this: This, token: u64, timeout: u64) -> bool {
    forget(this, token);
    let hub = hub(this);
    let me = std::thread::current().id();
    let deadline = Instant::now().checked_add(Duration::from_micros(timeout));
    let mut state = hub.state();
    loop {
        let running = state.running.iter().filter(|r| r.0 == token);
        if running.clone().any(|r| r.1 == me) {
            return false;
        }
        if running.count() == 0 {
            return true;
        }
        state = match deadline {
            None => hub.returned.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                let waited = hub.returned.wait_timeout(state, left);
                waited.unwrap_or_else(PoisonError::into_inner).0
            }
        };
    }
}

/// Dispatcher and aggregate device: no title of ours asks for them.
extern "system" fn create_dispatcher(_: This, out: Out) -> HRESULT {
    tracing::warn!("input dispatcher not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn create_aggregate(_: This, _kind: u32, out: Out) -> HRESULT {
    tracing::warn!("aggregate input device not implemented");
    fail(out, E_NOTIMPL)
}
extern "system" fn find_by_id(this: This, id: *const [u8; 32], out: Out) -> HRESULT {
    if id.is_null() || out.is_null() {
        return fail(out, E_INVALIDARG);
    }
    // SAFETY: checked non-null; the title passes a device id.
    let id = unsafe { &*id };
    match hub(this).devices.iter().find(|d| d.info.id == *id) {
        Some(device) => {
            // SAFETY: checked non-null.
            unsafe { out.write((device as *const Device).cast()) };
            S_OK
        }
        None => fail(out, DEVICE_NOT_FOUND),
    }
}
/// Lookup by object, platform handle or platform string: our devices have
/// none of these.
extern "system" fn find_device(_: This, _value: *const c_void, out: Out) -> HRESULT {
    fail(out, DEVICE_NOT_FOUND)
}
extern "system" fn enable_oem_device(
    _: This,
    _vendor: u16,
    _product: u16,
    _interface: u8,
    _collection: u8,
) -> HRESULT {
    S_OK
}
extern "system" fn set_focus_policy(_: This, _policy: u32) {}

static INPUT: Table<22> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    timestamp as *const (),           // 0x18
    current_reading as *const (),     // 0x20
    next_reading as *const (),        // 0x28
    previous_reading as *const (),    // 0x30
    temporal_reading as *const (),    // 0x38
    register_reading as *const (),    // 0x40
    register_device as *const (),     // 0x48
    register_other as *const (),      // 0x50 guide button
    register_other as *const (),      // 0x58 keyboard layout
    stop_callback as *const (),       // 0x60
    unregister_callback as *const (), // 0x68
    create_dispatcher as *const (),   // 0x70
    create_aggregate as *const (),    // 0x78
    find_by_id as *const (),          // 0x80
    find_device as *const (),         // 0x88 by object
    find_device as *const (),         // 0x90 by platform handle
    find_device as *const (),         // 0x98 by platform string
    enable_oem_device as *const (),   // 0xa0
    set_focus_policy as *const (),    // 0xa8
]);

extern "system" fn device_info(this: *const Device) -> *const DeviceInfo {
    // SAFETY: one of our devices.
    unsafe { &(*this).info }
}
extern "system" fn device_status(this: *const Device) -> u32 {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    if device.hub().state().slots[device.index].connected {
        STATUS_CONNECTED
    } else {
        0
    }
}
// shortcut: trigger motors are dropped (XInput has none). The host API that
// drives them without a redistributable is Windows.Gaming.Input (WinRT
// `Gamepad.Vibration`), which needs WinRT bindings this crate lacks outside
// diagnostics builds; the title sends trigger levels in slot 0x50.
extern "system" fn set_rumble(this: *const Device, params: *const [f32; 4]) {
    // SAFETY: one of our devices.
    let device = unsafe { &*this };
    let one_pad = device.hub().one_pad;
    if device.index >= PADS && !(one_pad && device.index == FALLBACK) {
        return;
    }
    let [low, high, ..] = if params.is_null() {
        [0.0; 4]
    } else {
        // SAFETY: checked non-null; four motor levels.
        unsafe { *params }
    };
    let speed = |level: f32| (level.clamp(0.0, 1.0) * f32::from(u16::MAX)) as u16;
    let mut source = lock(&device.hub().source);
    if one_pad {
        // The one pad is every host pad.
        for slot in 0..PADS {
            if source.pad(slot).is_some() {
                source.rumble(slot, speed(low), speed(high));
            }
        }
    } else {
        source.rumble(device.index, speed(low), speed(high));
    }
}
extern "system" fn unsupported<const SLOT: usize>(_: *const c_void) -> HRESULT {
    tracing::warn!(
        slot = format_args!("{SLOT:#x}"),
        "input device method not implemented"
    );
    E_NOTIMPL
}

static DEVICE: Table<21> = Table([
    no_interface as *const (),        // 0x00
    add_ref as *const (),             // 0x08 AddRef
    add_ref as *const (),             // 0x10 Release
    device_info as *const (),         // 0x18
    device_status as *const (),       // 0x20
    unsupported::<0x28> as *const (), // battery
    unsupported::<0x30> as *const (), // force feedback
    unsupported::<0x38> as *const (), //
    unsupported::<0x40> as *const (), //
    unsupported::<0x48> as *const (), // haptics
    set_rumble as *const (),          // 0x50
    unsupported::<0x58> as *const (), // input synchronization
    unsupported::<0x60> as *const (), //
    unsupported::<0x68> as *const (), // power off
    unsupported::<0x70> as *const (), // raw device access
    unsupported::<0x78> as *const (), //
    unsupported::<0x80> as *const (), //
    unsupported::<0x88> as *const (), //
    unsupported::<0x90> as *const (), //
    unsupported::<0x98> as *const (), //
    unsupported::<0xa0> as *const (), //
]);

fn reading<'a>(this: *const Reading) -> &'a Reading {
    // SAFETY: a reading we handed out and the title still holds.
    unsafe { &*this }
}
extern "system" fn reading_add_ref(this: *const Reading) -> u32 {
    reading(this).refs.fetch_add(1, Ordering::Relaxed) + 1
}
extern "system" fn reading_release(this: *const Reading) -> u32 {
    let left = reading(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference is gone; `Ref::new` boxed it.
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
    left
}
extern "system" fn input_kind(this: *const Reading) -> u32 {
    reading(this).kind()
}
extern "system" fn sequence_number(this: *const Reading, kind: u32) -> u64 {
    let reading = reading(this);
    if reading.kind() & kind != 0 {
        reading.sequence
    } else {
        0
    }
}
extern "system" fn reading_timestamp(this: *const Reading) -> u64 {
    reading(this).timestamp
}
extern "system" fn reading_device(this: *const Reading, out: *mut *const Device) {
    if !out.is_null() {
        // SAFETY: checked non-null; devices need no reference.
        unsafe { out.write(reading(this).device) };
    }
}
extern "system" fn key_count(this: *const Reading) -> u32 {
    match &reading(this).value {
        Value::Keys(keys) => keys.len() as u32,
        _ => 0,
    }
}
extern "system" fn mouse_state(this: *const Reading, out: *mut MouseState) -> bool {
    asked(MOUSE_KIND, "mouse state");
    match &reading(this).value {
        Value::Mouse(state) if !out.is_null() => {
            // SAFETY: checked non-null; room for a mouse state.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
extern "system" fn key_state(this: *const Reading, count: u32, out: *mut KeyState) -> u32 {
    let Value::Keys(keys) = &reading(this).value else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let n = keys.len().min(count as usize);
    // SAFETY: the title passes room for `count` keys.
    unsafe { std::ptr::copy_nonoverlapping(keys.as_ptr(), out, n) };
    n as u32
}
extern "system" fn gamepad_state(this: *const Reading, out: *mut PadState) -> bool {
    match &reading(this).value {
        Value::Pad(state) if !out.is_null() => {
            // SAFETY: checked non-null.
            unsafe { out.write(*state) };
            true
        }
        _ => false,
    }
}
/// Kinds our devices do not have: no count, no state.
extern "system" fn nothing(_: *const c_void) -> usize {
    0
}

static READING: Table<25> = Table([
    no_interface as *const (),      // 0x00
    reading_add_ref as *const (),   // 0x08
    reading_release as *const (),   // 0x10
    input_kind as *const (),        // 0x18
    sequence_number as *const (),   // 0x20
    reading_timestamp as *const (), // 0x28
    reading_device as *const (),    // 0x30
    nothing as *const (),           // 0x38 raw report
    nothing as *const (),           // 0x40 controller axes, buttons, switches
    nothing as *const (),           // 0x48
    nothing as *const (),           // 0x50
    nothing as *const (),           // 0x58
    nothing as *const (),           // 0x60
    nothing as *const (),           // 0x68
    key_count as *const (),         // 0x70
    key_state as *const (),         // 0x78
    mouse_state as *const (),       // 0x80
    nothing as *const (),           // 0x88 touch
    nothing as *const (),           // 0x90
    nothing as *const (),           // 0x98 motion
    nothing as *const (),           // 0xa0 arcade stick
    nothing as *const (),           // 0xa8 flight stick
    gamepad_state as *const (),     // 0xb0
    nothing as *const (),           // 0xb8 racing wheel
    nothing as *const (),           // 0xc0 UI navigation
]);

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "gameinput",
        dlls: &["gameinput.dll"],
        // Titles import it by ordinal only.
        exports: vec![HleExport {
            name: "#1",
            address: GameInputCreate as *const () as usize,
        }],
    }
}

/// Host input: XInput pads, the keyboard while our process has focus.
#[cfg(windows)]
mod host {
    use std::time::{Duration, Instant};

    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW,
    };
    use windows_sys::Win32::UI::Input::XboxController::{
        XINPUT_GAMEPAD, XINPUT_GAMEPAD_A, XINPUT_GAMEPAD_B, XINPUT_GAMEPAD_BACK,
        XINPUT_GAMEPAD_DPAD_DOWN, XINPUT_GAMEPAD_DPAD_LEFT, XINPUT_GAMEPAD_DPAD_RIGHT,
        XINPUT_GAMEPAD_DPAD_UP, XINPUT_GAMEPAD_LEFT_SHOULDER, XINPUT_GAMEPAD_LEFT_THUMB,
        XINPUT_GAMEPAD_RIGHT_SHOULDER, XINPUT_GAMEPAD_RIGHT_THUMB, XINPUT_GAMEPAD_START,
        XINPUT_GAMEPAD_X, XINPUT_GAMEPAD_Y, XINPUT_STATE, XINPUT_VIBRATION, XInputGetState,
        XInputSetState,
    };
    use std::sync::atomic::AtomicBool;

    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        ClipCursor, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, SetCursorPos,
    };

    use super::*;

    /// Asking XInput about an empty slot is slow; look again this often.
    const EMPTY_SLOT_RECHECK: Duration = Duration::from_secs(1);

    const BUTTONS: [(u16, u32); 14] = [
        (XINPUT_GAMEPAD_START, MENU),
        (XINPUT_GAMEPAD_BACK, VIEW),
        (XINPUT_GAMEPAD_A, A),
        (XINPUT_GAMEPAD_B, B),
        (XINPUT_GAMEPAD_X, X),
        (XINPUT_GAMEPAD_Y, Y),
        (XINPUT_GAMEPAD_DPAD_UP, UP),
        (XINPUT_GAMEPAD_DPAD_DOWN, DOWN),
        (XINPUT_GAMEPAD_DPAD_LEFT, LEFT),
        (XINPUT_GAMEPAD_DPAD_RIGHT, RIGHT),
        (XINPUT_GAMEPAD_LEFT_SHOULDER, LEFT_SHOULDER),
        (XINPUT_GAMEPAD_RIGHT_SHOULDER, RIGHT_SHOULDER),
        (XINPUT_GAMEPAD_LEFT_THUMB, LEFT_STICK),
        (XINPUT_GAMEPAD_RIGHT_THUMB, RIGHT_STICK),
    ];

    /// Raw values, no dead zone: titles apply their own.
    pub(super) fn convert(pad: &XINPUT_GAMEPAD) -> PadState {
        let stick = |v: i16| (f32::from(v) / 32767.0).max(-1.0);
        PadState {
            buttons: BUTTONS
                .iter()
                .filter(|(host, _)| pad.wButtons & host != 0)
                .fold(0, |all, (_, bit)| all | bit),
            left_trigger: f32::from(pad.bLeftTrigger) / 255.0,
            right_trigger: f32::from(pad.bRightTrigger) / 255.0,
            left_x: stick(pad.sThumbLX),
            left_y: stick(pad.sThumbLY),
            right_x: stick(pad.sThumbRX),
            right_y: stick(pad.sThumbRY),
        }
    }

    #[derive(Default)]
    pub(super) struct Host {
        empty_until: [Option<Instant>; PADS],
        /// F8 was down at the last poll.
        f8: bool,
        /// F8 let the pointer go.
        released: bool,
        /// The pointer was ours at the last poll.
        captured: bool,
        /// Mouse buttons play: all were up once since the pointer was
        /// taken (the click that focused the window does not fire).
        armed: bool,
    }

    /// The pointer is ours: the title's window hides it (WM_SETCURSOR).
    pub(super) static CAPTURED: AtomicBool = AtomicBool::new(false);

    const VK_F8: i32 = 0x77;

    /// A small box at the centre of `window`, to hold the pointer in.
    fn centre(window: HWND) -> Option<RECT> {
        let mut rect = RECT::default();
        // SAFETY: plain query into `rect`.
        if unsafe { GetWindowRect(window, &mut rect) } == 0 {
            return None;
        }
        let (x, y) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
        Some(RECT {
            left: x,
            top: y,
            right: x + 1,
            bottom: y + 1,
        })
    }

    impl Host {
        /// Takes or lets go of the pointer; whether it is ours now.
        fn capture(&mut self, wanted: bool) -> bool {
            // SAFETY: plain query.
            let window = (wanted && focused()).then(|| unsafe { GetForegroundWindow() });
            let rect = window.filter(|_| !self.released).and_then(centre);
            let captured = rect.is_some();
            // SAFETY: plain pointer calls with a valid box or none.
            unsafe {
                if let Some(rect) = rect {
                    if !self.captured {
                        SetCursorPos(rect.left, rect.top);
                        self.armed = false;
                    }
                    // Again every poll: the system drops the clip when
                    // windows move or focus changes.
                    ClipCursor(&rect);
                } else if self.captured {
                    ClipCursor(std::ptr::null());
                }
            }
            if captured != self.captured {
                tracing::info!(captured, "mouse pointer");
            }
            self.captured = captured;
            CAPTURED.store(captured, Ordering::Relaxed);
            captured
        }
    }

    impl Source for Host {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            if self.empty_until[slot].is_some_and(|until| Instant::now() < until) {
                return None;
            }
            let mut state = XINPUT_STATE::default();
            // SAFETY: room for the state.
            if unsafe { XInputGetState(slot as u32, &mut state) } != 0 {
                self.empty_until[slot] = Some(Instant::now() + EMPTY_SLOT_RECHECK);
                return None;
            }
            self.empty_until[slot] = None;
            #[allow(unused_mut)]
            let mut pad = convert(&state.Gamepad);
            // The scripted keys (`VICTORY_KEYS`) also press the first
            // pad's buttons, by the stand-in pad's table: with a real pad
            // connected the stand-in is not there, and a title that listens
            // to the pad (the title's start screen) saw nothing.
            #[cfg(feature = "diagnostics")]
            if slot == 0 {
                pad.buttons |= stand_in(Binds::Classic.table(false), &scripted()).buttons;
            }
            Some(pad)
        }

        fn keys(&mut self) -> Vec<KeyState> {
            #[cfg(feature = "diagnostics")]
            let mut down = scripted();
            #[cfg(not(feature = "diagnostics"))]
            let mut down = Vec::new();
            if focused() {
                // Left/right modifier keys only, as devices report them; no
                // mouse buttons (below 0x08).
                let held: Vec<u8> = (0x08..=0xFE_u8)
                    .filter(|vk| !(0x10..=0x12).contains(vk) && !down.contains(vk))
                    // SAFETY: plain queries.
                    .filter(|&vk| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0)
                    .collect();
                down.extend(held);
            }
            down.into_iter()
                .map(|vk| KeyState {
                    // SAFETY: plain query.
                    scan_code: unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC_EX) },
                    virtual_key: vk,
                    // shortcut: no characters (code point 0);
                    // the title reads scan codes and virtual keys only.
                    ..Default::default()
                })
                .collect()
        }

        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            let speeds = XINPUT_VIBRATION {
                wLeftMotorSpeed: low,
                wRightMotorSpeed: high,
            };
            // SAFETY: a valid vibration; an empty slot just fails.
            unsafe { XInputSetState(slot as u32, &speeds) };
        }

        fn mouse(&mut self, capture: bool) -> MouseInput {
            raw::start();
            let (dx, dy, wheel) = raw::take();
            let focused = focused();
            if capture {
                // SAFETY: plain query.
                let f8 = focused && unsafe { GetAsyncKeyState(VK_F8) } < 0;
                if f8 && !self.f8 {
                    self.released = !self.released;
                }
                self.f8 = f8;
            }
            let captured = self.capture(capture);
            if !focused {
                return MouseInput::default();
            }
            let buttons = MOUSE_VKS
                .iter()
                .enumerate()
                // SAFETY: plain queries.
                .filter(|(_, vk)| unsafe { GetAsyncKeyState(i32::from(**vk)) } < 0)
                .fold(0, |all, (bit, _)| all | 1 << bit);
            if captured && buttons == 0 {
                self.armed = true;
            }
            MouseInput {
                dx,
                dy,
                wheel,
                buttons,
                captured: captured && self.armed,
            }
        }
    }

    /// Raw mouse travel (WM_INPUT), unaccelerated and not stopped by the
    /// screen's edge, read on a thread of its own through a message-only
    /// window. Summed until the next poll takes it.
    mod raw {
        use std::sync::Once;
        use std::sync::atomic::{AtomicI64, Ordering};

        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::Input::{
            GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE,
            RAWINPUTHEADER, RID_INPUT, RIDEV_INPUTSINK, RIM_TYPEMOUSE, RegisterRawInputDevices,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG, RI_MOUSE_WHEEL,
            WM_INPUT,
        };
        use windows_sys::core::w;

        static DX: AtomicI64 = AtomicI64::new(0);
        static DY: AtomicI64 = AtomicI64::new(0);
        static WHEEL: AtomicI64 = AtomicI64::new(0);

        pub(super) fn start() {
            static START: Once = Once::new();
            START.call_once(|| {
                let spawned = std::thread::Builder::new()
                    .name("victory raw mouse".into())
                    .spawn(run);
                if let Err(error) = spawned {
                    tracing::warn!(%error, "no raw mouse input");
                }
            });
        }

        pub(super) fn take() -> (i64, i64, i64) {
            (
                DX.swap(0, Ordering::Relaxed),
                DY.swap(0, Ordering::Relaxed),
                WHEEL.swap(0, Ordering::Relaxed),
            )
        }

        fn run() {
            // SAFETY: a message-only window of a system class on this
            // thread, its messages pumped here until the process ends.
            unsafe {
                let instance = GetModuleHandleW(std::ptr::null());
                let window = CreateWindowExW(
                    0,
                    w!("STATIC"),
                    w!("victory raw mouse"),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                );
                if window.is_null() {
                    tracing::warn!("no raw mouse input: no window");
                    return;
                }
                // Generic desktop page, mouse; also while not in front (the
                // poll drops travel then).
                let device = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: window,
                };
                if RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) == 0 {
                    tracing::warn!("no raw mouse input: registration refused");
                    return;
                }
                tracing::info!("raw mouse input");
                let mut message = MSG::default();
                while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                    if message.message == WM_INPUT {
                        read(message.lParam as HRAWINPUT);
                    }
                    // The default procedure frees the input.
                    DispatchMessageW(&message);
                }
            }
        }

        fn read(input: HRAWINPUT) {
            let mut report = RAWINPUT::default();
            let mut size = size_of::<RAWINPUT>() as u32;
            // SAFETY: room for `size` bytes; a mouse report fits.
            let got = unsafe {
                GetRawInputData(
                    input,
                    RID_INPUT,
                    (&raw mut report).cast(),
                    &mut size,
                    size_of::<RAWINPUTHEADER>() as u32,
                )
            };
            if got == u32::MAX || report.header.dwType != RIM_TYPEMOUSE {
                return;
            }
            // SAFETY: a mouse report, as the header says.
            let mouse = unsafe { report.data.mouse };
            // Tablets and remote desktops report positions, not travel.
            if mouse.usFlags & MOUSE_MOVE_ABSOLUTE == 0 {
                DX.fetch_add(i64::from(mouse.lLastX), Ordering::Relaxed);
                DY.fetch_add(i64::from(mouse.lLastY), Ordering::Relaxed);
            }
            // SAFETY: plain fields of the report.
            let (flags, data) = unsafe {
                let b = mouse.Anonymous.Anonymous;
                (b.usButtonFlags, b.usButtonData)
            };
            if u32::from(flags) & RI_MOUSE_WHEEL != 0 {
                WHEEL.fetch_add(i64::from(data as i16), Ordering::Relaxed);
            }
        }
    }

    /// Debugging aid: `VICTORY_KEYS=<seconds>:<key>,...` holds each key for
    /// 0.2 s that long after input starts (`<from>-<to>:<key>`: all that
    /// time), with or without focus. A key is a
    /// letter, a digit, or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`,
    /// `Left`, `Right` (the stand-in pad's table maps them to buttons).
    #[cfg(feature = "diagnostics")]
    fn scripted() -> Vec<u8> {
        static SCRIPT: LazyLock<(Instant, Vec<(Duration, Duration, u8)>)> = LazyLock::new(|| {
            let list = std::env::var("VICTORY_KEYS").unwrap_or_default();
            let keys = list.split(',').filter_map(|entry| {
                let (at, key) = entry.split_once(':')?;
                let vk = match key.trim() {
                    "Enter" => 0x0D,
                    "Esc" => 0x1B,
                    "Tab" => 0x09,
                    "Back" => 0x08,
                    "Up" => 0x26,
                    "Down" => 0x28,
                    "Left" => 0x25,
                    "Right" => 0x27,
                    k if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() => {
                        k.as_bytes()[0].to_ascii_uppercase()
                    }
                    _ => return None,
                };
                let time = |t: &str| Duration::try_from_secs_f64(t.trim().parse().ok()?).ok();
                let (from, to) = match at.split_once('-') {
                    Some((from, to)) => (time(from)?, time(to)?),
                    None => (time(at)?, time(at)? + Duration::from_millis(200)),
                };
                Some((from, to, vk))
            });
            (Instant::now(), keys.collect())
        });
        let (start, keys) = &*SCRIPT;
        let now = start.elapsed();
        keys.iter()
            .filter(|&&(from, to, _)| (from..to).contains(&now))
            .map(|&(_, _, vk)| vk)
            .collect()
    }

    /// The foreground window is one of ours (the title's or our fallback).
    fn focused() -> bool {
        let mut process = 0;
        // SAFETY: plain queries; a null window gives process 0.
        unsafe {
            GetWindowThreadProcessId(GetForegroundWindow(), &mut process);
            process == GetCurrentProcessId()
        }
    }

    #[test]
    fn host_pads_convert_to_console_pads() {
        let pad = XINPUT_GAMEPAD {
            wButtons: XINPUT_GAMEPAD_A | XINPUT_GAMEPAD_START | XINPUT_GAMEPAD_DPAD_LEFT,
            bLeftTrigger: 255,
            sThumbLX: -32768,
            sThumbLY: 32767,
            ..Default::default()
        };
        let state = convert(&pad);
        assert_eq!(state.buttons, A | MENU | LEFT);
        assert_eq!(
            (
                state.left_trigger,
                state.left_x,
                state.left_y,
                state.right_x
            ),
            (1.0, -1.0, 1.0, 0.0)
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::HleResolver;
    use crate::HostPolicy;
    use victory_loader::Provenance;
    use victory_pe::ImportSymbol;

    #[derive(Default)]
    struct FakeHost {
        pads: [Option<PadState>; PADS],
        keys: Vec<KeyState>,
        rumble: Vec<(usize, u16, u16)>,
    }

    struct Fake(Arc<Mutex<FakeHost>>);

    impl Source for Fake {
        fn pad(&mut self, slot: usize) -> Option<PadState> {
            self.0.lock().unwrap().pads[slot]
        }
        fn keys(&mut self) -> Vec<KeyState> {
            self.0.lock().unwrap().keys.clone()
        }
        fn rumble(&mut self, slot: usize, low: u16, high: u16) {
            self.0.lock().unwrap().rumble.push((slot, low, high));
        }
    }

    fn fake() -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        fake_with(false)
    }

    fn fake_with(one_pad: bool) -> (&'static Hub, Arc<Mutex<FakeHost>>) {
        let host = Arc::new(Mutex::new(FakeHost::default()));
        let hub = Hub::new(Box::new(Fake(Arc::clone(&host))), one_pad);
        hub.poll();
        (hub, host)
    }

    fn key(virtual_key: u8) -> KeyState {
        KeyState {
            scan_code: u32::from(virtual_key) + 0x100,
            virtual_key,
            ..Default::default()
        }
    }

    fn slot<F: Copy, T>(object: *const T, offset: usize) -> F {
        // SAFETY: our objects start with their table, which has the slot.
        let f = unsafe { *(*(object as *const *const *const ())).add(offset / 8) };
        // SAFETY: the caller names the slot's signature.
        unsafe { std::mem::transmute_copy(&f) }
    }

    /// Device callbacks seen: device index, current and previous status.
    type Log = Mutex<Vec<(usize, u32, u32)>>;

    extern "system" fn record(
        _token: u64,
        context: *mut c_void,
        device: *const Device,
        _timestamp: u64,
        current: u32,
        previous: u32,
    ) {
        // SAFETY: the test passes its log and one of the hub's devices.
        let (log, index) = unsafe { (&*(context as *const Log), (*device).index) };
        log.lock().unwrap().push((index, current, previous));
    }

    fn register(hub: &Hub, kind: u32, enumeration: u32, log: &Log) -> u64 {
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let mut token = 0;
        let context = log as *const Log as *mut c_void;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let hr = register(
            &hub.object,
            std::ptr::null(),
            kind,
            1,
            enumeration,
            context,
            Some(record),
            &mut token,
        );
        assert_eq!(hr, S_OK);
        assert_ne!(token, 0);
        token
    }

    fn result(hr: HRESULT, out: *const c_void) -> Result<*const Reading, HRESULT> {
        if hr == S_OK {
            Ok(out.cast())
        } else {
            assert!(out.is_null());
            Err(hr)
        }
    }

    fn current(hub: &Hub, kind: u32, device: usize) -> Result<*const Reading, HRESULT> {
        type Current = extern "system" fn(This, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let hr = slot::<Current, _>(&hub.object, 0x20)(
            &hub.object,
            kind,
            &hub.devices[device],
            &mut out,
        );
        result(hr, out)
    }

    fn walk(hub: &Hub, offset: usize, from: *const Reading) -> Result<*const Reading, HRESULT> {
        type Walk = extern "system" fn(This, *const Reading, u32, *const Device, Out) -> HRESULT;
        let mut out = std::ptr::null();
        let device = reading(from).device;
        let hr =
            slot::<Walk, _>(&hub.object, offset)(&hub.object, from, GAMEPAD_KIND, device, &mut out);
        result(hr, out)
    }

    fn pad(reading: *const Reading) -> PadState {
        let mut state = PadState::default();
        let get =
            slot::<extern "system" fn(*const Reading, *mut PadState) -> bool, _>(reading, 0xb0);
        assert!(get(reading, &mut state));
        state
    }

    fn stamp(reading: *const Reading) -> (u64, u64) {
        let time = slot::<extern "system" fn(*const Reading) -> u64, _>(reading, 0x28);
        let sequence = slot::<extern "system" fn(*const Reading, u32) -> u64, _>(reading, 0x20);
        (time(reading), sequence(reading, GAMEPAD_KIND))
    }

    fn release(reading: *const Reading) {
        slot::<extern "system" fn(*const Reading) -> u32, _>(reading, 0x10)(reading);
    }

    #[test]
    fn ordinal_one_resolves_to_create() {
        let r = HleResolver::new(HostPolicy::none());
        assert_eq!(
            r.classify("GameInput.dll", &ImportSymbol::Ordinal(1)),
            Some(Provenance::Hle)
        );
        assert_eq!(r.classify("GameInput.dll", &ImportSymbol::Ordinal(2)), None);
    }

    #[test]
    fn pads_connect_report_and_disconnect() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        // Blocking enumeration ran before registration returned: with no
        // host pad the keyboard's stand-in is the only one.
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        host.lock().unwrap().pads[1] = Some(PadState {
            buttons: A,
            left_x: 0.5,
            ..Default::default()
        });
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[1..],
            [(1, STATUS_CONNECTED, 0), (FALLBACK, 0, STATUS_CONNECTED)]
        );

        let first = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(first).buttons, A);
        assert_eq!(pad(first).left_x, 0.5);
        // No change, no new reading.
        hub.poll();
        let again = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(again, first);
        release(again);

        std::thread::sleep(Duration::from_millis(2));
        host.lock().unwrap().pads[1].as_mut().unwrap().buttons = B;
        hub.poll();
        let second = current(hub, GAMEPAD_KIND, 1).unwrap();
        assert_eq!(pad(second).buttons, B);
        let (t1, s1) = stamp(first);
        let (t2, s2) = stamp(second);
        assert!(t2 > t1 && s2 == s1 + 1);
        assert!(t2 <= now());

        // The title's edge check: previous of current, and back again.
        assert_eq!(walk(hub, 0x30, second), Ok(first));
        assert_eq!(walk(hub, 0x28, first), Ok(second));
        assert_eq!(walk(hub, 0x28, second), Err(READING_NOT_FOUND));
        assert_eq!(walk(hub, 0x30, first), Err(READING_NOT_FOUND));
        for r in [first, first, second, second] {
            release(r);
        }

        host.lock().unwrap().pads[1] = None;
        hub.poll();
        assert_eq!(
            log.lock().unwrap()[3..],
            [(1, 0, STATUS_CONNECTED), (FALLBACK, STATUS_CONNECTED, 0)]
        );
        assert_eq!(current(hub, GAMEPAD_KIND, 1), Err(DEVICE_DISCONNECTED));
        // The stand-in is back, at rest.
        let rest = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(pad(rest), PadState::default());
        release(rest);
    }

    #[test]
    fn keys_feed_the_keyboard_and_the_stand_in_pad() {
        let (hub, host) = fake();
        let log = Log::default();
        register(hub, KEYBOARD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(KEYBOARD, STATUS_CONNECTED, 0)]);

        let keys = vec![key(b'W'), key(0x0D), key(b'L'), key(b'C')];
        host.lock().unwrap().keys = keys.clone();
        hub.poll();
        let r = current(hub, KEYBOARD_KIND, KEYBOARD).unwrap();
        let count = slot::<extern "system" fn(*const Reading) -> u32, _>(r, 0x70);
        let state =
            slot::<extern "system" fn(*const Reading, u32, *mut KeyState) -> u32, _>(r, 0x78);
        assert_eq!(count(r), 4);
        let mut got = [KeyState::default(); 8];
        assert_eq!(state(r, 8, got.as_mut_ptr()), 4);
        assert_eq!(got[..4], keys[..]);
        release(r);

        let r = current(hub, GAMEPAD_KIND, FALLBACK).unwrap();
        assert_eq!(
            pad(r),
            PadState {
                buttons: A,
                left_y: 1.0,
                right_x: 1.0,
                right_trigger: 1.0,
                ..Default::default()
            }
        );
        release(r);
        // A keyboard reading is no gamepad reading.
        assert_eq!(current(hub, GAMEPAD_KIND, KEYBOARD), Err(READING_NOT_FOUND));
    }

    #[test]
    fn a_host_pad_feeds_the_one_pad_which_never_goes() {
        let (hub, host) = fake_with(true);
        let log = Log::default();
        register(hub, GAMEPAD_KIND, 2, &log);
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);
        let pad = |hub: &Hub| match &hub.state().slots[FALLBACK].history.back().unwrap().value {
            Value::Pad(p) => *p,
            Value::Keys(_) | Value::Mouse(_) => unreachable!(),
        };
        host.lock().unwrap().keys = vec![key(b'D')];
        hub.poll();
        assert_eq!(pad(hub).left_x, 1.0);
        // A pad is switched on, in any slot: its input arrives, beside the
        // keyboard's, and no device comes or goes.
        host.lock().unwrap().pads[2] = Some(PadState { buttons: A, left_y: -0.5, ..Default::default() });
        hub.poll();
        let p = pad(hub);
        assert_eq!((p.buttons, p.left_x, p.left_y), (A, 1.0, -0.5));
        let device: *const Device = &hub.devices[FALLBACK];
        set_rumble(device, &[1.0, 0.0, 0.0, 0.0]);
        assert_eq!(host.lock().unwrap().rumble, [(2, u16::MAX, 0)]);
        // And off again.
        host.lock().unwrap().pads[2] = None;
        host.lock().unwrap().keys.clear();
        hub.poll();
        assert_eq!(pad(hub), PadState::default());
        assert_eq!(log.lock().unwrap().len(), 1);
        assert!(!hub.state().slots[2].connected);
    }

    #[test]
    fn async_enumeration_waits_for_the_poll_and_unregistering_stops_callbacks() {
        let (hub, host) = fake();
        let log = Log::default();
        let token = register(hub, GAMEPAD_KIND, ASYNC_ENUMERATION, &log);
        assert!(log.lock().unwrap().is_empty());
        hub.poll();
        assert_eq!(*log.lock().unwrap(), [(FALLBACK, STATUS_CONNECTED, 0)]);

        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(unregister(&hub.object, token, 5000));
        host.lock().unwrap().pads[0] = Some(PadState::default());
        hub.poll();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[test]
    fn unregistering_waits_for_a_running_callback() {
        use std::sync::atomic::AtomicBool;
        static INSIDE: AtomicBool = AtomicBool::new(false);
        static LEAVE: AtomicBool = AtomicBool::new(false);
        static FROM_INSIDE: AtomicU32 = AtomicU32::new(0);
        extern "system" fn slow(
            token: u64,
            context: *mut c_void,
            _: *const Device,
            _: u64,
            _: u32,
            _: u32,
        ) {
            // From inside its own callback, unregistering fails at once.
            let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(context, 0x68);
            if !unregister(context.cast(), token, 0) {
                FROM_INSIDE.fetch_add(1, Ordering::SeqCst);
            }
            INSIDE.store(true, Ordering::SeqCst);
            while !LEAVE.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let (hub, host) = fake();
        type Register = extern "system" fn(
            This,
            *const Device,
            u32,
            u32,
            u32,
            *mut c_void,
            Option<DeviceCallback>,
            *mut u64,
        ) -> HRESULT;
        let register = slot::<Register, _>(&hub.object, 0x48);
        let mut token = 0;
        let context = (&raw const hub.object).cast_mut().cast();
        assert_eq!(
            register(&hub.object, std::ptr::null(), GAMEPAD_KIND, 1, 0, context, Some(slow), &mut token),
            S_OK
        );
        host.lock().unwrap().pads[1] = Some(PadState::default());
        let poller = std::thread::spawn(|| hub.poll());
        while !INSIDE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        let unregister = slot::<extern "system" fn(This, u64, u64) -> bool, _>(&hub.object, 0x68);
        assert!(!unregister(&hub.object, token, 5000), "times out while it runs");
        let leave = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            LEAVE.store(true, Ordering::SeqCst);
        });
        let start = Instant::now();
        assert!(unregister(&hub.object, token, u64::MAX), "waits for it");
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(FROM_INSIDE.load(Ordering::SeqCst), 1);
        poller.join().unwrap();
        leave.join().unwrap();
    }

    fn vks(keys: &[u8]) -> Vec<KeyState> {
        keys.iter().map(|&vk| key(vk)).collect()
    }

    #[test]
    fn classic_binds_are_the_old_table() {
        let mut controls = Controls::default();
        let pad = controls.pad(&vks(b"QEFHXYZCWDJ"), MouseInput::default(), 0);
        assert_eq!(
            pad,
            PadState {
                buttons: LEFT_SHOULDER | RIGHT_SHOULDER | LEFT_STICK | RIGHT_STICK | X | Y,
                left_trigger: 1.0,
                right_trigger: 1.0,
                left_x: 1.0,
                left_y: 1.0,
                right_x: -1.0,
                right_y: 0.0,
            }
        );
        // Opposite keys cancel; mouse buttons do nothing without mouse look.
        let mouse = MouseInput { buttons: 0b11, captured: true, ..Default::default() };
        assert_eq!(controls.pad(&vks(b"AD"), mouse, 8000), PadState::default());
    }

    #[test]
    fn rockstar_binds_follow_the_pc_defaults_and_f_switches_to_driving() {
        use victory_core::pad::{
            VK_F7 as F7, VK_LCONTROL as LCTRL, VK_LSHIFT as LSHIFT, VK_SPACE as SPACE,
            VK_TAB as TAB,
        };
        let mut controls = Controls {
            binds: Binds::Rockstar,
            look: Some(Look::new(1.0, false, 0.2)),
            ..Default::default()
        };
        let captured = |buttons| MouseInput { buttons, captured: true, ..Default::default() };
        let mut t = 0;
        let mut pad = |keys: &[u8], mouse: MouseInput| {
            t += 8000;
            controls.pad(&vks(keys), mouse, t)
        };
        // On foot: W forward, Shift sprint (A), Space jump (X), right mouse
        // aims (LT), left fires (RT).
        let p = pad(&[b'W', LSHIFT, SPACE], captured(0b11));
        assert_eq!((p.left_y, p.buttons), (1.0, A | X));
        assert_eq!((p.left_trigger, p.right_trigger), (1.0, 1.0));
        // E is A on foot (confirm, interact); D-pad right is the right arrow.
        let p = pad(&[b'R', b'E', b'Q', TAB, LCTRL, b'C', b'V'], captured(0));
        assert_eq!(
            p.buttons,
            B | A | RIGHT_SHOULDER | LEFT_SHOULDER | LEFT_STICK | RIGHT_STICK | VIEW
        );
        // Mouse buttons need the pointer to be ours.
        let p = pad(&[], MouseInput { buttons: 0b01, ..Default::default() });
        assert_eq!(p.right_trigger, 0.0);
        // F enters the vehicle (Y) and switches the keys: W and S pedals,
        // Space the handbrake, E the horn.
        assert_eq!(pad(b"F", captured(0)).buttons, Y);
        assert_eq!(pad(b"F", captured(0)).buttons, Y, "held, no second switch");
        let p = pad(&[b'W', SPACE, b'E', b'D'], captured(0));
        assert_eq!((p.right_trigger, p.left_y, p.left_x), (1.0, 0.0, 1.0));
        assert_eq!(p.buttons, RIGHT_SHOULDER | LEFT_STICK);
        assert_eq!(pad(b"S", captured(0)).left_trigger, 1.0);
        // F again leaves it and is back on foot; F7 switches without a Y.
        pad(b"F", captured(0));
        assert_eq!(pad(b"W", captured(0)).left_y, 1.0);
        assert_eq!(pad(&[F7], captured(0)).buttons, 0);
        assert_eq!(pad(b"W", captured(0)).right_trigger, 1.0);
    }

    #[test]
    fn mouse_travel_turns_the_stick_and_it_comes_back_to_rest() {
        let mut look = Look::new(1.0, false, 0.2);
        // 1000 counts a second is a full deflection: 4 counts every 8 ms
        // settle at half of it, lifted past the dead zone.
        let mut last = (0.0, 0.0);
        for _ in 0..40 {
            last = look.step(4, 0, 0.008);
        }
        assert!((last.0 - (0.2 + 0.8 * 0.5)).abs() < 0.01, "{last:?}");
        assert_eq!(last.1, 0.0);
        // Mouse down is stick down, unless inverted.
        let (_, y) = Look::new(1.0, false, 0.2).step(0, 5, 0.008);
        assert!(y < -0.2);
        let (_, y) = Look::new(1.0, true, 0.2).step(0, 5, 0.008);
        assert!(y > 0.2);
        // Fast travel is a full deflection, no more.
        let (x, y) = Look::new(1.0, false, 0.2).step(-500, -500, 0.008);
        assert!((x.hypot(y) - 1.0).abs() < 1e-4 && x < 0.0 && y > 0.0);
        // The mouse stops: the stick is at rest within a few polls.
        let polls = (0..100).position(|_| look.step(0, 0, 0.008) == (0.0, 0.0));
        assert!(polls.is_some_and(|n| n <= 12), "{polls:?}");
        // One count is a small deflection past the dead zone, not nothing.
        let (x, _) = look.step(1, 0, 0.008);
        assert!(x > 0.2 && x < 0.3, "{x}");
        // Higher sensitivity, more deflection for the same travel.
        let slow = Look::new(1.0, false, 0.0).step(2, 0, 0.008).0;
        let fast = Look::new(2.0, false, 0.0).step(2, 0, 0.008).0;
        assert!((fast - 2.0 * slow).abs() < 1e-4);
    }

    #[test]
    fn mouse_look_feeds_the_stand_in_pad_only_while_captured() {
        let look = Some(Look::new(1.0, false, 0.2));
        let mut controls = Controls { look, ..Default::default() };
        let free = MouseInput { dx: 50, ..Default::default() };
        assert_eq!(controls.pad(&[], free, 8000).right_x, 0.0);
        let held = MouseInput { dx: 50, captured: true, ..Default::default() };
        assert!(controls.pad(&[], held, 16000).right_x > 0.5);
        // Keys still turn it too, added to the mouse's and clamped.
        let mut controls = Controls { look: Some(Look::new(1.0, false, 0.2)), ..Default::default() };
        let still = MouseInput { captured: true, ..Default::default() };
        assert_eq!(controls.pad(&vks(b"J"), still, 8000).right_x, -1.0);
        assert_eq!(controls.pad(&vks(b"L"), held, 16000).right_x, 1.0);
    }

    #[test]
    fn the_mouse_device_reports_summed_travel_when_switched_on() {
        struct Mice(MouseInput);
        impl Source for Mice {
            fn pad(&mut self, _: usize) -> Option<PadState> {
                None
            }
            fn keys(&mut self) -> Vec<KeyState> {
                Vec::new()
            }
            fn rumble(&mut self, _: usize, _: u16, _: u16) {}
            fn mouse(&mut self, capture: bool) -> MouseInput {
                assert!(!capture, "no mouse look, no capture");
                self.0
            }
        }
        let mouse = MouseInput { dx: 3, dy: -2, wheel: 120, buttons: 0b101, captured: false };
        let controls = Controls { mouse_device: true, ..Default::default() };
        let hub = Hub::with(Box::new(Mice(mouse)), true, controls);
        hub.poll();
        hub.poll();
        let r = current(hub, MOUSE_KIND, MOUSE).unwrap();
        let get = slot::<extern "system" fn(*const Reading, *mut MouseState) -> bool, _>(r, 0x80);
        let mut state = MouseState::default();
        assert!(get(r, &mut state));
        assert_eq!(state, MouseState { buttons: 0b101, x: 6, y: -4, wheel_x: 0, wheel_y: 240 });
        release(r);
        // Off (the default), there is no mouse.
        let (hub, _) = fake_with(true);
        assert_eq!(current(hub, MOUSE_KIND, MOUSE), Err(DEVICE_DISCONNECTED));
    }

    #[test]
    fn devices_are_found_by_id_and_rumble_reaches_the_host() {
        let (hub, host) = fake();
        host.lock().unwrap().pads[2] = Some(PadState::default());
        hub.poll();
        let device = &hub.devices[2] as *const Device;
        let info = slot::<extern "system" fn(*const Device) -> *const u8, _>(device, 0x18);
        // SAFETY: the info has the id at +0x20.
        let id = unsafe { *(info(device).add(0x20) as *const [u8; 32]) };
        assert_ne!(id, [0; 32]);

        type Find = extern "system" fn(This, *const [u8; 32], Out) -> HRESULT;
        let find = slot::<Find, _>(&hub.object, 0x80);
        let mut out = std::ptr::null();
        assert_eq!(find(&hub.object, &id, &mut out), S_OK);
        assert_eq!(out, device.cast());
        assert_eq!(find(&hub.object, &[1; 32], &mut out), DEVICE_NOT_FOUND);
        assert!(out.is_null());

        type Rumble = extern "system" fn(*const Device, *const [f32; 4]);
        let rumble = slot::<Rumble, _>(device, 0x50);
        rumble(device, &[0.5, 1.0, 1.0, 1.0]);
        rumble(&hub.devices[FALLBACK], &[1.0; 4]);
        rumble(device, std::ptr::null());
        assert_eq!(host.lock().unwrap().rumble, [(2, 32767, 65535), (2, 0, 0)]);
    }
}
