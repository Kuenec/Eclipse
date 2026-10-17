#![forbid(unsafe_code)]

use std::fmt;
use std::num::NonZeroU64;

use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::https::{self, DownloadError, Host};
use crate::links::PlaceId;

const UNIVERSE_HOSTS: &[Host] = &[Host::Exact("apis.roblox.com")];
const GAME_HOSTS: &[Host] = &[Host::Exact("games.roblox.com")];
const THUMBNAIL_HOSTS: &[Host] = &[Host::Exact("thumbnails.roblox.com")];
const ICON_IMAGE_HOSTS: &[Host] = &[Host::SubdomainOf("rbxcdn.com")];
const ANSWER_LIMIT: u64 = 64 * 1024;
const ICON_IMAGE_LIMIT: u64 = 1024 * 1024;
const ICON_REQUEST_SIZE: &str = "256x256";
const LARGEST_ICON_SIDE: u32 = 512;
const NAME_LIMIT: usize = 128;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const PNG_HEADER_CHUNK: &[u8] = b"IHDR";

#[derive(Debug)]
pub struct Experience {
    name: String,
    icon: Png,
}

impl Experience {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn icon(&self) -> &Png {
        &self.icon
    }
}

#[derive(Debug)]
pub struct Png {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
}

impl Png {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, ExperienceError> {
        let (width, height) = png_dimensions(&bytes).ok_or(ExperienceError::IconNotPng)?;
        if width > LARGEST_ICON_SIDE || height > LARGEST_ICON_SIDE {
            return Err(ExperienceError::IconTooLarge { width, height });
        }
        Ok(Self {
            bytes,
            width,
            height,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..24)?;
    if !header.starts_with(PNG_SIGNATURE) || &header[12..16] != PNG_HEADER_CHUNK {
        return None;
    }
    let width = u32::from_be_bytes(header[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(header[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    Universe,
    Details,
    Icon,
    IconImage,
}

impl Lookup {
    fn hosts(self) -> &'static [Host] {
        match self {
            Self::Universe => UNIVERSE_HOSTS,
            Self::Details => GAME_HOSTS,
            Self::Icon => THUMBNAIL_HOSTS,
            Self::IconImage => ICON_IMAGE_HOSTS,
        }
    }

    fn limit(self) -> u64 {
        match self {
            Self::Universe | Self::Details | Self::Icon => ANSWER_LIMIT,
            Self::IconImage => ICON_IMAGE_LIMIT,
        }
    }

    fn subject(self) -> &'static str {
        match self {
            Self::Universe => "the experience that holds the place",
            Self::Details => "the experience's name",
            Self::Icon => "the experience's icon",
            Self::IconImage => "the icon image",
        }
    }
}

#[derive(Debug)]
pub enum ExperienceError {
    Fetch {
        lookup: Lookup,
        cause: DownloadError,
    },
    Answer {
        lookup: Lookup,
        cause: serde_json::Error,
    },
    UnknownPlace(PlaceId),
    NoDetails,
    NoName,
    NoIcon,
    IconNotPng,
    IconTooLarge {
        width: u32,
        height: u32,
    },
}

impl fmt::Display for ExperienceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch { lookup, cause } => write!(
                f,
                "cannot ask {} for {}: {cause}",
                lookup.hosts()[0],
                lookup.subject()
            ),
            Self::Answer { lookup, cause } => write!(
                f,
                "{} sent an unexpected answer about {}: {cause}",
                lookup.hosts()[0],
                lookup.subject()
            ),
            Self::UnknownPlace(place) => write!(f, "Roblox knows no experience with place {place}"),
            Self::NoDetails => f.write_str("Roblox sent no details about the experience"),
            Self::NoName => f.write_str("the experience has no name on Roblox"),
            Self::NoIcon => f.write_str(
                "Roblox has no icon for this experience yet; try again in a few minutes",
            ),
            Self::IconNotPng => f.write_str("the experience's icon from Roblox is not a PNG image"),
            Self::IconTooLarge { width, height } => write!(
                f,
                "the experience's icon from Roblox is {width}x{height} pixels, larger than \
                 {LARGEST_ICON_SIDE}x{LARGEST_ICON_SIDE}"
            ),
        }
    }
}

impl std::error::Error for ExperienceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch { cause, .. } => Some(cause),
            Self::Answer { cause, .. } => Some(cause),
            Self::UnknownPlace(_)
            | Self::NoDetails
            | Self::NoName
            | Self::NoIcon
            | Self::IconNotPng
            | Self::IconTooLarge { .. } => None,
        }
    }
}

#[derive(Deserialize)]
struct UniverseAnswer {
    #[serde(rename = "universeId")]
    universe_id: Option<NonZeroU64>,
}

#[derive(Deserialize)]
struct Listing<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct Game {
    id: NonZeroU64,
    name: String,
}

#[derive(Deserialize)]
struct Thumbnail {
    #[serde(rename = "targetId")]
    target_id: NonZeroU64,
    #[serde(rename = "imageUrl")]
    image_url: Option<String>,
}

