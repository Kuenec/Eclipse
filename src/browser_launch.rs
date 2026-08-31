use std::fmt;

const MAX_PROTOCOL_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) struct BrowserLaunchError;

impl fmt::Display for BrowserLaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Roblox browser launch URL")
    }
}

impl std::error::Error for BrowserLaunchError {}

pub(super) fn place_id(input: &str) -> Result<u64, BrowserLaunchError> {
    parse_place_id(input).ok_or(BrowserLaunchError)
}

fn parse_place_id(input: &str) -> Option<u64> {
    if input.is_empty()
        || input.len() > MAX_PROTOCOL_BYTES
        || !input.as_bytes().iter().all(u8::is_ascii_graphic)
    {
        return None;
    }

    let (scheme, payload) = input.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("roblox-player") || payload.starts_with("//") {
        return None;
    }

    let mut fields = payload.split('+');
    if fields.next()? != "1" {
        return None;
    }

    let mut saw_launch_mode = false;
    let mut saw_game_info = false;
    let mut launcher_url = None;
    let mut saw_launch_time = false;
    let mut saw_browser_tracker = false;
    let mut saw_roblox_locale = false;
    let mut saw_game_locale = false;
    let mut saw_channel = false;
    let mut saw_launch_experiment = false;

    for field in fields {
        let (name, value) = field.split_once(':')?;
        match name {
            "launchmode" if !saw_launch_mode && value == "play" => saw_launch_mode = true,
            "gameinfo" if !saw_game_info && !value.is_empty() => {
                // The authentication ticket is intentionally discarded here. Only the place ID
                // from Roblox's trusted launcher URL crosses into the Android client.
                saw_game_info = true;
            }
            "placelauncherurl" if launcher_url.is_none() && !value.is_empty() => {
                launcher_url = Some(value);
            }
            "launchtime" if !saw_launch_time && decimal(value, false).is_some() => {
                saw_launch_time = true;
            }
            "browsertrackerid" if !saw_browser_tracker && decimal(value, true).is_some() => {
                saw_browser_tracker = true;
            }
            "robloxLocale" if !saw_roblox_locale && identifier(value, 16, false) => {
                saw_roblox_locale = true;
            }
            "gameLocale" if !saw_game_locale && identifier(value, 16, false) => {
                saw_game_locale = true;
            }
            "channel" if !saw_channel && identifier(value, 64, true) => saw_channel = true,
            "LaunchExp" if !saw_launch_experiment && value == "InApp" => {
                saw_launch_experiment = true;
            }
            _ => return None,
        }
    }

    if !saw_launch_mode || !saw_game_info {
        return None;
    }
    parse_launcher_url(&strict_percent_decode(launcher_url?)?)
}

fn strict_percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    if decoded.is_empty() || decoded.contains(&b'%') || !decoded.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    String::from_utf8(decoded).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_launcher_url(value: &str) -> Option<u64> {
    let (scheme, remainder) = value.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let (authority, path_and_query) = remainder.split_once('/')?;
    if !authority.eq_ignore_ascii_case("assetgame.roblox.com")
        && !authority.eq_ignore_ascii_case("www.roblox.com")
    {
        return None;
    }

    let (path, query) = path_and_query.split_once('?')?;
    if !launcher_path(path) || query.is_empty() || query.contains('#') {
        return None;
    }

    let mut saw_request = false;
    let mut place_id = None;
    let mut saw_browser_tracker = false;
    let mut saw_play_together = false;
    let mut saw_party_leader = false;
    let mut saw_referring_player = false;
    let mut saw_join_attempt_id = false;
    let mut saw_join_attempt_origin = false;

    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        match name {
            "request" if !saw_request && value == "RequestGame" => saw_request = true,
            "placeId" if place_id.is_none() => place_id = decimal(value, false),
            "browserTrackerId" if !saw_browser_tracker && decimal(value, true).is_some() => {
                saw_browser_tracker = true;
            }
            "isPlayTogetherGame" if !saw_play_together && value == "false" => {
                saw_play_together = true;
            }
            "isPartyLeader" if !saw_party_leader && value == "false" => {
                saw_party_leader = true;
            }
            "referredByPlayerId" if !saw_referring_player && decimal(value, true).is_some() => {
                saw_referring_player = true;
            }
            "joinAttemptId" if !saw_join_attempt_id && identifier(value, 64, false) => {
                saw_join_attempt_id = true;
            }
            "joinAttemptOrigin" if !saw_join_attempt_origin && identifier(value, 64, false) => {
                saw_join_attempt_origin = true;
            }
            _ => return None,
        }
    }

    if saw_request {
        place_id
    } else {
        None
    }
}

