use winit::event::MouseScrollDelta;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

use crate::framework::{TextEdit, TextMotion, TextUnit};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyEdge {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardShortcut {
    Copy,
    Cut,
    Paste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextFieldKey<'a> {
    Edit(TextEdit<'a>),
    Submit,
    Dismiss,
    SelectAll,
    Clipboard(ClipboardShortcut),
    Engine,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shortcut {
    SelectAll,
    Clipboard(ClipboardShortcut),
}

pub(crate) fn text_field_key<'a>(
    logical: &Key,
    physical: PhysicalKey,
    text: Option<&'a str>,
    modifiers: ModifiersState,
    multiline: bool,
    edge: KeyEdge,
) -> TextFieldKey<'a> {
    if edge == KeyEdge::Release {
        return match logical {
            Key::Named(NamedKey::Escape) => TextFieldKey::Dismiss,
            _ => TextFieldKey::Ignore,
        };
    }
    let ctrl = modifiers.control_key();
    let shift = modifiers.shift_key();
    let alt_or_super = modifiers.alt_key() || modifiers.super_key();
    let command = ctrl || alt_or_super;
    let first_press = |key: TextFieldKey<'a>| match edge {
        KeyEdge::Press => key,
        KeyEdge::Repeat | KeyEdge::Release => TextFieldKey::Ignore,
    };
    let unit = if ctrl {
        TextUnit::Word
    } else {
        TextUnit::Character
    };
    let edit = |edit: TextEdit<'a>| {
        if alt_or_super {
            TextFieldKey::Ignore
        } else {
            TextFieldKey::Edit(edit)
        }
    };
    match logical {
        Key::Named(NamedKey::Enter) if command => TextFieldKey::Ignore,
        Key::Named(NamedKey::Enter) if multiline => TextFieldKey::Edit(TextEdit::Type("\n")),
        Key::Named(NamedKey::Enter) => first_press(TextFieldKey::Submit),
        Key::Named(NamedKey::Tab) if !command && multiline => {
            TextFieldKey::Edit(TextEdit::Type("\t"))
        }
        Key::Named(NamedKey::Backspace) => edit(TextEdit::Backspace(unit)),
        Key::Named(NamedKey::Delete) if shift && !command => {
            first_press(TextFieldKey::Clipboard(ClipboardShortcut::Cut))
        }
        Key::Named(NamedKey::Delete) => edit(TextEdit::ForwardDelete(unit)),
        Key::Named(NamedKey::Insert) if shift && !command => {
            first_press(TextFieldKey::Clipboard(ClipboardShortcut::Paste))
        }
        Key::Named(NamedKey::Insert) if ctrl && !alt_or_super && !shift => {
            first_press(TextFieldKey::Clipboard(ClipboardShortcut::Copy))
        }
        Key::Named(NamedKey::Insert) if !command => TextFieldKey::Engine,
        Key::Named(NamedKey::ArrowLeft) => edit(TextEdit::Move(TextMotion::Left(unit))),
        Key::Named(NamedKey::ArrowRight) => edit(TextEdit::Move(TextMotion::Right(unit))),
        Key::Named(NamedKey::ArrowUp) if !command => {
            TextFieldKey::Edit(TextEdit::Move(TextMotion::Up))
        }
        Key::Named(NamedKey::ArrowDown) if !command => {
            TextFieldKey::Edit(TextEdit::Move(TextMotion::Down))
        }
        Key::Named(NamedKey::Home) if ctrl => edit(TextEdit::Move(TextMotion::TextStart)),
        Key::Named(NamedKey::Home) => edit(TextEdit::Move(TextMotion::LineStart)),
        Key::Named(NamedKey::End) if ctrl => edit(TextEdit::Move(TextMotion::TextEnd)),
        Key::Named(NamedKey::End) => edit(TextEdit::Move(TextMotion::LineEnd)),
        _ if ctrl && !alt_or_super => match shortcut(logical, physical) {
            Some(Shortcut::SelectAll) => first_press(TextFieldKey::SelectAll),
            Some(Shortcut::Clipboard(clipboard)) => first_press(TextFieldKey::Clipboard(clipboard)),
            None => TextFieldKey::Ignore,
        },
        _ if command => TextFieldKey::Ignore,
        _ => match text.filter(|text| text.chars().any(|character| !character.is_control())) {
            Some(text) => TextFieldKey::Edit(TextEdit::Type(text)),
            None => TextFieldKey::Ignore,
        },
    }
}

fn shortcut(logical: &Key, physical: PhysicalKey) -> Option<Shortcut> {
    if !matches!(logical, Key::Character(_)) {
        return None;
    }
    match android_key_code(logical, physical) {
        KEYCODE_A => Some(Shortcut::SelectAll),
        KEYCODE_C => Some(Shortcut::Clipboard(ClipboardShortcut::Copy)),
        KEYCODE_X => Some(Shortcut::Clipboard(ClipboardShortcut::Cut)),
        KEYCODE_V => Some(Shortcut::Clipboard(ClipboardShortcut::Paste)),
        _ => None,
    }
}

const KEYCODE_UNKNOWN: i32 = 0;
const KEYCODE_BACK: i32 = 4;
const KEYCODE_A: i32 = 29;
const KEYCODE_C: i32 = 31;
const KEYCODE_V: i32 = 50;
const KEYCODE_X: i32 = 52;

