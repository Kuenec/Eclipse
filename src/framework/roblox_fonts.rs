use crate::font::RasterFont;
use crate::text_layout::FaceChain;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

const FONT_MAPPINGS_ASSET: &str = "android/fonts/font-mappings.json";
const MAPPED_FONT_DIR: &str = "content/fonts/";
const UNMAPPED_FONT_DIR: &str = "fonts/";
const UNMAPPED_EM_PER_SIZE: f32 = 0.795;
const UNMAPPED_BOLD_LETTER_SPACING_EM: f32 = 0.04;
const SOURCE_SANS_BOLD: i32 = 4;
const SOURCE_SANS_LIGHT: i32 = 5;
const SCRIPT_FALLBACK_ASSETS: [&str; 9] = [
    "content/fonts/NotoNaskhArabicUI-Regular.ttf",
    "content/fonts/NotoSansThaiUI-Regular.ttf",
    "content/fonts/NotoSansDevanagariUI-Regular.ttf",
    "content/fonts/NotoSansBengaliUI-Regular.ttf",
    "content/fonts/NotoSansGeorgian-Regular.ttf",
    "content/fonts/NotoSansKhmerUI-Regular.ttf",
    "content/fonts/NotoSansMyanmarUI-Regular.ttf",
    "content/fonts/NotoSansSinhalaUI-Regular.ttf",
    "content/fonts/TwemojiMozilla.ttf",
];

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
struct FontMapping {
    #[serde(rename = "enum")]
    id: i32,
    font: String,
    #[serde(rename = "fromRbxFontRatio")]
    em_per_size: f32,
}

#[derive(Debug, Clone, PartialEq)]
struct FaceSpec {
    asset: String,
    em_per_size: f32,
    letter_spacing_em: f32,
}

#[derive(Clone, Copy)]
struct RobloxTextFace {
    font: &'static RasterFont,
    em_per_size: f32,
    letter_spacing_em: f32,
}

fn text_face(
    font_enum: i32,
    mappings: &[FontMapping],
    load: impl Fn(&str) -> Option<&'static RasterFont>,
) -> Option<RobloxTextFace> {
    let face = |spec: FaceSpec| {
        load(&spec.asset).map(|font| RobloxTextFace {
            font,
            em_per_size: spec.em_per_size,
            letter_spacing_em: spec.letter_spacing_em,
        })
    };
    mapped_spec(font_enum, mappings)
        .and_then(face)
        .or_else(|| face(unmapped_spec(font_enum)))
}

pub(crate) fn face_chain(font_enum: i32) -> Option<FaceChain<'static>> {
    let fallbacks = script_fallbacks();
    if let Some(face) = text_face(font_enum, font_mappings(), asset_font) {
        return Some(FaceChain {
            primary: face.font,
            em_per_size: face.em_per_size,
            letter_spacing_em: face.letter_spacing_em,
            fallbacks,
        });
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            font = font_enum,
            "text: the Roblox font for a focused TextBox is unavailable; using the host font"
        );
    }
    let primary = crate::host_fonts::system_font()?;
    Some(FaceChain {
        primary,
        em_per_size: primary.em_per_height(),
        letter_spacing_em: 0.0,
        fallbacks,
    })
}

fn script_fallbacks() -> &'static [&'static RasterFont] {
    static FALLBACKS: OnceLock<Vec<&'static RasterFont>> = OnceLock::new();
    FALLBACKS.get_or_init(|| {
        SCRIPT_FALLBACK_ASSETS
            .iter()
            .filter_map(|asset| asset_font(asset))
            .collect()
    })
}

fn font_mappings() -> &'static [FontMapping] {
    static MAPPINGS: OnceLock<Vec<FontMapping>> = OnceLock::new();
    MAPPINGS.get_or_init(|| {
        let Some(bytes) = super::read_asset_bytes(FONT_MAPPINGS_ASSET) else {
            tracing::warn!(
                asset = FONT_MAPPINGS_ASSET,
                "text: Roblox font mappings are missing; every TextBox uses Source Sans Pro"
            );
            return Vec::new();
        };
        parse_font_mappings(&bytes).unwrap_or_else(|error| {
            tracing::warn!(
                asset = FONT_MAPPINGS_ASSET,
                %error,
                "text: Roblox font mappings are unreadable; every TextBox uses Source Sans Pro"
            );
            Vec::new()
        })
    })
}

fn parse_font_mappings(bytes: &[u8]) -> Result<Vec<FontMapping>, serde_json::Error> {
    serde_json::from_slice(bytes)
}

fn mapped_spec(font_enum: i32, mappings: &[FontMapping]) -> Option<FaceSpec> {
    mappings
        .iter()
        .find(|mapping| mapping.id == font_enum)
        .map(|mapping| FaceSpec {
            asset: format!("{MAPPED_FONT_DIR}{}", mapping.font),
            em_per_size: mapping.em_per_size,
            letter_spacing_em: 0.0,
        })
}

