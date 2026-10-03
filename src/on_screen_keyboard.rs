use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt as _;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::framework::FocusedTextBox;
use crate::portal::{self, PortalError, PortalRequest, SteamKeyboard, SteamKeyboardMode};

const STEAM_KEYBOARD_HINT: &str = "SDL_ENABLE_STEAM_SCREEN_KEYBOARD";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnScreenKeyboard {
    Desktop,
    SteamClosed,
    SteamOpen,
}

impl OnScreenKeyboard {
    pub(crate) fn from_environment() -> Self {
        let chosen = Self::chosen_by(|name| std::env::var_os(name));
        if chosen == Self::SteamClosed {
            tracing::info!(
                "Steam's on-screen keyboard opens when a Roblox text box gains focus \
                 ({STEAM_KEYBOARD_HINT} is set)"
            );
        }
        chosen
    }

    fn chosen_by(variable: impl FnOnce(&str) -> Option<OsString>) -> Self {
        let steam_asked =
            variable(STEAM_KEYBOARD_HINT).is_some_and(|value| sdl_hint_is_true(value.as_bytes()));
        if steam_asked {
            Self::SteamClosed
        } else {
            Self::Desktop
        }
    }

    pub(crate) fn follow(&mut self, text_box: Option<FocusedTextBox>) {
        self.follow_with(text_box, portal::submit);
    }

    fn follow_with(
        &mut self,
        text_box: Option<FocusedTextBox>,
        submit: impl FnOnce(PortalRequest) -> Result<(), PortalError>,
    ) {
        let (next, keyboard) = match (*self, text_box) {
            (Self::SteamClosed, Some(text_box)) => (Self::SteamOpen, steam_keyboard_for(text_box)),
            (Self::SteamOpen, None) => (Self::SteamClosed, SteamKeyboard::Close),
            (Self::Desktop, _) | (Self::SteamClosed, None) | (Self::SteamOpen, Some(_)) => return,
        };
        *self = next;
        tracing::debug!(?keyboard, "asking Steam to change its on-screen keyboard");
        if let Err(error) = submit(PortalRequest::SteamKeyboard(keyboard)) {
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if REPORTED.swap(true, Ordering::Relaxed) {
                tracing::debug!(%error, ?keyboard, "could not ask Steam to change its keyboard");
            } else {
                tracing::warn!(%error, ?keyboard, "could not ask Steam to change its keyboard");
            }
        }
    }
}

fn sdl_hint_is_true(value: &[u8]) -> bool {
    !(value.is_empty() || value.starts_with(b"0") || value.eq_ignore_ascii_case(b"false"))
}

