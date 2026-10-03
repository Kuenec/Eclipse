use std::collections::HashMap;

use wayland_client::backend::protocol::Argument;
use wayland_client::protocol::{wl_data_device, wl_keyboard, wl_pointer, wl_touch};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3;

use super::super::wire::Args;
use super::{Class, Session};

const TOUCH_POINT_LIMIT: usize = 32;

#[derive(Default)]
struct Pointer {
    surface: Option<u32>,
    frame_pending: bool,
}

#[derive(Default)]
struct Keyboard {
    game: Option<u32>,
    entered: Option<u32>,
    modifiers: Option<[u32; 4]>,
}

#[derive(Default)]
struct Touch {
    points: HashMap<i32, u32>,
    frame_pending: bool,
}

#[derive(Default)]
struct TextInput {
    game: bool,
    entered: Option<u32>,
}

#[derive(Default)]
pub(super) struct Input {
    pointers: HashMap<u32, Pointer>,
    keyboards: HashMap<u32, Keyboard>,
    touches: HashMap<u32, Touch>,
    text_inputs: HashMap<u32, TextInput>,
    data_devices: HashMap<u32, Option<u32>>,
    gestures: HashMap<u32, bool>,
}

impl Input {
    pub(super) fn adopt(&mut self, id: u32, class: Class) {
        match class {
            Class::Pointer => {
                self.pointers.insert(id, Pointer::default());
            }
            Class::Keyboard => {
                self.keyboards.insert(id, Keyboard::default());
            }
            Class::Touch => {
                self.touches.insert(id, Touch::default());
            }
            Class::TextInput => {
                self.text_inputs.insert(id, TextInput::default());
            }
            Class::DataDevice => {
                self.data_devices.insert(id, None);
            }
            Class::Gesture { .. } => {
                self.gestures.insert(id, false);
            }
            _ => {}
        }
    }

    pub(super) fn forget(&mut self, id: u32, class: Class) {
        match class {
            Class::Pointer => {
                self.pointers.remove(&id);
            }
            Class::Keyboard => {
                self.keyboards.remove(&id);
            }
            Class::Touch => {
                self.touches.remove(&id);
            }
            Class::TextInput => {
                self.text_inputs.remove(&id);
            }
            Class::DataDevice => {
                self.data_devices.remove(&id);
            }
            Class::Gesture { .. } => {
                self.gestures.remove(&id);
            }
            _ => {}
        }
    }

    pub(super) fn surface_gone(&mut self, surface: u32) {
        let gone = |entered: &mut Option<u32>| {
            if *entered == Some(surface) {
                *entered = None;
            }
        };
        for pointer in self.pointers.values_mut() {
            gone(&mut pointer.surface);
        }
        for keyboard in self.keyboards.values_mut() {
            gone(&mut keyboard.entered);
        }
        for text_input in self.text_inputs.values_mut() {
            gone(&mut text_input.entered);
        }
        for entered in self.data_devices.values_mut() {
            gone(entered);
        }
        for touch in self.touches.values_mut() {
            touch.points.retain(|_, on| *on != surface);
        }
    }

    pub(super) fn text_input_surface(&self, text_input: u32) -> Option<u32> {
        self.text_inputs.get(&text_input)?.entered
    }

    pub(super) fn game_focused(&self) -> bool {
        self.keyboards
            .values()
            .any(|keyboard| keyboard.game.is_some())
    }
}

fn object_at(args: &Args, index: usize) -> Option<u32> {
    match args.get(index) {
        Some(Argument::Object(id)) if *id != 0 => Some(*id),
        _ => None,
    }
}

fn int_at(args: &Args, index: usize) -> Option<i32> {
    match args.get(index) {
        Some(Argument::Int(value)) => Some(*value),
        _ => None,
    }
}

fn uint_at(args: &Args, index: usize) -> Option<u32> {
    match args.get(index) {
        Some(Argument::Uint(value)) => Some(*value),
        _ => None,
    }
}

impl Session {
    pub(super) fn pointer_event(
        &mut self,
        pointer: u32,
        opcode: u16,
        args: Args,
        foreign: &[usize],
    ) {
        let Some(state) = self.input.pointers.get_mut(&pointer) else {
            return;
        };
        let forward = match opcode {
            wl_pointer::EVT_ENTER_OPCODE => {
                state.surface = object_at(&args, 1).filter(|_| foreign.is_empty());
                state.surface.is_some()
            }
            wl_pointer::EVT_LEAVE_OPCODE => {
                let ours = foreign.is_empty() && state.surface.is_some();
                state.surface = None;
                ours
            }
            wl_pointer::EVT_FRAME_OPCODE => {
                let pending = state.frame_pending || state.surface.is_some();
                state.frame_pending = false;
                pending
            }
            _ => state.surface.is_some(),
        };
        if forward && opcode != wl_pointer::EVT_FRAME_OPCODE {
            state.frame_pending = true;
        }
        let pressed = opcode == wl_pointer::EVT_BUTTON_OPCODE
            && uint_at(&args, 3) == Some(wl_pointer::ButtonState::Pressed as u32);
        let under = state.surface;
        if pressed && self.grabbing_popups() {
            self.dismiss_popups(under);
        }
        if forward {
            self.send(pointer, opcode, args);
        }
    }

