//! Guest-side delivery of the owned host window's ordered events.
use crate::{state::State, xwayland::Focus};
use legion_display_channel::input_events::{Batch, CaptureMode, Event, TimedEvent};
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    delegate_pointer_constraints, delegate_relative_pointer,
    input::{
        keyboard::{FilterResult, KeyboardTarget, Layout, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle, RelativeMotionEvent},
    },
    reexports::wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    utils::{Logical, Point, SERIAL_COUNTER},
    wayland::{
        pointer_constraints::{
            PointerConstraint, PointerConstraintsHandler, PointerConstraintsState,
            with_pointer_constraint,
        },
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
    },
};
use std::collections::BTreeSet;
use xkbcommon::xkb;

#[derive(Default)]
pub(crate) struct InputState {
    pub selected: Option<Focus>,
    pub host_keyboard: Option<bool>,
    pub keymap_generation: u32,
    pub events: u64,
    pub overflows: u64,
    pub map_retries: u64,
    pub relative_supported: bool,
    pub host_relative_supported: bool,
    pub host_capture_active: bool,
    absolute_events: u64,
    relative_events: u64,
    pub capture_active: bool,
    pub cursor_visible: Option<bool>,
    pointer_inside: bool,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    position: (f64, f64),
    extent: (u32, u32),
    time_anchor: Option<(u64, u64)>,
    last_time: u32,
    relative: Option<RelativePointerManagerState>,
    constraints: Option<PointerConstraintsState>,
}