fn steam_keyboard_for(text_box: FocusedTextBox) -> SteamKeyboard {
    let (x, y, width, height) = text_box.geometry;
    SteamKeyboard::Open {
        x,
        y,
        width,
        height,
        mode: if text_box.multiline {
            SteamKeyboardMode::MultipleLines
        } else {
            SteamKeyboardMode::SingleLine
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const LINE: FocusedTextBox = FocusedTextBox {
        geometry: (40, 620, 900, 52),
        multiline: false,
        masked: false,
    };

    fn chosen_in(environment: &[(&str, &str)]) -> OnScreenKeyboard {
        OnScreenKeyboard::chosen_by(|name| {
            environment
                .iter()
                .find(|(set, _)| *set == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    fn requested(
        mut keyboard: OnScreenKeyboard,
        focus: &[Option<FocusedTextBox>],
        answer: fn() -> Result<(), PortalError>,
    ) -> (Vec<SteamKeyboard>, OnScreenKeyboard) {
        let sent = RefCell::new(Vec::new());
        for &text_box in focus {
            keyboard.follow_with(text_box, |request| {
                let PortalRequest::SteamKeyboard(change) = request else {
                    panic!("only Steam keyboard requests are expected here");
                };
                sent.borrow_mut().push(change);
                answer()
            });
        }
        (sent.into_inner(), keyboard)
    }

    #[test]
    fn steam_is_chosen_only_for_the_values_sdl_reads_as_true() {
        for value in ["1", "TRUE", "true", "yes", "2"] {
            assert_eq!(
                chosen_in(&[(STEAM_KEYBOARD_HINT, value)]),
                OnScreenKeyboard::SteamClosed,
                "{value:?}"
            );
        }
        for value in ["0", "01", "false", "FALSE", ""] {
            assert_eq!(
                chosen_in(&[(STEAM_KEYBOARD_HINT, value)]),
                OnScreenKeyboard::Desktop,
                "{value:?}"
            );
        }
    }

    #[test]
    fn a_steam_deck_or_gamescope_alone_keeps_the_desktop_keyboard() {
        for environment in [
            &[][..],
            &[("SteamDeck", "1")][..],
            &[
                ("SteamDeck", "1"),
                ("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-0"),
            ][..],
        ] {
            assert_eq!(
                chosen_in(environment),
                OnScreenKeyboard::Desktop,
                "{environment:?}"
            );
        }
    }

    #[test]
    fn focusing_a_text_box_opens_the_keyboard_over_it() {
        let multiline = FocusedTextBox {
            geometry: (-8, 0, 1280, 300),
            multiline: true,
            masked: false,
        };
        for (text_box, opened) in [
            (
                LINE,
                SteamKeyboard::Open {
                    x: 40,
                    y: 620,
                    width: 900,
                    height: 52,
                    mode: SteamKeyboardMode::SingleLine,
                },
            ),
            (
                multiline,
                SteamKeyboard::Open {
                    x: -8,
                    y: 0,
                    width: 1280,
                    height: 300,
                    mode: SteamKeyboardMode::MultipleLines,
                },
            ),
        ] {
            assert_eq!(
                requested(
                    OnScreenKeyboard::SteamClosed,
                    &[None, Some(text_box)],
                    || Ok(())
                ),
                (vec![opened], OnScreenKeyboard::SteamOpen)
            );
        }
    }

    #[test]
    fn a_masked_text_box_opens_the_keyboard_too() {
        let password = FocusedTextBox {
            masked: true,
            ..LINE
        };
        let (sent, keyboard) =
            requested(OnScreenKeyboard::SteamClosed, &[Some(password)], || Ok(()));
        assert!(matches!(sent[..], [SteamKeyboard::Open { .. }]), "{sent:?}");
        assert_eq!(keyboard, OnScreenKeyboard::SteamOpen);
    }

    #[test]
    fn leaving_the_text_box_closes_the_keyboard() {
        assert_eq!(
            requested(OnScreenKeyboard::SteamOpen, &[None, None], || Ok(())),
            (vec![SteamKeyboard::Close], OnScreenKeyboard::SteamClosed)
        );
    }

    #[test]
    fn moving_between_text_boxes_leaves_the_keyboard_open() {
        let next_box = FocusedTextBox {
            geometry: (40, 700, 900, 52),
            ..LINE
        };
        let (sent, keyboard) = requested(
            OnScreenKeyboard::SteamClosed,
            &[Some(LINE), Some(LINE), Some(next_box), None],
            || Ok(()),
        );
        assert!(
            matches!(
                sent[..],
                [SteamKeyboard::Open { y: 620, .. }, SteamKeyboard::Close]
            ),
            "{sent:?}"
        );
        assert_eq!(keyboard, OnScreenKeyboard::SteamClosed);
    }

    #[test]
    fn the_desktop_keyboard_sends_nothing() {
        assert_eq!(
            requested(
                OnScreenKeyboard::Desktop,
                &[Some(LINE), None, Some(LINE)],
                || Ok(())
            ),
            (Vec::new(), OnScreenKeyboard::Desktop)
        );
    }

    #[test]
    fn a_refused_request_is_recorded_and_not_retried() {
        let (sent, keyboard) = requested(
            OnScreenKeyboard::SteamClosed,
            &[Some(LINE), Some(LINE), Some(LINE)],
            || Err(PortalError::Stopped),
        );
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(keyboard, OnScreenKeyboard::SteamOpen);
    }
}