fn unmapped_spec(font_enum: i32) -> FaceSpec {
    let (file, letter_spacing_em) = match font_enum {
        SOURCE_SANS_BOLD => ("SourceSansPro-Bold.ttf", UNMAPPED_BOLD_LETTER_SPACING_EM),
        SOURCE_SANS_LIGHT => ("SourceSansPro-Light.ttf", 0.0),
        _ => ("SourceSansPro-Regular.ttf", 0.0),
    };
    FaceSpec {
        asset: format!("{UNMAPPED_FONT_DIR}{file}"),
        em_per_size: UNMAPPED_EM_PER_SIZE,
        letter_spacing_em,
    }
}

fn asset_font(asset: &str) -> Option<&'static RasterFont> {
    static FONTS: Mutex<Option<HashMap<String, Option<&'static RasterFont>>>> = Mutex::new(None);
    let mut fonts = FONTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fonts = fonts.get_or_insert_with(HashMap::new);
    if let Some(font) = fonts.get(asset) {
        return *font;
    }
    let loaded = super::read_asset_bytes(asset)
        .ok_or_else(|| "asset is missing from the APK".to_string())
        .and_then(|bytes| RasterFont::open(bytes.into(), 0));
    let font = match loaded {
        Ok(font) => Some(&*Box::leak(Box::new(font))),
        Err(error) => {
            tracing::warn!(asset, %error, "text: Roblox font could not be loaded");
            None
        }
    };
    fonts.insert(asset.to_owned(), font);
    font
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPPINGS: &str = r#"[
        { "enum" : 6, "font" : "SourceSansPro-It.ttf", "fromRbxFontRatio" : 0.8951044945 },
        { "enum" : 10, "font" : "Inconsolata-Regular.ttf", "fromRbxFontRatio" : 0.9532888465 },
        { "enum" : 46, "font" : "BuilderSans-Regular.otf", "fromRbxFontRatio" : 0.7936507937 }
    ]"#;

    #[test]
    fn text_boxes_use_the_face_and_size_ratio_roblox_keyboard_uses() {
        let mappings = parse_font_mappings(MAPPINGS.as_bytes()).expect("mappings parse");
        assert_eq!(
            mapped_spec(46, &mappings),
            Some(FaceSpec {
                asset: "content/fonts/BuilderSans-Regular.otf".to_owned(),
                em_per_size: 0.793_650_8,
                letter_spacing_em: 0.0,
            })
        );
        assert_eq!(
            mapped_spec(6, &mappings).map(|spec| spec.em_per_size),
            Some(0.895_104_47)
        );
        assert_eq!(mapped_spec(4, &mappings), None);
        assert_eq!(
            unmapped_spec(4),
            FaceSpec {
                asset: "fonts/SourceSansPro-Bold.ttf".to_owned(),
                em_per_size: 0.795,
                letter_spacing_em: 0.04,
            }
        );
        assert_eq!(unmapped_spec(5).asset, "fonts/SourceSansPro-Light.ttf");
        assert_eq!(unmapped_spec(100).asset, "fonts/SourceSansPro-Regular.ttf");
        assert_eq!(unmapped_spec(100).letter_spacing_em, 0.0);
    }

    #[test]
    fn text_faces_load_from_the_apk_and_fall_back_to_source_sans() {
        use std::io::Write;
        let Some(host) = crate::host_fonts::system_font() else {
            eprintln!("SKIP: no host font to stand in for the Roblox fonts");
            return;
        };
        let mut zip_bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            let options = zip::write::SimpleFileOptions::default();
            for (name, bytes) in [
                (
                    "assets/android/fonts/font-mappings.json",
                    MAPPINGS.as_bytes(),
                ),
                ("assets/content/fonts/BuilderSans-Regular.otf", host.data()),
                ("assets/fonts/SourceSansPro-Regular.ttf", host.data()),
            ] {
                writer.start_file(name, options).expect("start entry");
                writer.write_all(bytes).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        let path = std::env::temp_dir().join(format!(
            "eclipse-roblox-fonts-test-{}.apk",
            std::process::id()
        ));
        std::fs::write(&path, &zip_bytes).expect("write fixture apk");
        let apk = path.to_str().expect("utf-8 temp path");
        let read = |asset: &str| super::super::read_asset_bytes_from(apk, asset);
        let mappings = parse_font_mappings(&read(FONT_MAPPINGS_ASSET).expect("mappings asset"))
            .expect("mappings parse");
        let loaded = std::cell::RefCell::new(Vec::new());
        let load = |asset: &str| -> Option<&'static RasterFont> {
            let bytes = read(asset)?;
            loaded.borrow_mut().push(asset.to_owned());
            let font = RasterFont::open(bytes.into(), 0).expect("fixture font opens");
            Some(Box::leak(Box::new(font)))
        };

        let builder = text_face(46, &mappings, load).expect("mapped face");
        assert_eq!(builder.font.data(), host.data());
        assert_eq!(builder.em_per_size, 0.793_650_8);
        assert_eq!(
            loaded.take(),
            ["content/fonts/BuilderSans-Regular.otf".to_owned()]
        );

        let italic = text_face(6, &mappings, load).expect("face for a mapped font the APK lacks");
        assert_eq!(italic.em_per_size, UNMAPPED_EM_PER_SIZE);
        assert_eq!(
            loaded.take(),
            ["fonts/SourceSansPro-Regular.ttf".to_owned()]
        );

        assert!(text_face(SOURCE_SANS_BOLD, &mappings, load).is_none());
        std::fs::remove_file(&path).expect("remove fixture apk");
    }
}
