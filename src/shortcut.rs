#![forbid(unsafe_code)]

use std::fmt::{self, Write as _};

use crate::links::{LaunchTarget, PlaceId};

const SANDBOX_PROGRAM: &str = "eclipse";
const OPEN_COMMAND: &str = "open";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceShortcut(PlaceId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShortcutRefusal {
    LaunchData,
    NotAGame(&'static str),
}

impl fmt::Display for ShortcutRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LaunchData => f.write_str(
                "A shortcut opens a game without launch data; give the game's page link or its \
                 place ID.",
            ),
            Self::NotAGame(kind) => write!(
                f,
                "Shortcuts open games only, not {kind} links, so that no server, join code or \
                 account is stored in the app menu; give the game's page link or its place ID."
            ),
        }
    }
}

impl std::error::Error for ShortcutRefusal {}

pub enum Launcher<'a> {
    Portal,
    Host {
        quoted_program: &'a str,
        name: &'a str,
        icon: &'a str,
    },
}

impl PlaceShortcut {
    pub fn for_link(target: &LaunchTarget) -> Result<Self, ShortcutRefusal> {
        match target {
            LaunchTarget::Place {
                place,
                launch_data: None,
            } => Ok(Self(*place)),
            LaunchTarget::Place {
                launch_data: Some(_),
                ..
            } => Err(ShortcutRefusal::LaunchData),
            LaunchTarget::Server { .. }
            | LaunchTarget::PrivateServer { .. }
            | LaunchTarget::FollowUser { .. }
            | LaunchTarget::Share { .. } => Err(ShortcutRefusal::NotAGame(target.kind())),
        }
    }

    pub fn place(self) -> PlaceId {
        self.0
    }

    pub fn desktop_file_id(self, app_id: &str) -> String {
        format!("{app_id}.Place{}.desktop", self.0)
    }

    pub fn desktop_entry(self, launcher: Launcher<'_>) -> String {
        let mut entry = String::from("[Desktop Entry]\nVersion=1.5\nType=Application\n");
        let program = match launcher {
            Launcher::Portal => SANDBOX_PROGRAM,
            Launcher::Host {
                quoted_program,
                name,
                icon,
            } => {
                writeln!(entry, "Name={}\nIcon={}", escaped(name), escaped(icon))
                    .expect("writing to a String cannot fail");
                quoted_program
            }
        };
        let link = LaunchTarget::Place {
            place: self.0,
            launch_data: None,
        }
        .android_uri();
        writeln!(entry, "Exec={program} {OPEN_COMMAND} {link}")
            .expect("writing to a String cannot fail");
        entry.push_str("Terminal=false\nStartupNotify=true\nCategories=Game;\n");
        entry
    }
}

fn escaped(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            ' ' if index == 0 => escaped.push_str("\\s"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::parse;

    const ACCESS: &str = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
    const JOB: &str = "3a5e0cf4-3e23-46a0-9dc7-887dad37e760";
    const SHARE: &str = "2f4c6e8a0b1d3f5a7c9e1b3d5f7a9c0e";

    fn shortcut(link: &str) -> Result<PlaceShortcut, ShortcutRefusal> {
        PlaceShortcut::for_link(&parse(link).expect(link))
    }

    #[test]
    fn game_links_and_place_ids_become_shortcuts_to_the_place() {
        for link in [
            "1818",
            "https://www.roblox.com/games/1818/Crossroads",
            "https://www.roblox.com/fr/games/1818",
            "roblox://placeId=1818",
            "roblox://experiences/start?placeId=1818",
            "roblox-player:1+launchmode:play+gameinfo:TICKET+placelauncherurl:https%3A%2F%2F\
             assetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestGame%26\
             placeId%3D1818",
        ] {
            let place = shortcut(link).expect(link).place();
            assert_eq!(place.to_string(), "1818", "{link}");
        }
    }

    #[test]
    fn a_private_server_link_is_refused_without_repeating_its_codes() {
        for link in [
            format!("roblox://placeId=1818&accessCode={ACCESS}&linkCode=123456789"),
            "https://www.roblox.com/games/1818/Crossroads?privateServerLinkCode=0123456789"
                .to_owned(),
        ] {
            let refused = shortcut(&link).expect_err(&link);
            assert_eq!(refused, ShortcutRefusal::NotAGame("private-server"));
            let message = refused.to_string();
            assert_eq!(
                message,
                "Shortcuts open games only, not private-server links, so that no server, join \
                 code or account is stored in the app menu; give the game's page link or its \
                 place ID."
            );
            for secret in [ACCESS, "123456789", "0123456789"] {
                assert!(!message.contains(secret), "{message}");
            }
        }
    }

    #[test]
    fn server_friend_share_and_launch_data_links_are_refused() {
        for (link, refusal) in [
            (
                format!("roblox://placeId=1818&gameInstanceId={JOB}"),
                ShortcutRefusal::NotAGame("server"),
            ),
            (
                "roblox://userId=156".to_owned(),
                ShortcutRefusal::NotAGame("friend"),
            ),
            (
                format!("https://www.roblox.com/share?code={SHARE}&type=Server"),
                ShortcutRefusal::NotAGame("share"),
            ),
            (
                "roblox://placeId=1818&launchData=mode%3Dhard".to_owned(),
                ShortcutRefusal::LaunchData,
            ),
        ] {
            assert_eq!(shortcut(&link), Err(refusal), "{link}");
        }
        assert_eq!(
            ShortcutRefusal::LaunchData.to_string(),
            "A shortcut opens a game without launch data; give the game's page link or its place \
             ID."
        );
    }

    #[test]
    fn the_desktop_file_id_names_the_app_and_the_place() {
        assert_eq!(
            shortcut("1818")
                .unwrap()
                .desktop_file_id("io.github.kuenec.Eclipse"),
            "io.github.kuenec.Eclipse.Place1818.desktop"
        );
    }

    #[test]
    fn the_portal_entry_runs_the_sandboxed_eclipse_with_only_the_place_link() {
        assert_eq!(
            shortcut("https://www.roblox.com/games/1818/Crossroads")
                .unwrap()
                .desktop_entry(Launcher::Portal),
            "[Desktop Entry]\n\
             Version=1.5\n\
             Type=Application\n\
             Exec=eclipse open roblox://placeId=1818\n\
             Terminal=false\n\
             StartupNotify=true\n\
             Categories=Game;\n"
        );
    }

    #[test]
    fn the_host_entry_names_the_experience_and_escapes_its_values() {
        assert_eq!(
            shortcut("1818").unwrap().desktop_entry(Launcher::Host {
                quoted_program: "\"/opt/eclipse dev/eclipse\"",
                name: " Cross\\roads",
                icon: "/home/a b/.local/share/icons/x.png",
            }),
            "[Desktop Entry]\n\
             Version=1.5\n\
             Type=Application\n\
             Name=\\sCross\\\\roads\n\
             Icon=/home/a b/.local/share/icons/x.png\n\
             Exec=\"/opt/eclipse dev/eclipse\" open roblox://placeId=1818\n\
             Terminal=false\n\
             StartupNotify=true\n\
             Categories=Game;\n"
        );
        assert_eq!(escaped("a\nb\tc\rd"), "a\\nb\\tc\\rd");
    }
}
