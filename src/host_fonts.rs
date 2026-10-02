use crate::font::{GlyphSource, RasterFont};
use fontconfig_sys as fc;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const MAX_FALLBACK_FACES: usize = 16;
const MAX_FALLBACK_SOURCES: usize = 256;
const MAX_FALLBACK_DECISIONS: usize = 4096;
const INK_PROBE_PPEM: f32 = 16.0;
const COLLECTION_INDEX_MASK: u32 = 0xFFFF;
const HANGUL_FILLERS: [char; 4] = ['\u{115F}', '\u{1160}', '\u{3164}', '\u{FFA0}'];
const FONT_DIRS: [&str; 4] = [
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/usr/share/fonts/truetype",
    "/run/host/fonts",
];

type FallbackDecisions = HashMap<(char, Presentation), Option<&'static RasterFont>>;
type FallbackFaces = HashMap<FontSource, Option<&'static RasterFont>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Presentation {
    Text,
    Emoji,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FontSource {
    pub(crate) path: PathBuf,
    pub(crate) index: u32,
}

impl FontSource {
    fn file(path: PathBuf) -> Self {
        Self { path, index: 0 }
    }

    fn load(&self) -> Result<RasterFont, String> {
        let data = std::fs::read(&self.path).map_err(|error| error.to_string())?;
        RasterFont::open(data.into(), self.index)
    }
}

pub(crate) fn system_font() -> Option<&'static RasterFont> {
    static FONT: OnceLock<Option<RasterFont>> = OnceLock::new();
    FONT.get_or_init(|| {
        let chosen = load_first_font(system_font_candidates());
        match &chosen {
            Some((source, _)) => tracing::info!(
                path = %source.path.display(),
                index = source.index,
                "text: using the host sans-serif font"
            ),
            None => tracing::warn!(
                "text: no usable host font (ECLIPSE_FONT, fontconfig sans-serif, font directories); \
                 host-rendered text is disabled"
            ),
        }
        chosen.map(|(_, font)| font)
    })
    .as_ref()
}

fn system_font_candidates() -> impl Iterator<Item = FontSource> {
    let configured = std::env::var_os("ECLIPSE_FONT").and_then(|value| {
        let path = PathBuf::from(value);
        if path.is_file() {
            Some(FontSource::file(path))
        } else {
            tracing::warn!(path = %path.display(), "ECLIPSE_FONT is not a file; ignoring it");
            None
        }
    });
    let matched = sorted_fonts(c"sans-serif", None, Presentation::Text);
    let walked = FONT_DIRS
        .into_iter()
        .flat_map(|dir| font_files_in(Path::new(dir)))
        .map(FontSource::file);
    configured.into_iter().chain(matched).chain(walked)
}

pub(crate) fn load_first_font(
    candidates: impl IntoIterator<Item = FontSource>,
) -> Option<(FontSource, RasterFont)> {
    candidates.into_iter().find_map(|source| {
        let loaded = source.load().and_then(|font| match font.glyph_source() {
            GlyphSource::Outlines => Ok(font),
            GlyphSource::ColorStrikes => Err("font has no scalable outline".to_string()),
        });
        match loaded {
            Ok(font) => Some((source, font)),
            Err(error) => {
                tracing::warn!(
                    path = %source.path.display(),
                    index = source.index,
                    %error,
                    "text: font rejected; trying the next candidate"
                );
                None
            }
        }
    })
}

fn font_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.extension().is_some_and(|ext| {
            let ext = ext.to_ascii_lowercase();
            ext == "ttf" || ext == "otf"
        }) {
            files.push(path);
        }
    }
    files.extend(subdirs.iter().flat_map(|subdir| font_files_in(subdir)));
    files
}

pub(crate) fn fallback_for(
    character: char,
    presentation: Presentation,
) -> Option<&'static RasterFont> {
    static DECISIONS: Mutex<Option<FallbackDecisions>> = Mutex::new(None);
    let decisions = || {
        DECISIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    if let Some(decision) = decisions()
        .as_ref()
        .and_then(|known| known.get(&(character, presentation)).copied())
    {
        return decision;
    }
    static FACES: Mutex<Option<FallbackFaces>> = Mutex::new(None);
    let sources = sorted_fonts(family_for(presentation), Some(character), presentation);
    let decision = first_drawing_face(&FACES, sources, character, presentation);
    let mut known = decisions();
    let known = known.get_or_insert_with(HashMap::new);
    if known.len() >= MAX_FALLBACK_DECISIONS {
        known.clear();
    }
    known.insert((character, presentation), decision);
    decision
}

