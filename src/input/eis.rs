// SPDX-License-Identifier: GPL-3.0-only

//! EIS (Emulated Input Server) receiver for remote desktop input injection.
//!
//! Accepts input events from EIS clients (e.g., xdg-desktop-portal-cosmic
//! RemoteDesktop sessions) and injects them into the compositor's input stack
//! using the same Smithay APIs as the normal input pipeline.
//!
//! Uses `reis::calloop::EisRequestSource` to process EIS protocol events
//! directly on the compositor's calloop event loop (no background threads).

use reis::{calloop::EisRequestSourceEvent, eis, event::DeviceCapability, request::EisRequest};
use smithay::{
    backend::input::{KeyState, TouchSlot},
    input::{
        keyboard::{FilterResult, Keycode},
        touch::{DownEvent, MotionEvent as TouchMotionEvent, UpEvent},
    },
    utils::SERIAL_COUNTER,
};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, error, info, warn};

use crate::shell::SeatExt;
use crate::state::State;
use crate::utils::geometry::{Global, PointExt, PointGlobalExt};
use crate::utils::prelude::OutputExt;

/// Maximum number of concurrent EIS connections allowed.
const MAX_EIS_CONNECTIONS: usize = 8;

/// Maximum valid evdev keycode (KEY_MAX from linux/input-event-codes.h).
const MAX_EVDEV_KEYCODE: u32 = 0x2FF;

/// Maximum touch slot ID (generous upper bound; real devices rarely exceed 20).
const MAX_TOUCH_ID: u32 = 256;

/// Manages EIS connections on the compositor's calloop event loop.
#[derive(Debug)]
pub struct EisState {
    evlh: calloop::LoopHandle<'static, State>,
    active_connections: AtomicUsize,
}

impl EisState {
    /// Create a new EIS state.
    pub fn new(evlh: &calloop::LoopHandle<'static, State>) -> anyhow::Result<Self> {
        info!("EIS input receiver initialized");
        Ok(Self {
            evlh: evlh.clone(),
            active_connections: AtomicUsize::new(0),
        })
    }

