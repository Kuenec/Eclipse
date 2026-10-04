use std::fmt;
use std::process::Command;

use serde::{Deserialize, Serialize};
use wayland_client::delegate_noop;
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use winit::window::{UserAttentionType, Window};

use super::wayland_window::{RequestError, SurfaceRequests, WaylandWindow};

const LONGEST_TOKEN: usize = 256;
const LAUNCH_TOKEN_VARIABLES: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Token(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidToken;

impl fmt::Display for InvalidToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "an activation token must be 1 to {LONGEST_TOKEN} printable ASCII characters without \
             spaces"
        )
    }
}

impl std::error::Error for InvalidToken {}

impl Token {
    pub fn parse(text: String) -> Result<Self, InvalidToken> {
        let valid = (1..=LONGEST_TOKEN).contains(&text.len())
            && text.bytes().all(|byte| byte.is_ascii_graphic());
        if valid {
            Ok(Self(text))
        } else {
            Err(InvalidToken)
        }
    }

    pub fn from_launch_environment() -> Option<Self> {
        LAUNCH_TOKEN_VARIABLES
            .iter()
            .find_map(|name| Self::parse(std::env::var(name).ok()?).ok())
    }

    pub fn pass_to(&self, command: &mut Command) {
        for name in LAUNCH_TOKEN_VARIABLES {
            command.env(name, &self.0);
        }
    }

    pub fn withhold_from(command: &mut Command) {
        for name in LAUNCH_TOKEN_VARIABLES {
            command.env_remove(name);
        }
    }
}

impl TryFrom<String> for Token {
    type Error = InvalidToken;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(text)
    }
}

impl From<Token> for String {
    fn from(token: Token) -> Self {
        token.0
    }
}

pub(crate) fn raise(window: &Window, token: Option<&Token>) {
    let surface = match WaylandWindow::of(window) {
        Ok(surface) => surface,
        Err(error) => {
            tracing::warn!(%error, "the window cannot be brought to the front without its handle");
            return;
        }
    };
    match (surface, token) {
        (None, _) => window.focus_window(),
        (Some(_), None) => window.request_user_attention(Some(UserAttentionType::Informational)),
        (Some(surface), Some(token)) => match activate(&surface, token) {
            Ok(()) => tracing::info!("asked the compositor to bring the window to the front"),
            Err(error) => {
                tracing::warn!(%error, "the compositor was not asked to bring the window to the front");
            }
        },
    }
}

delegate_noop!(SurfaceRequests: XdgActivationV1);

fn activate(target: &WaylandWindow<'_>, token: &Token) -> Result<(), RequestError> {
    target.send(1..=1, |activation: &XdgActivationV1, surface, _| {
        activation.activate(token.0.clone(), surface);
        activation.destroy();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_one_to_256_printable_ascii_characters_without_spaces() {
        for valid in [
            "a".to_owned(),
            "x".repeat(LONGEST_TOKEN),
            "_mutter-1234_TIME5678".to_owned(),
            "kwin-2f1c0a4e-9b0e-4f5f-a8f3-0e7c1f3e9d2a".to_owned(),
        ] {
            assert_eq!(Token::parse(valid.clone()), Ok(Token(valid)));
        }
        for invalid in [
            String::new(),
            "x".repeat(LONGEST_TOKEN + 1),
            "two words".to_owned(),
            "bell\u{7}".to_owned(),
            "tab\there".to_owned(),
            "caf\u{e9}".to_owned(),
        ] {
            assert_eq!(
                Token::parse(invalid.clone()),
                Err(InvalidToken),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn a_withheld_token_reaches_the_command_under_neither_startup_variable() {
        use std::collections::BTreeMap;
        use std::ffi::OsStr;

        let mut command = Command::new("eclipse-settings");
        Token::withhold_from(&mut command);
        let withheld: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            withheld,
            BTreeMap::from([
                (OsStr::new("DESKTOP_STARTUP_ID"), None),
                (OsStr::new("XDG_ACTIVATION_TOKEN"), None)
            ])
        );
    }

    #[test]
    fn a_token_is_handed_to_a_restarted_launch_under_both_startup_variables() {
        use std::collections::BTreeMap;
        use std::ffi::OsStr;

        let mut command = Command::new("eclipse");
        Token::parse("t-1".to_owned())
            .unwrap()
            .pass_to(&mut command);
        let passed: BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            passed,
            BTreeMap::from([
                (OsStr::new("DESKTOP_STARTUP_ID"), Some(OsStr::new("t-1"))),
                (OsStr::new("XDG_ACTIVATION_TOKEN"), Some(OsStr::new("t-1")))
            ])
        );
    }

    #[test]
    fn activation_hands_the_token_and_the_window_surface_to_xdg_activation() {
        use crate::graphics::fake_compositor::{wire_string, FakeCompositor, Request};
        use wayland_client::protocol::{wl_display, wl_registry};
        use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1;

        let mut compositor = FakeCompositor::offering(&[("xdg_activation_v1", 1)]);
        let (display, window) = compositor.handles();
        let target = unsafe { WaylandWindow::from_handles_that_outlive_it(display, window) }
            .expect("the fake compositor hands out Wayland handles");
        activate(&target, &Token::parse("t-1".to_owned()).unwrap()).expect("an activation");
        let requests = compositor.requests_since_last_call();
        assert_eq!(
            requests.iter().map(Request::call).collect::<Vec<_>>(),
            [
                ("wl_display", wl_display::REQ_GET_REGISTRY_OPCODE),
                ("wl_display", wl_display::REQ_SYNC_OPCODE),
                ("wl_registry", wl_registry::REQ_BIND_OPCODE),
                ("xdg_activation_v1", xdg_activation_v1::REQ_ACTIVATE_OPCODE),
                ("xdg_activation_v1", xdg_activation_v1::REQ_DESTROY_OPCODE),
            ]
        );
        assert_eq!(
            requests[3].words,
            [wire_string("t-1"), vec![compositor.surface_id()]].concat()
        );
    }

    #[test]
    fn tokens_cross_the_wire_as_plain_strings_and_are_checked_on_arrival() {
        let token = Token::parse("abc".to_owned()).unwrap();
        assert_eq!(serde_json::to_string(&token).unwrap(), r#""abc""#);
        assert_eq!(serde_json::from_str::<Token>(r#""abc""#).unwrap(), token);
        for invalid in [r#""""#, r#""a b""#, r#""a\u0000""#] {
            assert!(serde_json::from_str::<Token>(invalid).is_err(), "{invalid}");
        }
    }
}