pub fn look_up(place: PlaceId) -> Result<Experience, ExperienceError> {
    let agent = https::api_agent();
    look_up_with(place, |lookup, url| {
        https::fetch_capped(&agent, url, lookup.hosts(), lookup.limit())
    })
}

fn look_up_with(
    place: PlaceId,
    mut fetch: impl FnMut(Lookup, &str) -> Result<Vec<u8>, DownloadError>,
) -> Result<Experience, ExperienceError> {
    let mut ask = |lookup: Lookup, url: &str| {
        fetch(lookup, url).map_err(|cause| ExperienceError::Fetch { lookup, cause })
    };
    let universe_url = format!("https://apis.roblox.com/universes/v1/places/{place}/universe");
    let universe =
        answer::<UniverseAnswer>(Lookup::Universe, &ask(Lookup::Universe, &universe_url)?)?
            .universe_id
            .ok_or(ExperienceError::UnknownPlace(place))?;

    let details_url = format!("https://games.roblox.com/v1/games?universeIds={universe}");
    let game = answer::<Listing<Game>>(Lookup::Details, &ask(Lookup::Details, &details_url)?)?
        .data
        .into_iter()
        .find(|game| game.id == universe)
        .ok_or(ExperienceError::NoDetails)?;
    let name = display_name(&game.name).ok_or(ExperienceError::NoName)?;

    let icon_url = format!(
        "https://thumbnails.roblox.com/v1/games/icons?universeIds={universe}\
         &size={ICON_REQUEST_SIZE}&format=Png&isCircular=false"
    );
    let image_url = answer::<Listing<Thumbnail>>(Lookup::Icon, &ask(Lookup::Icon, &icon_url)?)?
        .data
        .into_iter()
        .find(|icon| icon.target_id == universe)
        .and_then(|icon| icon.image_url)
        .filter(|url| !url.is_empty())
        .ok_or(ExperienceError::NoIcon)?;
    let icon = Png::parse(ask(Lookup::IconImage, &image_url)?)?;
    Ok(Experience { name, icon })
}

fn answer<T: DeserializeOwned>(lookup: Lookup, body: &[u8]) -> Result<T, ExperienceError> {
    serde_json::from_slice(body).map_err(|cause| ExperienceError::Answer { lookup, cause })
}