fn family_for(presentation: Presentation) -> &'static CStr {
    match presentation {
        Presentation::Text => c"sans-serif",
        Presentation::Emoji => c"emoji",
    }
}

fn first_drawing_face(
    faces: &Mutex<Option<FallbackFaces>>,
    sources: Vec<FontSource>,
    character: char,
    presentation: Presentation,
) -> Option<&'static RasterFont> {
    sources.into_iter().find_map(|source| {
        let mut faces = faces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drawing_face(
            faces.get_or_insert_with(HashMap::new),
            source,
            character,
            presentation,
        )
    })
}

fn drawing_face(
    faces: &mut FallbackFaces,
    source: FontSource,
    character: char,
    presentation: Presentation,
) -> Option<&'static RasterFont> {
    if let Some(known) = faces.get(&source) {
        return known.filter(|font| draws(font, character, presentation));
    }
    let kept = faces.values().filter(|face| face.is_some()).count();
    if kept >= MAX_FALLBACK_FACES || faces.len() >= MAX_FALLBACK_SOURCES {
        return None;
    }
    let font = match source.load() {
        Ok(font) => font,
        Err(error) => {
            tracing::warn!(
                path = %source.path.display(),
                index = source.index,
                %error,
                "text: fallback font rejected"
            );
            faces.insert(source, None);
            return None;
        }
    };
    if !draws(&font, character, presentation) {
        return None;
    }
    let font: &'static RasterFont = Box::leak(Box::new(font));
    faces.insert(source, Some(font));
    Some(font)
}

fn draws(font: &RasterFont, character: char, presentation: Presentation) -> bool {
    let Some(glyph) = font.glyph_index(character) else {
        return false;
    };
    if presentation == Presentation::Emoji && !font.has_color() {
        return false;
    }
    if character.is_whitespace() || HANGUL_FILLERS.contains(&character) {
        return true;
    }
    font.at_em(INK_PROBE_PPEM)
        .and_then(|mut scaled| scaled.render(glyph, 0.0, INK_PROBE_PPEM))
        .is_some_and(|image| match image.pixels {
            crate::font::GlyphPixels::Coverage(values) => values.iter().any(|&value| value != 0),
            crate::font::GlyphPixels::Color(values) => values.iter().any(|value| value[3] != 0),
        })
}

struct FontConfig(*mut fc::FcConfig);

unsafe impl Send for FontConfig {}

fn font_config() -> Option<&'static Mutex<FontConfig>> {
    static CONFIG: OnceLock<Option<Mutex<FontConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let config = unsafe { fc::FcInitLoadConfigAndFonts() };
            if config.is_null() {
                tracing::warn!("text: fontconfig could not load its configuration");
                return None;
            }
            Some(Mutex::new(FontConfig(config)))
        })
        .as_ref()
}

fn sorted_fonts(
    family: &CStr,
    character: Option<char>,
    presentation: Presentation,
) -> Vec<FontSource> {
    let Some(config) = font_config() else {
        return Vec::new();
    };
    let config = config
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    unsafe { query_sorted_fonts(config.0, family, character, presentation) }
}

unsafe fn query_sorted_fonts(
    config: *mut fc::FcConfig,
    family: &CStr,
    character: Option<char>,
    presentation: Presentation,
) -> Vec<FontSource> {
    let pattern = unsafe { fc::FcPatternCreate() };
    if pattern.is_null() {
        return Vec::new();
    }
    let mut charset = std::ptr::null_mut();
    unsafe {
        fc::FcPatternAddString(
            pattern,
            fc::constants::FC_FAMILY.as_ptr(),
            family.as_ptr().cast(),
        );
        if presentation == Presentation::Emoji {
            fc::FcPatternAddBool(pattern, fc::constants::FC_COLOR.as_ptr(), 1);
        }
        if let Some(character) = character {
            charset = fc::FcCharSetCreate();
            if !charset.is_null() {
                fc::FcCharSetAddChar(charset, u32::from(character));
                fc::FcPatternAddCharSet(pattern, fc::constants::FC_CHARSET.as_ptr(), charset);
            }
        }
        fc::FcConfigSubstitute(config, pattern, fc::FcMatchPattern);
        fc::FcDefaultSubstitute(pattern);
    }
    let mut result = fc::FcResultNoMatch;
    let set = unsafe { fc::FcFontSort(config, pattern, 0, std::ptr::null_mut(), &mut result) };
    let mut sources = Vec::new();
    if !set.is_null() {
        let fonts = unsafe { &*set };
        let count = usize::try_from(fonts.nfont).unwrap_or(0);
        let patterns = if fonts.fonts.is_null() || count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(fonts.fonts, count) }
        };
        for &font in patterns {
            if let Some(source) = unsafe { font_source(font, character) } {
                sources.push(source);
            }
        }
        unsafe { fc::FcFontSetDestroy(set) };
    }
    unsafe {
        if !charset.is_null() {
            fc::FcCharSetDestroy(charset);
        }
        fc::FcPatternDestroy(pattern);
    }
    sources
}