    pub(super) fn keyboard_event(
        &mut self,
        keyboard: u32,
        opcode: u16,
        args: Args,
        foreign: &[usize],
    ) {
        let focused = self.input.game_focused();
        let Some(state) = self.input.keyboards.get_mut(&keyboard) else {
            return;
        };
        match opcode {
            wl_keyboard::EVT_ENTER_OPCODE => {
                let Some(serial) = uint_at(&args, 0) else {
                    return;
                };
                state.game = Some(serial);
                if foreign.is_empty() {
                    state.entered = object_at(&args, 1);
                    self.send(keyboard, opcode, args);
                } else {
                    let keys = match args.into_iter().nth(2) {
                        Some(Argument::Array(keys)) => *keys,
                        _ => Vec::new(),
                    };
                    self.move_keyboard(keyboard, serial, keys);
                }
            }
            wl_keyboard::EVT_LEAVE_OPCODE => {
                state.game = None;
                let entered = state.entered.take();
                if let (Some(serial), Some(surface)) = (uint_at(&args, 0), entered) {
                    self.send(
                        keyboard,
                        opcode,
                        vec![Argument::Uint(serial), Argument::Object(surface)],
                    );
                }
                if !self.input.game_focused() {
                    self.dismiss_popups(None);
                }
            }
            wl_keyboard::EVT_MODIFIERS_OPCODE => {
                let modifiers = [1, 2, 3, 4].map(|index| uint_at(&args, index).unwrap_or(0));
                state.modifiers = Some(modifiers);
                if state.entered.is_some() {
                    self.send(keyboard, opcode, args);
                }
            }
            wl_keyboard::EVT_KEY_OPCODE => {
                if state.entered.is_some() {
                    self.send(keyboard, opcode, args);
                }
            }
            _ => self.send(keyboard, opcode, args),
        }
        if focused != self.input.game_focused() {
            self.configure_toplevels();
        }
    }

    fn move_keyboard(&mut self, keyboard: u32, serial: u32, keys: Vec<u8>) {
        let target = self.keyboard_target();
        let Some(state) = self.input.keyboards.get_mut(&keyboard) else {
            return;
        };
        if state.entered == target {
            return;
        }
        let previous = std::mem::replace(&mut state.entered, target);
        let modifiers = state.modifiers;
        if let Some(previous) = previous.filter(|surface| self.surfaces.contains_key(surface)) {
            self.send(
                keyboard,
                wl_keyboard::EVT_LEAVE_OPCODE,
                vec![Argument::Uint(serial), Argument::Object(previous)],
            );
        }
        let Some(target) = target else {
            return;
        };
        self.send(
            keyboard,
            wl_keyboard::EVT_ENTER_OPCODE,
            vec![
                Argument::Uint(serial),
                Argument::Object(target),
                Argument::Array(Box::new(keys)),
            ],
        );
        if let Some([depressed, latched, locked, group]) = modifiers {
            self.send(
                keyboard,
                wl_keyboard::EVT_MODIFIERS_OPCODE,
                vec![
                    Argument::Uint(serial),
                    Argument::Uint(depressed),
                    Argument::Uint(latched),
                    Argument::Uint(locked),
                    Argument::Uint(group),
                ],
            );
        }
    }

    pub(super) fn sync_focus(&mut self) {
        let keyboards: Vec<(u32, u32)> = self
            .input
            .keyboards
            .iter()
            .filter_map(|(id, state)| Some((*id, state.game?)))
            .collect();
        for (keyboard, serial) in keyboards {
            self.move_keyboard(keyboard, serial, Vec::new());
        }
        let target = self.keyboard_target();
        let text_inputs: Vec<(u32, Option<u32>)> = self
            .input
            .text_inputs
            .iter_mut()
            .filter(|(_, state)| state.game && state.entered != target)
            .map(|(id, state)| (*id, std::mem::replace(&mut state.entered, target)))
            .collect();
        for (text_input, previous) in text_inputs {
            self.move_text_input(text_input, previous, target);
        }
    }

    fn move_text_input(&mut self, text_input: u32, previous: Option<u32>, target: Option<u32>) {
        if let Some(previous) = previous.filter(|surface| self.surfaces.contains_key(surface)) {
            self.send(
                text_input,
                zwp_text_input_v3::EVT_LEAVE_OPCODE,
                vec![Argument::Object(previous)],
            );
        }
        if let Some(target) = target {
            self.send(
                text_input,
                zwp_text_input_v3::EVT_ENTER_OPCODE,
                vec![Argument::Object(target)],
            );
        }
    }