pub(crate) fn android_key_code(logical: &Key, physical: PhysicalKey) -> i32 {
    if let Some(letter) = layout_letter(logical) {
        return KEYCODE_A + i32::from(letter - b'a');
    }
    match physical {
        PhysicalKey::Code(code) => physical_key_code(code),
        PhysicalKey::Unidentified(_) => KEYCODE_UNKNOWN,
    }
}

fn layout_letter(key: &Key) -> Option<u8> {
    let Key::Character(text) = key else {
        return None;
    };
    match text.as_bytes() {
        [letter] if letter.is_ascii_alphabetic() => Some(letter.to_ascii_lowercase()),
        _ => None,
    }
}

fn physical_key_code(code: KeyCode) -> i32 {
    match code {
        KeyCode::Escape => KEYCODE_BACK,
        KeyCode::ArrowUp => 19,
        KeyCode::ArrowDown => 20,
        KeyCode::ArrowLeft => 21,
        KeyCode::ArrowRight => 22,
        KeyCode::Digit0 => 7,
        KeyCode::Digit1 => 8,
        KeyCode::Digit2 => 9,
        KeyCode::Digit3 => 10,
        KeyCode::Digit4 => 11,
        KeyCode::Digit5 => 12,
        KeyCode::Digit6 => 13,
        KeyCode::Digit7 => 14,
        KeyCode::Digit8 => 15,
        KeyCode::Digit9 => 16,
        KeyCode::KeyA => KEYCODE_A,
        KeyCode::KeyB => 30,
        KeyCode::KeyC => KEYCODE_C,
        KeyCode::KeyD => 32,
        KeyCode::KeyE => 33,
        KeyCode::KeyF => 34,
        KeyCode::KeyG => 35,
        KeyCode::KeyH => 36,
        KeyCode::KeyI => 37,
        KeyCode::KeyJ => 38,
        KeyCode::KeyK => 39,
        KeyCode::KeyL => 40,
        KeyCode::KeyM => 41,
        KeyCode::KeyN => 42,
        KeyCode::KeyO => 43,
        KeyCode::KeyP => 44,
        KeyCode::KeyQ => 45,
        KeyCode::KeyR => 46,
        KeyCode::KeyS => 47,
        KeyCode::KeyT => 48,
        KeyCode::KeyU => 49,
        KeyCode::KeyV => KEYCODE_V,
        KeyCode::KeyW => 51,
        KeyCode::KeyX => KEYCODE_X,
        KeyCode::KeyY => 53,
        KeyCode::KeyZ => 54,
        KeyCode::Comma => 55,
        KeyCode::Period => 56,
        KeyCode::AltLeft => 57,
        KeyCode::AltRight => 58,
        KeyCode::ShiftLeft => 59,
        KeyCode::ShiftRight => 60,
        KeyCode::Tab => 61,
        KeyCode::Space => 62,
        KeyCode::Enter => 66,
        KeyCode::Backspace => 67,
        KeyCode::Backquote => 68,
        KeyCode::Minus => 69,
        KeyCode::Equal => 70,
        KeyCode::BracketLeft => 71,
        KeyCode::BracketRight => 72,
        KeyCode::Backslash | KeyCode::IntlBackslash => 73,
        KeyCode::Semicolon => 74,
        KeyCode::Quote => 75,
        KeyCode::Slash => 76,
        KeyCode::ContextMenu => 82,
        KeyCode::PageUp => 92,
        KeyCode::PageDown => 93,
        KeyCode::Delete => 112,
        KeyCode::ControlLeft => 113,
        KeyCode::ControlRight => 114,
        KeyCode::CapsLock => 115,
        KeyCode::ScrollLock => 116,
        KeyCode::SuperLeft => 117,
        KeyCode::SuperRight => 118,
        KeyCode::PrintScreen => 120,
        KeyCode::Pause => 121,
        KeyCode::Home => 122,
        KeyCode::End => 123,
        KeyCode::Insert => 124,
        KeyCode::F1 => 131,
        KeyCode::F2 => 132,
        KeyCode::F3 => 133,
        KeyCode::F4 => 134,
        KeyCode::F5 => 135,
        KeyCode::F6 => 136,
        KeyCode::F7 => 137,
        KeyCode::F8 => 138,
        KeyCode::F9 => 139,
        KeyCode::F10 => 140,
        KeyCode::F11 => 141,
        KeyCode::F12 => 142,
        KeyCode::NumLock => 143,
        KeyCode::Numpad0 => 144,
        KeyCode::Numpad1 => 145,
        KeyCode::Numpad2 => 146,
        KeyCode::Numpad3 => 147,
        KeyCode::Numpad4 => 148,
        KeyCode::Numpad5 => 149,
        KeyCode::Numpad6 => 150,
        KeyCode::Numpad7 => 151,
        KeyCode::Numpad8 => 152,
        KeyCode::Numpad9 => 153,
        KeyCode::NumpadDivide => 154,
        KeyCode::NumpadMultiply => 155,
        KeyCode::NumpadSubtract => 156,
        KeyCode::NumpadAdd => 157,
        KeyCode::NumpadDecimal => 158,
        KeyCode::NumpadComma => 159,
        KeyCode::NumpadEnter => 160,
        KeyCode::NumpadEqual => 161,
        KeyCode::IntlYen => 216,
        KeyCode::IntlRo => 217,
        _ => KEYCODE_UNKNOWN,
    }
}