unsafe fn font_source(font: *mut fc::FcPattern, character: Option<char>) -> Option<FontSource> {
    if let Some(character) = character {
        let mut charset = std::ptr::null_mut();
        let found = unsafe {
            fc::FcPatternGetCharSet(font, fc::constants::FC_CHARSET.as_ptr(), 0, &mut charset)
        };
        if found != fc::FcResultMatch
            || charset.is_null()
            || unsafe { fc::FcCharSetHasChar(charset, u32::from(character)) } == 0
        {
            return None;
        }
    }
    let mut file = std::ptr::null_mut();
    if unsafe { fc::FcPatternGetString(font, fc::constants::FC_FILE.as_ptr(), 0, &mut file) }
        != fc::FcResultMatch
        || file.is_null()
    {
        return None;
    }
    let path = unsafe { CStr::from_ptr(file.cast()) };
    let mut index = 0;
    if unsafe { fc::FcPatternGetInteger(font, fc::constants::FC_INDEX.as_ptr(), 0, &mut index) }
        != fc::FcResultMatch
    {
        index = 0;
    }
    Some(FontSource {
        path: PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())),
        index: u32::try_from(index).ok()? & COLLECTION_INDEX_MASK,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_that_does_not_load_is_skipped_for_the_next_one() {
        let Some(real) = sorted_fonts(c"sans-serif", None, Presentation::Text)
            .into_iter()
            .find(|source| source.load().is_ok())
        else {
            eprintln!("SKIP: fontconfig lists no loadable sans-serif font");
            return;
        };
        let bogus =
            std::env::temp_dir().join(format!("eclipse-not-a-font-{}.ttf", std::process::id()));
        std::fs::write(&bogus, b"not a font").expect("write the bogus font");
        let missing = FontSource::file(bogus.with_extension("missing.ttf"));
        let chosen = load_first_font([FontSource::file(bogus.clone()), missing, real.clone()]);
        std::fs::remove_file(&bogus).expect("remove the bogus font");
        assert_eq!(chosen.map(|(source, _)| source), Some(real));
    }

    #[test]
    fn faces_that_draw_nothing_leave_the_fallback_budget_for_real_scripts() {
        let pick = |faces: &Mutex<Option<FallbackFaces>>, character: char| {
            let sources = sorted_fonts(c"sans-serif", Some(character), Presentation::Text);
            first_drawing_face(faces, sources, character, Presentation::Text)
        };
        let faces = Mutex::new(None);
        for invisible in [
            '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{061C}', '\u{FFF9}', '\u{FFFA}', '\u{FFFB}',
        ] {
            pick(&faces, invisible);
        }
        let cjk = pick(&faces, '\u{4F60}');
        if cjk.is_none() && pick(&Mutex::new(None), '\u{4F60}').is_none() {
            eprintln!("SKIP: the host has no font that draws CJK");
            return;
        }
        assert!(
            cjk.is_some(),
            "CJK still falls back after invisible characters"
        );
    }

    #[test]
    fn hangul_fillers_come_from_a_font_that_maps_them_although_they_are_blank() {
        let filler = '\u{3164}';
        let sources = sorted_fonts(c"sans-serif", Some(filler), Presentation::Text);
        if sources.is_empty() {
            eprintln!("SKIP: no host font maps the Hangul filler");
            return;
        }
        let face = first_drawing_face(&Mutex::new(None), sources, filler, Presentation::Text);
        assert!(face.is_some());
    }
}