    pub(super) fn text_input_event(
        &mut self,
        text_input: u32,
        opcode: u16,
        args: Args,
        foreign: &[usize],
    ) {
        let target = self.keyboard_target();
        let Some(state) = self.input.text_inputs.get_mut(&text_input) else {
            return;
        };
        match opcode {
            zwp_text_input_v3::EVT_ENTER_OPCODE if !foreign.is_empty() => {
                state.game = true;
                let previous = std::mem::replace(&mut state.entered, target);
                self.move_text_input(text_input, previous, target);
            }
            zwp_text_input_v3::EVT_ENTER_OPCODE => {
                state.entered = object_at(&args, 0);
                self.send(text_input, opcode, args);
            }
            zwp_text_input_v3::EVT_LEAVE_OPCODE if !foreign.is_empty() => {
                state.game = false;
                let previous = state.entered.take();
                self.move_text_input(text_input, previous, None);
            }
            zwp_text_input_v3::EVT_LEAVE_OPCODE => {
                if state.entered.is_some() && state.entered == object_at(&args, 0) {
                    state.entered = None;
                    self.send(text_input, opcode, args);
                }
            }
            _ => self.send(text_input, opcode, args),
        }
    }

    pub(super) fn touch_event(&mut self, touch: u32, opcode: u16, args: Args, foreign: &[usize]) {
        let Some(state) = self.input.touches.get_mut(&touch) else {
            return;
        };
        let forward = match opcode {
            wl_touch::EVT_DOWN_OPCODE => {
                let down = object_at(&args, 2)
                    .filter(|_| foreign.is_empty())
                    .zip(int_at(&args, 3))
                    .filter(|_| state.points.len() < TOUCH_POINT_LIMIT);
                if let Some((surface, id)) = down {
                    state.points.insert(id, surface);
                }
                let touched = down.map(|(surface, _)| surface);
                if self.grabbing_popups() {
                    self.dismiss_popups(touched);
                }
                touched.is_some()
            }
            wl_touch::EVT_UP_OPCODE => {
                int_at(&args, 2).is_some_and(|id| state.points.remove(&id).is_some())
            }
            wl_touch::EVT_MOTION_OPCODE => {
                int_at(&args, 1).is_some_and(|id| state.points.contains_key(&id))
            }
            wl_touch::EVT_SHAPE_OPCODE | wl_touch::EVT_ORIENTATION_OPCODE => {
                int_at(&args, 0).is_some_and(|id| state.points.contains_key(&id))
            }
            wl_touch::EVT_FRAME_OPCODE => std::mem::take(&mut state.frame_pending),
            wl_touch::EVT_CANCEL_OPCODE => {
                let active = !state.points.is_empty() || state.frame_pending;
                state.points.clear();
                state.frame_pending = false;
                active
            }
            _ => false,
        };
        if let Some(state) = self.input.touches.get_mut(&touch) {
            if forward
                && opcode != wl_touch::EVT_FRAME_OPCODE
                && opcode != wl_touch::EVT_CANCEL_OPCODE
            {
                state.frame_pending = true;
            }
        }
        if forward {
            self.send(touch, opcode, args);
        }
    }

    pub(super) fn data_device_event(
        &mut self,
        device: u32,
        opcode: u16,
        args: Args,
        foreign: &[usize],
    ) {
        let Some(entered) = self.input.data_devices.get_mut(&device) else {
            return;
        };
        let forward = match opcode {
            wl_data_device::EVT_ENTER_OPCODE => {
                *entered = object_at(&args, 1).filter(|_| !foreign.contains(&1));
                entered.is_some()
            }
            wl_data_device::EVT_LEAVE_OPCODE => entered.take().is_some(),
            wl_data_device::EVT_MOTION_OPCODE | wl_data_device::EVT_DROP_OPCODE => {
                entered.is_some()
            }
            _ => foreign.is_empty(),
        };
        if forward {
            self.send(device, opcode, args);
        }
    }

    pub(super) fn gesture_event(
        &mut self,
        gesture: u32,
        opcode: u16,
        end: u16,
        args: Args,
        foreign: &[usize],
    ) {
        let Some(active) = self.input.gestures.get_mut(&gesture) else {
            return;
        };
        let begins = args.iter().any(|arg| matches!(arg, Argument::Object(_)));
        let forward = if begins {
            *active = foreign.is_empty();
            *active
        } else if opcode == end {
            std::mem::take(active)
        } else {
            *active
        };
        if forward {
            self.send(gesture, opcode, args);
        }
    }
}