fn launcher_path(path: &str) -> bool {
    if matches!(path, "game/PlaceLauncher.ashx" | "Game/PlaceLauncher.ashx") {
        return true;
    }
    let mut segments = path.split('/');
    let (Some(locale), Some("Game"), Some("PlaceLauncher.ashx"), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    !locale.is_empty()
        && locale.len() <= 16
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn decimal(value: &str, allow_zero: bool) -> Option<u64> {
    if value.is_empty()
        || value.len() > 20
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return None;
    }
    let parsed = value.parse::<u64>().ok()?;
    (allow_zero || parsed > 0).then_some(parsed)
}

fn identifier(value: &str, max_len: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLACE_ID: u64 = 90_441_122_676_618;
    const SAMPLE: &str = "roblox-player:1+launchmode:play+gameinfo:[TICKET]+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestGame%26placeId%3D90441122676618";

    #[test]
    fn extracts_only_the_place_id_from_the_browser_protocol() {
        assert_eq!(place_id(SAMPLE).unwrap(), PLACE_ID);
        assert_eq!(
            place_id(&SAMPLE.replacen("roblox-player", "ROBLOX-PLAYER", 1)).unwrap(),
            PLACE_ID
        );
    }

    #[test]
    fn accepts_current_roblox_web_metadata() {
        let protocol = "roblox-player:1+launchmode:play+gameinfo:SECRET_TICKET+launchtime:1788192000000+placelauncherurl:https%3A%2F%2Fwww.roblox.com%2Ffr%2FGame%2FPlaceLauncher.ashx%3FjoinAttemptOrigin%3DPlayButton%26placeId%3D90441122676618%26request%3DRequestGame%26browserTrackerId%3D216042055264%26isPlayTogetherGame%3Dfalse%26isPartyLeader%3Dfalse%26referredByPlayerId%3D0%26joinAttemptId%3D3a5e0cf49-3e23-46a0-9dc7-887dad37e760+browsertrackerid:216042055264+robloxLocale:fr_fr+gameLocale:fr_fr+channel:+LaunchExp:InApp";
        assert_eq!(place_id(protocol).unwrap(), PLACE_ID);
    }

    #[test]
    fn rejects_untrusted_or_lossy_targets() {
        let invalid = [
            SAMPLE.replacen("roblox-player:", "https:", 1),
            SAMPLE.replacen("roblox-player:1", "roblox-player://1", 1),
            SAMPLE.replacen("launchmode:play", "launchmode:edit", 1),
            SAMPLE.replacen("gameinfo:[TICKET]", "gameinfo:", 1),
            format!("{SAMPLE}+unknown:value"),
            SAMPLE.replacen("https%3A", "http%3A", 1),
            SAMPLE.replacen("assetgame.roblox.com", "assetgame.roblox.com.evil", 1),
            SAMPLE.replacen("PlaceLauncher.ashx", "Other.ashx", 1),
            SAMPLE.replacen("RequestGame", "RequestPrivateGame", 1),
            SAMPLE.replacen("placeId%3D90441122676618", "placeId%3D090441122676618", 1),
            SAMPLE.replacen("https%3A", "https%253A", 1),
            SAMPLE.replacen("%2F", "%XZ", 1),
            format!("{SAMPLE}\n"),
        ];
        for protocol in invalid {
            assert!(
                place_id(&protocol).is_err(),
                "accepted invalid protocol shape"
            );
        }
    }

    #[test]
    fn rejects_duplicate_and_server_specific_parameters() {
        let duplicate = SAMPLE.replace(
            "%26placeId%3D90441122676618",
            "%26placeId%3D90441122676618%26placeId%3D1",
        );
        let server = SAMPLE.replace(
            "%26placeId%3D90441122676618",
            "%26placeId%3D90441122676618%26gameId%3Ddeadbeef",
        );
        assert!(place_id(&duplicate).is_err());
        assert!(place_id(&server).is_err());
    }

    #[test]
    fn errors_never_echo_the_authentication_ticket() {
        let secret = "SUPER_SECRET_TICKET_4f9d8c";
        let invalid = format!("roblox-player:1+launchmode:edit+gameinfo:{secret}");
        let error = place_id(&invalid).unwrap_err().to_string();
        assert!(!error.contains(secret));
        assert_eq!(error, "invalid Roblox browser launch URL");
    }
}
