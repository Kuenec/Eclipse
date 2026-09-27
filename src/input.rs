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
    let letter = match logical {
        Key::Character(character) if character.chars().all(|c| c.is_ascii_alphabetic()) => {
            let mut letters = character.chars();
            match (letters.next(), letters.next()) {
                (Some(letter), None) => letter.to_ascii_lowercase(),
                _ => return None,
            }
        }
        Key::Character(_) => physical_shortcut_letter(physical)?,
        _ => return None,
    };
    match letter {
        'a' => Some(Shortcut::SelectAll),
        'c' => Some(Shortcut::Clipboard(ClipboardShortcut::Copy)),
        'x' => Some(Shortcut::Clipboard(ClipboardShortcut::Cut)),
        'v' => Some(Shortcut::Clipboard(ClipboardShortcut::Paste)),
        _ => None,
    }
}

fn physical_shortcut_letter(physical: PhysicalKey) -> Option<char> {
    match physical {
        PhysicalKey::Code(KeyCode::KeyA) => Some('a'),
        PhysicalKey::Code(KeyCode::KeyC) => Some('c'),
        PhysicalKey::Code(KeyCode::KeyX) => Some('x'),
        PhysicalKey::Code(KeyCode::KeyV) => Some('v'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