fn display_name(raw: &str) -> Option<String> {
    let printable: String = raw
        .chars()
        .filter_map(|c| match c {
            c if c.is_control() && c.is_whitespace() => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let cut: String = printable.trim().chars().take(NAME_LIMIT).collect();
    let name = cut.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::links::{parse, LaunchTarget};

    const UNIVERSE: &[u8] = include_bytes!("../tests/fixtures/roblox-api/universe.json");
    const GAMES: &[u8] = include_bytes!("../tests/fixtures/roblox-api/games.json");
    const ICONS: &[u8] = include_bytes!("../tests/fixtures/roblox-api/icons.json");
    const ICON: &[u8] = include_bytes!("../tests/fixtures/roblox-api/icon.png");
    const ICON_IMAGE_URL: &str =
        "https://tr.rbxcdn.com/180DAY-672284ae50f6bece92ce3c92c1ea6067/256/256/Image/Png/noFilter";

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(PNG_HEADER_CHUNK);
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn place(text: &str) -> PlaceId {
        match parse(text) {
            Ok(LaunchTarget::Place { place, .. }) => place,
            other => panic!("{text} is not a place: {other:?}"),
        }
    }

    struct Roblox {
        universe: Vec<u8>,
        games: Vec<u8>,
        icons: Vec<u8>,
        image: Vec<u8>,
        asked: Vec<(Lookup, String)>,
    }

    impl Roblox {
        fn recorded() -> Self {
            Self {
                universe: UNIVERSE.to_vec(),
                games: GAMES.to_vec(),
                icons: ICONS.to_vec(),
                image: ICON.to_vec(),
                asked: Vec::new(),
            }
        }

        fn look_up(&mut self, place: PlaceId) -> Result<Experience, ExperienceError> {
            look_up_with(place, |lookup, url| {
                self.asked.push((lookup, url.to_owned()));
                https::trusted_uri(url, lookup.hosts())?;
                Ok(match lookup {
                    Lookup::Universe => self.universe.clone(),
                    Lookup::Details => self.games.clone(),
                    Lookup::Icon => self.icons.clone(),
                    Lookup::IconImage => self.image.clone(),
                })
            })
        }
    }

    #[test]
    fn the_recorded_answers_give_the_name_and_the_icon() {
        let mut roblox = Roblox::recorded();

        let experience = roblox.look_up(place("1818")).expect("the lookup succeeds");

        assert_eq!(experience.name(), "Crossroads");
        assert_eq!(experience.icon().bytes(), ICON);
        assert_eq!(experience.icon().dimensions(), (256, 256));
        assert_eq!(
            roblox.asked,
            [
                (
                    Lookup::Universe,
                    "https://apis.roblox.com/universes/v1/places/1818/universe".to_owned()
                ),
                (
                    Lookup::Details,
                    "https://games.roblox.com/v1/games?universeIds=13058".to_owned()
                ),
                (
                    Lookup::Icon,
                    "https://thumbnails.roblox.com/v1/games/icons?universeIds=13058\
                     &size=256x256&format=Png&isCircular=false"
                        .to_owned()
                ),
                (Lookup::IconImage, ICON_IMAGE_URL.to_owned()),
            ]
        );
    }

    #[test]
    fn a_place_roblox_does_not_know_is_named_in_the_error() {
        let mut roblox = Roblox::recorded();
        roblox.universe = br#"{"universeId":null}"#.to_vec();

        let error = roblox
            .look_up(place("1"))
            .expect_err("no experience holds place 1");

        assert!(
            matches!(error, ExperienceError::UnknownPlace(_)),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "Roblox knows no experience with place 1");
        assert_eq!(roblox.asked.len(), 1);
    }

    #[test]
    fn icons_are_fetched_only_over_https_from_rbxcdn() {
        for url in [
            "http://tr.rbxcdn.com/x/256/256/Image/Png/noFilter",
            "https://evil.example/x.png",
            "https://rbxcdn.com.evil.example/x.png",
        ] {
            let mut roblox = Roblox::recorded();
            roblox.icons = format!(
                r#"{{"data":[{{"targetId":13058,"state":"Completed","imageUrl":"{url}"}}]}}"#
            )
            .into_bytes();

            let error = roblox.look_up(place("1818")).expect_err(url);

            assert!(
                matches!(
                    error,
                    ExperienceError::Fetch {
                        lookup: Lookup::IconImage,
                        cause: DownloadError::Untrusted { .. }
                    }
                ),
                "{url}: {error:?}"
            );
            assert_eq!(
                error.to_string(),
                "cannot ask *.rbxcdn.com for the icon image: the download link is not an HTTPS \
                 URL on *.rbxcdn.com"
            );
        }
    }

    #[test]
    fn an_icon_roblox_has_not_made_yet_is_reported() {
        for icons in [
            r#"{"data":[{"targetId":13058,"state":"Pending","imageUrl":null}]}"#,
            r#"{"data":[{"targetId":13058,"state":"Pending","imageUrl":""}]}"#,
            r#"{"data":[{"targetId":99,"state":"Completed","imageUrl":"https://tr.rbxcdn.com/x"}]}"#,
            r#"{"data":[]}"#,
        ] {
            let mut roblox = Roblox::recorded();
            roblox.icons = icons.as_bytes().to_vec();

            let error = roblox.look_up(place("1818")).expect_err(icons);

            assert!(
                matches!(error, ExperienceError::NoIcon),
                "{icons}: {error:?}"
            );
            assert_eq!(roblox.asked.len(), 3, "{icons}");
        }
    }

    #[test]
    fn unexpected_answers_name_the_host_and_the_subject() {
        let mut roblox = Roblox::recorded();
        roblox.games = b"<html>".to_vec();

        let error = roblox
            .look_up(place("1818"))
            .expect_err("the answer is not JSON");

        assert!(
            error.to_string().starts_with(
                "games.roblox.com sent an unexpected answer about the experience's name: "
            ),
            "{error}"
        );
        let mut roblox = Roblox::recorded();
        roblox.games = br#"{"data":[]}"#.to_vec();
        assert!(matches!(
            roblox.look_up(place("1818")),
            Err(ExperienceError::NoDetails)
        ));
    }

    #[test]
    fn names_lose_control_characters_and_are_cut_to_128_characters() {
        assert_eq!(
            display_name("  [UPD]\u{7}\nBlox\tFruits  ").as_deref(),
            Some("[UPD] Blox Fruits")
        );
        let long = format!("{} tail", "é".repeat(127));
        assert_eq!(display_name(&long), Some("é".repeat(127)));
        assert_eq!(display_name(" \n\t"), None);
    }

    #[test]
    fn icons_must_be_png_images_of_at_most_512_pixels_a_side() {
        assert_eq!(Png::parse(png(512, 512)).unwrap().dimensions(), (512, 512));
        assert_eq!(Png::parse(ICON.to_vec()).unwrap().dimensions(), (256, 256));
        for (bytes, refused) in [
            (b"GIF89a....................".to_vec(), "is not a PNG image"),
            (ICON[..23].to_vec(), "is not a PNG image"),
            (png(0, 256), "is not a PNG image"),
            (png(513, 256), "is 513x256 pixels, larger than 512x512"),
            (png(256, 1024), "is 256x1024 pixels, larger than 512x512"),
        ] {
            let error = Png::parse(bytes).expect_err(refused);
            assert!(error.to_string().ends_with(refused), "{error}");
        }
    }
}