    /// Accept a new EIS client connection from a UNIX socket fd.
    ///
    /// Creates an `EisRequestSource` calloop event source that processes the
    /// EIS protocol directly on the compositor's event loop. No background
    /// threads are spawned.
    pub fn add_connection(&self, socket: UnixStream) {
        let current = self.active_connections.load(Ordering::Acquire);
        if current >= MAX_EIS_CONNECTIONS {
            warn!(
                current,
                max = MAX_EIS_CONNECTIONS,
                "Rejecting EIS connection: limit reached"
            );
            return;
        }
        self.active_connections.fetch_add(1, Ordering::AcqRel);
        let active = self.active_connections.load(Ordering::Acquire);
        info!(active, "Accepting new EIS client connection");

        let context = match eis::Context::new(socket) {
            Ok(ctx) => ctx,
            Err(e) => {
                error!("Failed to create EIS context: {e}");
                self.active_connections.fetch_sub(1, Ordering::AcqRel);
                return;
            }
        };

        let source = reis::calloop::EisRequestSource::new(context, 0);

        if let Err(e) = self.evlh.insert_source(source, |event, connection, state| {
            match event {
                Ok(EisRequestSourceEvent::Connected) => {
                    // Truncate client name to prevent log flooding
                    let client_name: String = connection
                        .name()
                        .unwrap_or("<unknown>")
                        .chars()
                        .take(128)
                        .collect();
                    debug!(client = %client_name, "EIS client connected");

                    // Add a seat with all input capabilities
                    let _seat = connection.add_seat(
                        Some("seat0"),
                        DeviceCapability::Keyboard
                            | DeviceCapability::Pointer
                            | DeviceCapability::PointerAbsolute
                            | DeviceCapability::Button
                            | DeviceCapability::Scroll
                            | DeviceCapability::Touch,
                    );
                    if let Err(e) = connection.flush() {
                        warn!("Failed to flush EIS seat announcement: {e}");
                    }
                }
                Ok(EisRequestSourceEvent::Request(request)) => {
                    process_eis_request(state, connection, request);
                }
                Err(e) => {
                    warn!("EIS protocol error: {e}");
                }
            }
            Ok(calloop::PostAction::Continue)
        }) {
            error!("Failed to insert EIS calloop source: {}", e.error);
            self.active_connections.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Process a single EIS protocol request by injecting it into the compositor's
/// Smithay input stack.
fn process_eis_request(
    state: &mut State,
    connection: &mut reis::request::Connection,
    request: EisRequest,
) {
    let time = state.common.clock.now().as_millis();

    match request {
        EisRequest::KeyboardKey(key_evt) => {
            if key_evt.key > MAX_EVDEV_KEYCODE {
                warn!(
                    keycode = key_evt.key,
                    "Rejecting keyboard event: keycode out of range"
                );
                return;
            }
            let seat = state.common.shell.read().seats.last_active().clone();
            if let Some(keyboard) = seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                let key_state = if key_evt.state == eis::keyboard::KeyState::Press {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                keyboard.input(
                    state,
                    Keycode::new(key_evt.key),
                    key_state,
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward::<bool>,
                );
            }
        }
        EisRequest::PointerMotion(motion) => {
            let dx = f64::from(motion.dx);
            let dy = f64::from(motion.dy);
            if !dx.is_finite() || !dy.is_finite() {
                warn!("Rejecting pointer motion: non-finite delta");
                return;
            }

            let shell = state.common.shell.read();
            let seat = shell.seats.last_active().clone();
            if let Some(pointer) = seat.get_pointer() {
                let current = pointer.current_location().as_global();
                let mut position = current;
                position.x += dx;
                position.y += dy;

                // Clamp to output geometry
                let output = shell
                    .outputs()
                    .find(|o| o.geometry().to_f64().contains(position))
                    .cloned()
                    .unwrap_or_else(|| seat.active_output());
                let geom = output.geometry();
                position.x = position
                    .x
                    .clamp(geom.loc.x as f64, (geom.loc.x + geom.size.w - 1) as f64);
                position.y = position
                    .y
                    .clamp(geom.loc.y as f64, (geom.loc.y + geom.size.h - 1) as f64);

                // Compute surface under the new pointer position
                let under = State::surface_under(position, &output, &shell)
                    .map(|(target, pos)| (target, pos.as_logical()));

                let serial = SERIAL_COUNTER.next_serial();
                std::mem::drop(shell);
                pointer.motion(
                    state,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: position.as_logical(),
                        serial,
                        time,
                    },
                );
                pointer.frame(state);
            }
        }
        EisRequest::PointerMotionAbsolute(motion) => {
            let x = f64::from(motion.dx_absolute);
            let y = f64::from(motion.dy_absolute);
            if !x.is_finite() || !y.is_finite() {
                warn!("Rejecting absolute pointer motion: non-finite coordinates");
                return;
            }

            let shell = state.common.shell.read();
            let seat = shell.seats.last_active().clone();
            if let Some(pointer) = seat.get_pointer() {
                let position: smithay::utils::Point<f64, Global> = (x, y).into();

                // Find the output containing this position
                let output = shell
                    .outputs()
                    .find(|o| o.geometry().to_f64().contains(position))
                    .cloned()
                    .unwrap_or_else(|| seat.active_output());

                // Compute surface under the pointer position
                let under = State::surface_under(position, &output, &shell)
                    .map(|(target, pos)| (target, pos.as_logical()));

                let serial = SERIAL_COUNTER.next_serial();
                std::mem::drop(shell);
                pointer.motion(
                    state,
                    under,
                    &smithay::input::pointer::MotionEvent {
                        location: (x, y).into(),
                        serial,
                        time,
                    },
                );
                pointer.frame(state);
            }
        }
        EisRequest::Button(btn) => {
            if btn.button > MAX_EVDEV_KEYCODE {
                warn!(
                    button = btn.button,
                    "Rejecting button event: code out of range"
                );
                return;
            }
            let seat = state.common.shell.read().seats.last_active().clone();
            if let Some(pointer) = seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                let state_val = if btn.state == eis::button::ButtonState::Press {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                };
                pointer.button(
                    state,
                    &smithay::input::pointer::ButtonEvent {
                        button: btn.button,
                        state: state_val,
                        serial,
                        time,
                    },
                );
                pointer.frame(state);
            }
        }
        EisRequest::ScrollDelta(scroll) => {
            let dx = f64::from(scroll.dx);
            let dy = f64::from(scroll.dy);
            if !dx.is_finite() || !dy.is_finite() {
                warn!("Rejecting scroll event: non-finite delta");
                return;
            }
            let seat = state.common.shell.read().seats.last_active().clone();
            if let Some(pointer) = seat.get_pointer() {
                use smithay::backend::input::Axis;
                let mut frame = smithay::input::pointer::AxisFrame::new(time);
                if dy.abs() > 0.0 {
                    frame = frame.value(Axis::Vertical, dy);
                }
                if dx.abs() > 0.0 {
                    frame = frame.value(Axis::Horizontal, dx);
                }
                pointer.axis(state, frame);
                pointer.frame(state);
            }
        }
        EisRequest::TouchDown(touch) => {
            if touch.touch_id > MAX_TOUCH_ID {
                warn!(
                    touch_id = touch.touch_id,
                    "Rejecting touch down: ID out of range"
                );
                return;
            }
            let x = f64::from(touch.x);
            let y = f64::from(touch.y);
            if !x.is_finite() || !y.is_finite() {
                warn!("Rejecting touch down: non-finite coordinates");
                return;
            }
            let (seat, under) = resolve_touch_target(state, x, y);
            if let Some(touch_handle) = seat.get_touch() {
                let serial = SERIAL_COUNTER.next_serial();
                touch_handle.down(
                    state,
                    under,
                    &DownEvent {
                        slot: TouchSlot::from(Some(touch.touch_id)),
                        location: (x, y).into(),
                        serial,
                        time,
                    },
                );
                touch_handle.frame(state);
            }
        }
        EisRequest::TouchMotion(touch) => {
            if touch.touch_id > MAX_TOUCH_ID {
                warn!(
                    touch_id = touch.touch_id,
                    "Rejecting touch motion: ID out of range"
                );
                return;
            }
            let x = f64::from(touch.x);
            let y = f64::from(touch.y);
            if !x.is_finite() || !y.is_finite() {
                warn!("Rejecting touch motion: non-finite coordinates");
                return;
            }
            let (seat, under) = resolve_touch_target(state, x, y);
            if let Some(touch_handle) = seat.get_touch() {
                touch_handle.motion(
                    state,
                    under,
                    &TouchMotionEvent {
                        slot: TouchSlot::from(Some(touch.touch_id)),
                        location: (x, y).into(),
                        time,
                    },
                );
                touch_handle.frame(state);
            }
        }
        EisRequest::TouchUp(touch) => {
            let seat = state.common.shell.read().seats.last_active().clone();
            if let Some(touch_handle) = seat.get_touch() {
                let serial = SERIAL_COUNTER.next_serial();
                touch_handle.up(
                    state,
                    &UpEvent {
                        slot: TouchSlot::from(Some(touch.touch_id)),
                        time,
                        serial,
                    },
                );
                touch_handle.frame(state);
            }
        }
        EisRequest::TouchCancel(_) => {
            let seat = state.common.shell.read().seats.last_active().clone();
            if let Some(touch_handle) = seat.get_touch() {
                touch_handle.cancel(state);
                touch_handle.frame(state);
            }
        }
        EisRequest::Disconnect => {
            info!("EIS client disconnected");
        }
        EisRequest::Bind(bind) => {
            debug!(
                "EIS client bound with capabilities: {:?}",
                bind.capabilities
            );

            // Prepare XKB keymap fd if keyboard capability is requested
            let keymap_fd = if bind.capabilities.contains(DeviceCapability::Keyboard) {
                prepare_xkb_keymap_fd(state)
            } else {
                None
            };

            let device = bind.seat.add_device(
                Some("remote-input"),
                eis::device::DeviceType::Virtual,
                bind.capabilities,
                |device| {
                    // Send compositor's XKB keymap to keyboard before device.done()
                    if let Some((ref fd, size)) = keymap_fd {
                        if let Some(keyboard) = device.interface::<eis::Keyboard>() {
                            keyboard.keymap(eis::keyboard::KeymapType::Xkb, size, fd.as_fd());
                        }
                    }
                },
            );
            device.resumed();
            if let Err(e) = connection.flush() {
                warn!("Failed to flush EIS device announcement: {e}");
            }
        }
        EisRequest::DeviceStartEmulating(_) | EisRequest::DeviceStopEmulating(_) => {}
        EisRequest::Frame(_) => {}
        _ => {
            debug!("Unhandled EIS request: {:?}", request);
        }
    }
}

/// Prepare the compositor's XKB keymap as a sealed memfd for sending to EIS clients.
///
/// Compiles the keymap from the compositor's current XKB configuration (RMLVO names),
/// writes it to a memfd with a null terminator, and seals the fd. Returns the fd and
/// total size (including null terminator), or `None` if keymap creation fails.
fn prepare_xkb_keymap_fd(state: &State) -> Option<(std::os::fd::OwnedFd, u32)> {
    use std::os::fd::FromRawFd;
    use xkbcommon::xkb;

    let conf = state.common.config.xkb_config();

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        &conf.rules,
        &conf.model,
        &conf.layout,
        &conf.variant,
        conf.options.clone(),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;

    let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let keymap_bytes = keymap_string.as_bytes();
    let size = (keymap_bytes.len() + 1) as u32; // +1 for null terminator

    // Create a sealed memfd for the keymap data
    let name = c"eis-keymap";
    let raw_fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw_fd < 0 {
        warn!("Failed to create memfd for EIS keymap");
        return None;
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(raw_fd) };

    // Write keymap string + null terminator
    use std::io::Write;
    if file.write_all(keymap_bytes).is_err() || file.write_all(&[0]).is_err() {
        warn!("Failed to write keymap to memfd");
        return None;
    }

    // Seal the memfd to prevent modification (best-effort)
    unsafe {
        libc::fcntl(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL,
        );
    }

    let owned_fd: std::os::fd::OwnedFd = file.into();
    Some((owned_fd, size))
}

/// Resolve the surface under a given position, acquiring and releasing the
/// shell read lock before returning so callers can use `&mut State`.
#[allow(clippy::type_complexity)]
fn resolve_touch_target(
    state: &State,
    x: f64,
    y: f64,
) -> (
    smithay::input::Seat<State>,
    Option<(
        <State as smithay::input::SeatHandler>::PointerFocus,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )>,
) {
    let shell = state.common.shell.read();
    let seat = shell.seats.last_active().clone();
    let position = (x, y).into();
    let under = shell
        .outputs()
        .find(|output| output.geometry().to_f64().contains(position))
        .and_then(|output| {
            State::surface_under(position, output, &shell)
                .map(|(target, pos)| (target, pos.as_logical()))
        });
    (seat, under)
}