const LOGICAL_PIXELS_PER_NOTCH: f64 = 40.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WheelSteps {
    pending: f64,
}

impl WheelSteps {
    pub(crate) fn notches(&mut self, delta: MouseScrollDelta, scale_factor: f64) -> Option<f32> {
        let steps = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y),
            MouseScrollDelta::PixelDelta(position) => {
                position.y / scale_factor / LOGICAL_PIXELS_PER_NOTCH
            }
        };
        if steps * self.pending < 0.0 {
            self.pending = 0.0;
        }
        self.pending += steps;
        let whole = self.pending.trunc();
        if whole == 0.0 {
            return None;
        }
        self.pending -= whole;
        Some(whole as f32)
    }
}

pub(crate) const MAX_TOUCH_POINTERS: usize = 10;

const PRIMARY_POINTER_ID: i32 = 0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TouchPointer {
    pub(crate) id: i32,
    pub(crate) x: f32,
    pub(crate) y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TouchAction {
    Down,
    PointerDown(usize),
    Move,
    PointerUp(usize),
    Up,
    Cancel(usize),
}

impl TouchAction {
    fn index(self) -> usize {
        match self {
            Self::Down | Self::Move | Self::Up => 0,
            Self::PointerDown(index) | Self::PointerUp(index) | Self::Cancel(index) => index,
        }
    }

    pub(crate) fn android_action(self) -> i32 {
        let masked = match self {
            Self::Down => 0,
            Self::Up => 1,
            Self::Move => 2,
            Self::Cancel(_) => 3,
            Self::PointerDown(_) => 5,
            Self::PointerUp(_) => 6,
        };
        masked | (self.index() as i32) << 8
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PrimaryTouch {
    Press { x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Release { x: f32, y: f32 },
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TouchMotion {
    pub(crate) action: TouchAction,
    pointers: [TouchPointer; MAX_TOUCH_POINTERS],
    count: usize,
}

impl TouchMotion {
    pub(crate) fn pointers(&self) -> &[TouchPointer] {
        &self.pointers[..self.count]
    }

    pub(crate) fn pressed_pointer(&self) -> Option<TouchPointer> {
        match self.action {
            TouchAction::Down | TouchAction::PointerDown(_) => {
                self.pointers().get(self.action.index()).copied()
            }
            TouchAction::Move
            | TouchAction::PointerUp(_)
            | TouchAction::Up
            | TouchAction::Cancel(_) => None,
        }
    }

    pub(crate) fn ends_stream(&self) -> bool {
        let last_pointer = self.count == 1;
        match self.action {
            TouchAction::Up | TouchAction::Cancel(_) => last_pointer,
            TouchAction::Down
            | TouchAction::PointerDown(_)
            | TouchAction::Move
            | TouchAction::PointerUp(_) => false,
        }
    }

    pub(crate) fn primary(&self) -> Option<PrimaryTouch> {
        let &TouchPointer { id, x, y } = self.pointers().first()?;
        if id != PRIMARY_POINTER_ID {
            return None;
        }
        match self.action {
            TouchAction::Move => Some(PrimaryTouch::Move { x, y }),
            action if action.index() != 0 => None,
            TouchAction::Down | TouchAction::PointerDown(_) => Some(PrimaryTouch::Press { x, y }),
            TouchAction::Up | TouchAction::PointerUp(_) => Some(PrimaryTouch::Release { x, y }),
            TouchAction::Cancel(_) => Some(PrimaryTouch::Cancel),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Finger {
    finger: u64,
    pointer: TouchPointer,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TouchTracker {
    fingers: [Finger; MAX_TOUCH_POINTERS],
    count: usize,
    moved: bool,
}

impl TouchTracker {
    pub(crate) fn is_active(&self) -> bool {
        self.count > 0
    }

    pub(crate) fn touch(
        &mut self,
        finger: u64,
        phase: winit::event::TouchPhase,
        x: f32,
        y: f32,
    ) -> impl Iterator<Item = TouchMotion> + use<> {
        use winit::event::TouchPhase;

        let tracked = self.position(finger);
        let edge = match (phase, tracked) {
            (TouchPhase::Started, None) if self.count < MAX_TOUCH_POINTERS => FingerEdge::Press,
            (TouchPhase::Moved, Some(index)) => {
                self.move_to(index, x, y);
                FingerEdge::Unchanged
            }
            (TouchPhase::Ended, Some(index)) => FingerEdge::Lift(index),
            (TouchPhase::Cancelled, Some(index)) => FingerEdge::Cancel(index),
            _ => FingerEdge::Unchanged,
        };
        let flushed = match edge {
            FingerEdge::Unchanged => None,
            FingerEdge::Press | FingerEdge::Lift(_) | FingerEdge::Cancel(_) => self.flush(),
        };
        let edge = match edge {
            FingerEdge::Unchanged => None,
            FingerEdge::Press => Some(self.press(finger, x, y)),
            FingerEdge::Lift(index) => {
                self.fingers[index].pointer.x = x;
                self.fingers[index].pointer.y = y;
                let action = match self.count {
                    1 => TouchAction::Up,
                    _ => TouchAction::PointerUp(index),
                };
                Some(self.remove(index, action))
            }
            FingerEdge::Cancel(index) => Some(self.remove(index, TouchAction::Cancel(index))),
        };
        [flushed, edge].into_iter().flatten()
    }

    pub(crate) fn flush(&mut self) -> Option<TouchMotion> {
        if !std::mem::take(&mut self.moved) || self.count == 0 {
            return None;
        }
        Some(self.motion(TouchAction::Move))
    }

    pub(crate) fn cancel(&mut self) -> TouchCancel {
        TouchCancel {
            tracker: std::mem::take(self),
        }
    }

    fn position(&self, finger: u64) -> Option<usize> {
        self.fingers[..self.count]
            .iter()
            .position(|tracked| tracked.finger == finger)
    }

    fn move_to(&mut self, index: usize, x: f32, y: f32) {
        let pointer = &mut self.fingers[index].pointer;
        if (pointer.x, pointer.y) != (x, y) {
            pointer.x = x;
            pointer.y = y;
            self.moved = true;
        }
    }

    fn press(&mut self, finger: u64, x: f32, y: f32) -> TouchMotion {
        let index = self.fingers[..self.count]
            .iter()
            .zip(0..)
            .position(|(tracked, id)| tracked.pointer.id != id)
            .unwrap_or(self.count);
        let id = index as i32;
        self.fingers.copy_within(index..self.count, index + 1);
        self.fingers[index] = Finger {
            finger,
            pointer: TouchPointer { id, x, y },
        };
        self.count += 1;
        let action = match self.count {
            1 => TouchAction::Down,
            _ => TouchAction::PointerDown(index),
        };
        self.motion(action)
    }

    fn remove(&mut self, index: usize, action: TouchAction) -> TouchMotion {
        let motion = self.motion(action);
        self.fingers.copy_within(index + 1..self.count, index);
        self.count -= 1;
        motion
    }

    fn motion(&self, action: TouchAction) -> TouchMotion {
        let mut pointers = [TouchPointer::default(); MAX_TOUCH_POINTERS];
        for (pointer, finger) in pointers.iter_mut().zip(&self.fingers[..self.count]) {
            *pointer = finger.pointer;
        }
        TouchMotion {
            action,
            pointers,
            count: self.count,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum FingerEdge {
    Unchanged,
    Press,
    Lift(usize),
    Cancel(usize),
}

pub(crate) struct TouchCancel {
    tracker: TouchTracker,
}

impl Iterator for TouchCancel {
    type Item = TouchMotion;

    fn next(&mut self) -> Option<TouchMotion> {
        if let Some(moved) = self.tracker.flush() {
            return Some(moved);
        }
        let last = self.tracker.count.checked_sub(1)?;
        Some(self.tracker.remove(last, TouchAction::Cancel(last)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::event::TouchPhase;

    const NONE: ModifiersState = ModifiersState::empty();
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const NO_KEY: PhysicalKey = PhysicalKey::Code(KeyCode::Fn);

    fn press<'a>(key: Key, text: Option<&'a str>, modifiers: ModifiersState) -> TextFieldKey<'a> {
        text_field_key(&key, NO_KEY, text, modifiers, false, KeyEdge::Press)
    }

    fn named(key: NamedKey) -> Key {
        Key::Named(key)
    }

    #[test]
    fn enter_submits_a_single_line_box_once_and_breaks_lines_in_a_multiline_box() {
        let enter = named(NamedKey::Enter);
        assert_eq!(press(enter.clone(), Some("\r"), NONE), TextFieldKey::Submit);
        assert_eq!(
            press(enter.clone(), Some("\r"), SHIFT),
            TextFieldKey::Submit
        );
        assert_eq!(press(enter.clone(), Some("\r"), CTRL), TextFieldKey::Ignore);
        assert_eq!(
            text_field_key(&enter, NO_KEY, Some("\r"), NONE, false, KeyEdge::Repeat),
            TextFieldKey::Ignore
        );
        assert_eq!(
            text_field_key(&enter, NO_KEY, Some("\r"), NONE, true, KeyEdge::Press),
            TextFieldKey::Edit(TextEdit::Type("\n"))
        );
        assert_eq!(
            text_field_key(&enter, NO_KEY, Some("\r"), NONE, false, KeyEdge::Release),
            TextFieldKey::Ignore
        );
    }

    #[test]
    fn escape_leaves_the_box_on_release_like_android_back() {
        let escape = named(NamedKey::Escape);
        assert_eq!(
            press(escape.clone(), Some("\u{1b}"), NONE),
            TextFieldKey::Ignore
        );
        assert_eq!(
            text_field_key(&escape, NO_KEY, None, NONE, false, KeyEdge::Release),
            TextFieldKey::Dismiss
        );
    }

    #[test]
    fn navigation_and_deletion_keys_edit_the_text_field() {
        let cases = [
            (
                NamedKey::ArrowLeft,
                NONE,
                TextEdit::Move(TextMotion::Left(TextUnit::Character)),
            ),
            (
                NamedKey::ArrowRight,
                NONE,
                TextEdit::Move(TextMotion::Right(TextUnit::Character)),
            ),
            (
                NamedKey::ArrowLeft,
                CTRL,
                TextEdit::Move(TextMotion::Left(TextUnit::Word)),
            ),
            (
                NamedKey::ArrowRight,
                CTRL,
                TextEdit::Move(TextMotion::Right(TextUnit::Word)),
            ),
            (NamedKey::ArrowUp, NONE, TextEdit::Move(TextMotion::Up)),
            (NamedKey::ArrowDown, NONE, TextEdit::Move(TextMotion::Down)),
            (NamedKey::Home, NONE, TextEdit::Move(TextMotion::LineStart)),
            (NamedKey::End, NONE, TextEdit::Move(TextMotion::LineEnd)),
            (NamedKey::Home, CTRL, TextEdit::Move(TextMotion::TextStart)),
            (NamedKey::End, CTRL, TextEdit::Move(TextMotion::TextEnd)),
            (
                NamedKey::Backspace,
                NONE,
                TextEdit::Backspace(TextUnit::Character),
            ),
            (
                NamedKey::Backspace,
                CTRL,
                TextEdit::Backspace(TextUnit::Word),
            ),
            (
                NamedKey::Delete,
                NONE,
                TextEdit::ForwardDelete(TextUnit::Character),
            ),
            (
                NamedKey::Delete,
                CTRL,
                TextEdit::ForwardDelete(TextUnit::Word),
            ),
        ];
        for (key, modifiers, edit) in cases {
            assert_eq!(
                press(named(key), None, modifiers),
                TextFieldKey::Edit(edit),
                "{key:?} {modifiers:?}"
            );
            assert_eq!(
                text_field_key(&named(key), NO_KEY, None, modifiers, false, KeyEdge::Repeat),
                TextFieldKey::Edit(edit),
                "held {key:?} repeats"
            );
        }
    }

    #[test]
    fn tab_is_typed_only_into_multiline_boxes() {
        let tab = named(NamedKey::Tab);
        assert_eq!(press(tab.clone(), Some("\t"), NONE), TextFieldKey::Ignore);
        assert_eq!(
            text_field_key(&tab, NO_KEY, Some("\t"), NONE, true, KeyEdge::Press),
            TextFieldKey::Edit(TextEdit::Type("\t"))
        );
    }

    #[test]
    fn ctrl_a_selects_all_on_latin_and_non_latin_layouts() {
        let key_a = PhysicalKey::Code(KeyCode::KeyA);
        let key_q = PhysicalKey::Code(KeyCode::KeyQ);
        let select = |logical: &str, physical| {
            text_field_key(
                &Key::Character(logical.into()),
                physical,
                None,
                CTRL,
                false,
                KeyEdge::Press,
            )
        };
        assert_eq!(select("a", key_a), TextFieldKey::SelectAll);
        assert_eq!(select("ф", key_a), TextFieldKey::SelectAll);
        assert_eq!(select("a", key_q), TextFieldKey::SelectAll);
        assert_eq!(select("q", key_a), TextFieldKey::Ignore);
    }

    #[test]
    fn clipboard_shortcuts_fire_once_per_press() {
        let letter = |logical: &str| press(Key::Character(logical.into()), None, CTRL);
        assert_eq!(
            letter("c"),
            TextFieldKey::Clipboard(ClipboardShortcut::Copy)
        );
        assert_eq!(letter("x"), TextFieldKey::Clipboard(ClipboardShortcut::Cut));
        assert_eq!(
            letter("v"),
            TextFieldKey::Clipboard(ClipboardShortcut::Paste)
        );
        assert_eq!(
            press(named(NamedKey::Insert), None, SHIFT),
            TextFieldKey::Clipboard(ClipboardShortcut::Paste)
        );
        assert_eq!(
            press(named(NamedKey::Insert), None, CTRL),
            TextFieldKey::Clipboard(ClipboardShortcut::Copy)
        );
        assert_eq!(
            press(named(NamedKey::Delete), None, SHIFT),
            TextFieldKey::Clipboard(ClipboardShortcut::Cut)
        );
        assert_eq!(
            text_field_key(
                &Key::Character("v".into()),
                NO_KEY,
                Some("v"),
                CTRL,
                false,
                KeyEdge::Repeat
            ),
            TextFieldKey::Ignore
        );
    }

    #[test]
    fn typed_text_keeps_every_character_the_key_produced() {
        for text in ["a", "ф", "中", "J\u{301}", "😀"] {
            assert_eq!(
                press(Key::Character(text.into()), Some(text), NONE),
                TextFieldKey::Edit(TextEdit::Type(text))
            );
        }
        assert_eq!(
            press(named(NamedKey::Space), Some(" "), NONE),
            TextFieldKey::Edit(TextEdit::Type(" "))
        );
    }

    #[test]
    fn command_chords_and_textless_keys_never_type() {
        assert_eq!(
            press(Key::Character("q".into()), Some("q"), CTRL),
            TextFieldKey::Ignore
        );
        assert_eq!(
            press(Key::Character("x".into()), Some("x"), ModifiersState::ALT),
            TextFieldKey::Ignore
        );
        assert_eq!(
            press(named(NamedKey::ArrowLeft), None, ModifiersState::ALT),
            TextFieldKey::Ignore
        );
        assert_eq!(press(named(NamedKey::F1), None, NONE), TextFieldKey::Ignore);
    }

    #[test]
    fn insert_alone_still_reaches_the_engine_menu_toggle() {
        assert_eq!(
            press(named(NamedKey::Insert), None, NONE),
            TextFieldKey::Engine
        );
    }

    fn key_code(character: &str, physical: KeyCode) -> i32 {
        android_key_code(
            &Key::Character(character.into()),
            PhysicalKey::Code(physical),
        )
    }

    fn named_key_code(key: NamedKey, physical: KeyCode) -> i32 {
        android_key_code(&named(key), PhysicalKey::Code(physical))
    }

    #[test]
    fn letters_follow_the_layout() {
        assert_eq!(key_code("a", KeyCode::KeyQ), 29, "AZERTY");
        assert_eq!(key_code("z", KeyCode::KeyY), 54, "QWERTZ");
        assert_eq!(key_code("A", KeyCode::KeyA), 29);
        assert_eq!(key_code("z", KeyCode::KeyZ), 54);
    }

    #[test]
    fn the_azerty_digit_row_sends_digit_key_codes() {
        assert_eq!(key_code("&", KeyCode::Digit1), 8);
        assert_eq!(key_code("é", KeyCode::Digit2), 9);
        assert_eq!(key_code("-", KeyCode::Digit6), 13);
        assert_eq!(key_code("_", KeyCode::Digit8), 15);
        assert_eq!(key_code("à", KeyCode::Digit0), 7);
    }

    #[test]
    fn punctuation_follows_the_physical_key() {
        assert_eq!(key_code("ß", KeyCode::Minus), 69, "QWERTZ");
        assert_eq!(key_code("ü", KeyCode::BracketLeft), 71, "QWERTZ");
        assert_eq!(key_code("ö", KeyCode::Semicolon), 74, "QWERTZ");
        assert_eq!(key_code("ä", KeyCode::Quote), 75, "QWERTZ");
        assert_eq!(key_code("@", KeyCode::Digit2), 9, "US Shift+2");
        assert_eq!(key_code("#", KeyCode::Digit3), 10, "US Shift+3");
        assert_eq!(key_code("9", KeyCode::Digit9), 16);
        assert_eq!(key_code(".", KeyCode::Period), 56);
        assert_eq!(key_code("/", KeyCode::Slash), 76);
    }

    #[test]
    fn letter_keys_of_non_latin_layouts_follow_the_physical_key() {
        assert_eq!(key_code("ф", KeyCode::KeyA), 29);
        assert_eq!(key_code("ц", KeyCode::KeyW), 51);
        assert_eq!(key_code("Ф", KeyCode::KeyA), 29);
    }

    #[test]
    fn named_keys_send_their_generic_layout_key_codes() {
        let cases = [
            (NamedKey::Shift, KeyCode::ShiftLeft, 59),
            (NamedKey::Control, KeyCode::ControlLeft, 113),
            (NamedKey::AltGraph, KeyCode::AltRight, 58),
            (NamedKey::F1, KeyCode::F1, 131),
            (NamedKey::F12, KeyCode::F12, 142),
            (NamedKey::Enter, KeyCode::NumpadEnter, 160),
            (NamedKey::Insert, KeyCode::Insert, 124),
            (NamedKey::Backspace, KeyCode::Backspace, 67),
            (NamedKey::Enter, KeyCode::Enter, 66),
            (NamedKey::Space, KeyCode::Space, 62),
        ];
        for (key, physical, expected) in cases {
            assert_eq!(named_key_code(key, physical), expected, "{physical:?}");
        }
        assert_eq!(key_code("5", KeyCode::Numpad5), 149);
        assert_eq!(key_code("<", KeyCode::IntlBackslash), 73);
        assert_eq!(key_code("\\", KeyCode::IntlRo), 217);
        assert_eq!(key_code("¥", KeyCode::IntlYen), 216);
    }

    #[test]
    fn escape_stays_android_back() {
        assert_eq!(named_key_code(NamedKey::Escape, KeyCode::Escape), 4);
    }

    fn line(y: f32) -> MouseScrollDelta {
        MouseScrollDelta::LineDelta(0.0, y)
    }

    fn pixels(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, y))
    }

    #[test]
    fn a_wheel_notch_scrolls_one_step() {
        let mut wheel = WheelSteps::default();
        assert_eq!(wheel.notches(line(1.0), 1.0), Some(1.0));
        assert_eq!(wheel.notches(line(-1.0), 2.0), Some(-1.0));
    }

    #[test]
    fn fractional_lines_add_up_to_whole_steps() {
        let mut wheel = WheelSteps::default();
        for _ in 0..3 {
            assert_eq!(wheel.notches(line(0.25), 1.0), None);
        }
        assert_eq!(wheel.notches(line(0.25), 1.0), Some(1.0));
        assert_eq!(wheel.notches(line(1.5), 1.0), Some(1.0));
        assert_eq!(wheel.notches(line(0.5), 1.0), Some(1.0));
    }

    #[test]
    fn pixel_scrolling_steps_every_forty_logical_pixels_at_any_scale() {
        assert_eq!(WheelSteps::default().notches(pixels(80.0), 2.0), Some(1.0));
        assert_eq!(WheelSteps::default().notches(pixels(80.0), 1.0), Some(2.0));
        assert_eq!(
            WheelSteps::default().notches(pixels(-100.0), 1.25),
            Some(-2.0)
        );
    }

    #[test]
    fn reversing_direction_drops_the_remainder() {
        let mut wheel = WheelSteps::default();
        assert_eq!(wheel.notches(line(0.75), 1.0), None);
        assert_eq!(wheel.notches(line(-0.5), 1.0), None);
        assert_eq!(wheel.notches(line(-0.5), 1.0), Some(-1.0));
    }

    #[test]
    fn scrolling_below_one_notch_sends_nothing() {
        let mut wheel = WheelSteps::default();
        assert_eq!(wheel.notches(pixels(39.0), 1.0), None);
        assert_eq!(wheel.notches(pixels(0.0), 1.0), None);
        assert_eq!(wheel.notches(line(0.0), 1.0), None);
    }

    #[test]
    fn keys_without_an_android_key_code_send_unknown() {
        assert_eq!(
            named_key_code(NamedKey::AudioVolumeMute, KeyCode::AudioVolumeMute),
            0
        );
        assert_eq!(
            android_key_code(
                &Key::Unidentified(winit::keyboard::NativeKey::Xkb(0)),
                PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Xkb(0))
            ),
            0
        );
    }

    fn finger(
        tracker: &mut TouchTracker,
        finger: u64,
        phase: TouchPhase,
        x: f32,
        y: f32,
    ) -> Vec<TouchMotion> {
        tracker.touch(finger, phase, x, y).collect()
    }

    fn summary(motion: &TouchMotion) -> (i32, Vec<i32>) {
        (
            motion.action.android_action(),
            motion.pointers().iter().map(|pointer| pointer.id).collect(),
        )
    }

    fn summaries(motions: &[TouchMotion]) -> Vec<(i32, Vec<i32>)> {
        motions.iter().map(summary).collect()
    }

    fn pointer(id: i32, x: f32, y: f32) -> TouchPointer {
        TouchPointer { id, x, y }
    }

    #[test]
    fn one_finger_goes_down_and_up_as_pointer_zero() {
        let mut touch = TouchTracker::default();
        let down = finger(&mut touch, 7, TouchPhase::Started, 10.0, 20.0);
        assert_eq!(down.len(), 1);
        assert_eq!(down[0].action, TouchAction::Down);
        assert_eq!(down[0].pointers(), [pointer(0, 10.0, 20.0)]);
        assert!(touch.is_active());

        let up = finger(&mut touch, 7, TouchPhase::Ended, 11.0, 21.0);
        assert_eq!(summaries(&up), [(1, vec![0])]);
        assert_eq!(up[0].pointers(), [pointer(0, 11.0, 21.0)]);
        assert!(!touch.is_active());
    }

    #[test]
    fn a_second_finger_is_a_pointer_down_at_index_one() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 10.0, 20.0);
        let second = finger(&mut touch, 9, TouchPhase::Started, 30.0, 40.0);
        assert_eq!(summaries(&second), [(0x105, vec![0, 1])]);
        assert_eq!(second[0].action, TouchAction::PointerDown(1));
        assert_eq!(second[0].pointers()[1], pointer(1, 30.0, 40.0));
    }

    #[test]
    fn a_lifted_first_finger_frees_the_lowest_id_for_the_next_finger() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 10.0, 20.0);
        finger(&mut touch, 9, TouchPhase::Started, 30.0, 40.0);

        let lifted = finger(&mut touch, 7, TouchPhase::Ended, 10.0, 20.0);
        assert_eq!(summaries(&lifted), [(0x6, vec![0, 1])]);

        finger(&mut touch, 9, TouchPhase::Moved, 31.0, 41.0);
        let moved = touch.flush().expect("the remaining finger moved");
        assert_eq!(summary(&moved), (2, vec![1]));

        let third = finger(&mut touch, 11, TouchPhase::Started, 50.0, 60.0);
        assert_eq!(summaries(&third), [(0x5, vec![0, 1])]);
        assert_eq!(
            third[0].pointers(),
            [pointer(0, 50.0, 60.0), pointer(1, 31.0, 41.0)]
        );

        let fourth = finger(&mut touch, 13, TouchPhase::Started, 70.0, 80.0);
        assert_eq!(summaries(&fourth), [(0x205, vec![0, 1, 2])]);
    }

    #[test]
    fn moves_inside_one_frame_become_one_move_with_the_last_positions() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 0.0, 0.0);
        finger(&mut touch, 9, TouchPhase::Started, 100.0, 0.0);
        for step in 1..=20 {
            let offset = step as f32;
            assert!(finger(&mut touch, 7, TouchPhase::Moved, offset, offset).is_empty());
            assert!(finger(&mut touch, 9, TouchPhase::Moved, 100.0 - offset, offset).is_empty());
        }
        let moved = touch.flush().expect("one move per frame");
        assert_eq!(moved.action, TouchAction::Move);
        assert_eq!(
            moved.pointers(),
            [pointer(0, 20.0, 20.0), pointer(1, 80.0, 20.0)]
        );
        assert_eq!(touch.flush(), None, "the frame's move went out once");

        finger(&mut touch, 7, TouchPhase::Moved, 20.0, 20.0);
        assert_eq!(touch.flush(), None, "an unchanged position is no move");
    }

    #[test]
    fn an_edge_sends_the_pending_move_first() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 0.0, 0.0);
        finger(&mut touch, 7, TouchPhase::Moved, 5.0, 5.0);
        let second = finger(&mut touch, 9, TouchPhase::Started, 50.0, 50.0);
        assert_eq!(summaries(&second), [(2, vec![0]), (0x105, vec![0, 1])]);
        assert_eq!(second[0].pointers(), [pointer(0, 5.0, 5.0)]);

        finger(&mut touch, 9, TouchPhase::Moved, 40.0, 40.0);
        let lifted = finger(&mut touch, 7, TouchPhase::Ended, 6.0, 6.0);
        assert_eq!(summaries(&lifted), [(2, vec![0, 1]), (0x6, vec![0, 1])]);
        assert_eq!(touch.flush(), None);
    }

    #[test]
    fn an_eleventh_finger_is_ignored() {
        let mut touch = TouchTracker::default();
        for id in 0..10 {
            assert_eq!(
                finger(&mut touch, id, TouchPhase::Started, 0.0, 0.0).len(),
                1
            );
        }
        assert!(finger(&mut touch, 10, TouchPhase::Started, 0.0, 0.0).is_empty());
        assert!(finger(&mut touch, 10, TouchPhase::Moved, 1.0, 1.0).is_empty());
        assert_eq!(touch.flush(), None);
        assert!(finger(&mut touch, 10, TouchPhase::Ended, 1.0, 1.0).is_empty());

        let lifted = finger(&mut touch, 9, TouchPhase::Ended, 0.0, 0.0);
        assert_eq!(summaries(&lifted)[0].0, 0x906);
    }

    #[test]
    fn cancel_ends_every_pointer_and_clears_the_tracker() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 0.0, 0.0);
        finger(&mut touch, 9, TouchPhase::Started, 10.0, 10.0);
        finger(&mut touch, 11, TouchPhase::Started, 20.0, 20.0);
        finger(&mut touch, 9, TouchPhase::Moved, 12.0, 12.0);

        let cancelled: Vec<_> = touch.cancel().collect();
        assert_eq!(
            summaries(&cancelled),
            [
                (2, vec![0, 1, 2]),
                (0x203, vec![0, 1, 2]),
                (0x103, vec![0, 1]),
                (0x3, vec![0]),
            ]
        );
        assert!(!touch.is_active());
        assert_eq!(touch.cancel().count(), 0);
        assert_eq!(
            summaries(&finger(&mut touch, 7, TouchPhase::Started, 0.0, 0.0)),
            [(0, vec![0])]
        );
    }

    #[test]
    fn a_cancelled_finger_ends_only_its_own_pointer() {
        let mut touch = TouchTracker::default();
        finger(&mut touch, 7, TouchPhase::Started, 0.0, 0.0);
        finger(&mut touch, 9, TouchPhase::Started, 10.0, 10.0);
        let cancelled = finger(&mut touch, 7, TouchPhase::Cancelled, 0.0, 0.0);
        assert_eq!(summaries(&cancelled), [(0x3, vec![0, 1])]);
        let last = finger(&mut touch, 9, TouchPhase::Cancelled, 10.0, 10.0);
        assert_eq!(summaries(&last), [(0x3, vec![1])]);
        assert!(!touch.is_active());
    }

    #[test]
    fn the_first_finger_alone_drives_the_primary_pointer() {
        let mut touch = TouchTracker::default();
        let primary = |motions: Vec<TouchMotion>| -> Vec<PrimaryTouch> {
            motions.iter().filter_map(TouchMotion::primary).collect()
        };
        assert_eq!(
            primary(finger(&mut touch, 7, TouchPhase::Started, 1.0, 2.0)),
            [PrimaryTouch::Press { x: 1.0, y: 2.0 }]
        );
        assert!(primary(finger(&mut touch, 9, TouchPhase::Started, 50.0, 50.0)).is_empty());
        finger(&mut touch, 7, TouchPhase::Moved, 3.0, 4.0);
        assert_eq!(
            touch.flush().and_then(|motion| motion.primary()),
            Some(PrimaryTouch::Move { x: 3.0, y: 4.0 })
        );
        assert!(primary(finger(&mut touch, 9, TouchPhase::Ended, 50.0, 50.0)).is_empty());
        assert_eq!(
            primary(finger(&mut touch, 7, TouchPhase::Ended, 5.0, 6.0)),
            [PrimaryTouch::Release { x: 5.0, y: 6.0 }]
        );

        finger(&mut touch, 7, TouchPhase::Started, 1.0, 2.0);
        finger(&mut touch, 9, TouchPhase::Started, 50.0, 50.0);
        finger(&mut touch, 7, TouchPhase::Ended, 1.0, 2.0);
        finger(&mut touch, 9, TouchPhase::Moved, 60.0, 60.0);
        assert_eq!(touch.flush().and_then(|motion| motion.primary()), None);
        let cancelled: Vec<_> = touch.cancel().collect();
        assert!(primary(cancelled).is_empty());

        finger(&mut touch, 7, TouchPhase::Started, 1.0, 2.0);
        assert_eq!(primary(touch.cancel().collect()), [PrimaryTouch::Cancel]);
    }
}