impl State {
    pub(crate) fn cursor_position(&self) -> (i32, i32) {
        (
            self.ordered_input.position.0.round() as i32,
            self.ordered_input.position.1.round() as i32,
        )
    }
    pub(crate) fn input_diagnostics(&self) -> serde_json::Value {
        serde_json::json!({
            "position": self.ordered_input.position,
            "extent": self.ordered_input.extent,
            "cursor_visible": self.ordered_input.cursor_visible,
            "constraint": format!("{:?}", self.input_capture_mode()),
            "pointer_inside": self.ordered_input.pointer_inside,
            "relative_supported": self.ordered_input.relative_supported,
            "capture_active": self.ordered_input.capture_active,
            "host_relative_supported": self.ordered_input.host_relative_supported,
            "host_capture_active": self.ordered_input.host_capture_active,
            "absolute_events": self.ordered_input.absolute_events,
            "relative_events": self.ordered_input.relative_events,
        })
    }
    pub(crate) fn input_window_changed(&mut self) {
        self.deactivate_pointer_constraint();
        if self.ordered_input.pointer_inside {
            self.pointer_position(self.ordered_input.last_time);
        }
    }
    pub(crate) fn enable_relative_input(&mut self, dh: &DisplayHandle) {
        if self.ordered_input.relative.is_some() {
            return;
        }
        self.ordered_input.relative_supported = true;
        self.ordered_input.relative = Some(RelativePointerManagerState::new::<Self>(dh));
        self.ordered_input.constraints = Some(PointerConstraintsState::new::<Self>(dh));
    }
    pub(crate) fn input_capture_mode(&self) -> CaptureMode {
        if !self.ordered_input.relative_supported || self.ordered_input.host_keyboard == Some(false)
        {
            return CaptureMode::None;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return CaptureMode::None;
        };
        let Some(surface) = pointer.current_focus() else {
            return CaptureMode::None;
        };
        with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
            Some(constraint) if constraint.is_active() => match &*constraint {
                PointerConstraint::Locked(_) => CaptureMode::Lock,
                PointerConstraint::Confined(_) => CaptureMode::Confine,
            },
            _ => CaptureMode::None,
        })
    }
    fn input_surface(&self) -> Option<WlSurface> {
        self.ordered_input
            .selected
            .as_ref()
            .and_then(|focus| focus.wl_surface().map(|s| s.into_owned()))
    }
    fn key_event(&mut self, code: u32, down: bool, time: u32, restore: bool) {
        let changed = if down {
            self.ordered_input.keys.insert(code)
        } else {
            self.ordered_input.keys.remove(&code)
        };
        if !changed {
            return;
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.input::<(), _>(
                self,
                (code + 8).into(),
                if down {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
                SERIAL_COUNTER.next_serial(),
                time,
                // Restore events arrive with keyboard focus cleared, so this records the held key for the next enter without typing it.
                |_, _, _| FilterResult::Forward,
            );
            if !restore {
                self.keyboard_events += 1;
            }
        }
    }
    fn button_event(&mut self, code: u32, down: bool, time: u32) {
        let changed = if down {
            self.ordered_input.buttons.insert(code)
        } else {
            self.ordered_input.buttons.remove(&code)
        };
        if changed && let Some(pointer) = self.seat.get_pointer() {
            pointer.button(
                self,
                &ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    button: code,
                    state: if down {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                },
            );
            pointer.frame(self);
            self.pointer_events += 1;
        }
    }
    fn release_keys(&mut self, time: u32) {
        let keys = self.ordered_input.keys.iter().copied().collect::<Vec<_>>();
        for code in keys {
            self.key_event(code, false, time, false);
        }
    }
    fn release_buttons(&mut self, time: u32) {
        let buttons = self
            .ordered_input
            .buttons
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for code in buttons {
            self.button_event(code, false, time);
        }
    }
    pub(crate) fn reset_ordered_input(&mut self, time: u32) {
        self.release_keys(time);
        self.release_buttons(time);
    }
    fn pointer_position(&mut self, time: u32) {
        if let Some(pointer) = self.seat.get_pointer() {
            let focus = if self.ordered_input.pointer_inside {
                self.input_surface().map(|s| (s, (0.0, 0.0).into()))
            } else {
                None
            };
            pointer.motion(
                self,
                focus,
                &MotionEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    location: self.ordered_input.position.into(),
                },
            );
            pointer.frame(self);
            self.pointer_events += 1;
            self.activate_pointer_constraint();
        }
    }
    fn activate_pointer_constraint(&mut self) {
        if self.ordered_input.host_keyboard == Some(false) {
            return;
        }
        if let Some(pointer) = self.seat.get_pointer()
            && let Some(surface) = pointer.current_focus()
        {
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(constraint) = constraint {
                    let point: Point<f64, Logical> = self.ordered_input.position.into();
                    if constraint
                        .region()
                        .is_none_or(|region| region.contains(point.to_i32_round()))
                    {
                        constraint.activate();
                    }
                }
            });
        }
    }
    fn host_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            // Decode the host's masks using the same transferred keymap, without prematurely changing Smithay's internal XKB state.
            let mods = keyboard.with_xkb_state(self, |context| {
                let xkb = context.xkb().lock().expect("keyboard XKB lock");
                // SAFETY: the temporary state and its keymap reference are dropped inside this callback, before the locked Xkb owner.
                let mut state = xkb::State::new(unsafe { xkb.keymap() });
                state.update_mask(depressed, latched, locked, 0, 0, group);
                let mut mods = ModifiersState::default();
                mods.update_with(&state);
                mods
            });
            let changed = keyboard.set_modifier_state(mods);
            keyboard.with_xkb_state(self, |mut context| context.set_layout(Layout(group)));
            if changed != 0
                && let Some(focus) = keyboard.current_focus()
            {
                focus.modifiers(
                    &self.seat.clone(),
                    self,
                    keyboard.modifier_state(),
                    SERIAL_COUNTER.next_serial(),
                );
            }
        }
    }
    fn deliver_input_event(&mut self, timed: TimedEvent, now_us: u64) {
        let (host, guest) = *self
            .ordered_input
            .time_anchor
            .get_or_insert((timed.time_us, now_us));
        let local_us = guest.saturating_add(timed.time_us.saturating_sub(host));
        let time = (local_us / 1000) as u32;
        self.ordered_input.last_time = time;
        match timed.event {
            Event::Key { code, down } => {
                if self.ordered_input.host_keyboard != Some(false) {
                    self.key_event(code, down, time, false);
                }
            }
            Event::Button { code, down } => {
                if self.ordered_input.pointer_inside || self.ordered_input.capture_active {
                    self.button_event(code, down, time);
                }
            }
            Event::Absolute { x, y } => {
                self.ordered_input.absolute_events += 1;
                if self.input_capture_mode() != CaptureMode::Lock {
                    self.ordered_input.position = (
                        x * f64::from(self.ordered_input.extent.0),
                        y * f64::from(self.ordered_input.extent.1),
                    );
                    self.pointer_position(time);
                }
            }
            Event::Relative {
                dx,
                dy,
                raw_dx,
                raw_dy,
            } => {
                self.ordered_input.relative_events += 1;
                if (self.ordered_input.pointer_inside || self.ordered_input.capture_active)
                    && let Some(pointer) = self.seat.get_pointer()
                {
                    pointer.relative_motion(
                        self,
                        pointer
                            .current_focus()
                            .map(|surface| (surface, (0.0, 0.0).into())),
                        &RelativeMotionEvent {
                            delta: (dx, dy).into(),
                            delta_unaccel: (raw_dx, raw_dy).into(),
                            utime: local_us,
                        },
                    );
                    pointer.frame(self);
                    self.pointer_events += 1;
                }
            }
            Event::Axis {
                axis,
                value,
                v120,
                stop,
                source,
            } => {
                if self.ordered_input.pointer_inside
                    && let Some(pointer) = self.seat.get_pointer()
                {
                    let axis = if axis == 0 {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    };
                    let source = match source {
                        1 => AxisSource::Finger,
                        2 => AxisSource::Continuous,
                        3 => AxisSource::WheelTilt,
                        _ => AxisSource::Wheel,
                    };
                    let mut frame = AxisFrame::new(time).source(source).value(axis, value);
                    if v120 != 0 {
                        frame = frame.v120(axis, v120);
                    }
                    if stop {
                        frame = frame.stop(axis);
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                    self.pointer_events += 1;
                }
            }
            Event::Focus {
                keyboard: true,
                focused,
            } => {
                if !focused {
                    self.reset_ordered_input(time);
                }
                self.ordered_input.host_keyboard = Some(focused);
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let focus = if focused {
                        self.ordered_input.selected.clone()
                    } else {
                        None
                    };
                    keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
                }
                if !focused {
                    self.deactivate_pointer_constraint();
                } else {
                    self.activate_pointer_constraint();
                }
            }
            Event::Focus {
                keyboard: false,
                focused,
            } => {
                if !focused {
                    self.release_buttons(time);
                    self.deactivate_pointer_constraint();
                }
                self.ordered_input.pointer_inside = focused;
                self.pointer_position(time);
            }
            Event::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => self.host_modifiers(depressed, latched, locked, group),
            Event::Reset => self.reset_ordered_input(time),
            Event::KeyboardRestore => {
                self.release_keys(time);
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                }
            }
            Event::HeldKey { code } => self.key_event(code, true, time, true),
            Event::Capture { active } => self.ordered_input.capture_active = active,
            Event::Repeat { rate, delay } => {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.change_repeat_info(rate as i32, delay as i32);
                }
            }
        }
        self.ordered_input.events += 1;
    }
    fn deactivate_pointer_constraint(&mut self) {
        if let Some(pointer) = self.seat.get_pointer()
            && let Some(surface) = pointer.current_focus()
        {
            with_pointer_constraint(&surface, &pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.deactivate();
                }
            });
        }
        self.ordered_input.capture_active = false;
    }
    pub(crate) fn ordered_input_batch(&mut self, batch: Batch, now_us: u64) {
        self.ordered_input.extent = (batch.width, batch.height);
        self.ordered_input.host_relative_supported = batch.relative_supported;
        self.ordered_input.host_capture_active = batch.capture_active;
        self.ordered_input.overflows = batch.overflows;
        for event in batch.events {
            self.deliver_input_event(event, now_us);
        }
        if batch.closed {
            self.reset_ordered_input((now_us / 1000) as u32);
        }
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _: &WlSurface, _: &PointerHandle<Self>) {
        self.activate_pointer_constraint();
    }
    fn cursor_position_hint(
        &mut self,
        _: &WlSurface,
        _: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        self.ordered_input.position = (location.x, location.y);
    }
}
delegate_relative_pointer!(State);
delegate_pointer_constraints!(State);
