use crate::font::{GlyphPixels, GlyphSource, RasterFont, ScaledFont};
use crate::host_fonts::{self, Presentation};
use rustybuzz::{Direction, UnicodeBuffer};
use std::collections::HashMap;
use std::ops::Range;
use unicode_bidi::{BidiInfo, Level};
use unicode_properties::emoji::{EmojiStatus, UnicodeEmoji};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const CARET_WIDTH: f32 = 2.0;
const TAB_STOP: f32 = 20.0;
const DEFAULT_FONT_SIZE: f32 = 14.0;
const MAX_FONT_SIZE: f32 = 100.0;
const HIDDEN_CONTROL: char = '\u{200B}';
const INK_MARGIN: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HorizontalAlignment {
    Left,
    Right,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerticalAlignment {
    Top,
    Center,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineMode {
    Single,
    Wrapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    Caret(usize),
    All,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FieldStyle {
    pub(crate) font_size: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) lines: LineMode,
    pub(crate) horizontal: HorizontalAlignment,
    pub(crate) vertical: VerticalAlignment,
}

impl FieldStyle {
    pub(crate) fn of_text_box(
        font_size: f32,
        (width, height): (u32, u32),
        multiline: bool,
        text_wrapped: bool,
        x_alignment: i32,
        y_alignment: i32,
    ) -> Self {
        Self {
            font_size: if font_size.is_finite() && font_size > 0.0 {
                font_size.min(MAX_FONT_SIZE)
            } else {
                DEFAULT_FONT_SIZE
            },
            width: width as f32,
            height: height as f32,
            lines: if multiline || text_wrapped {
                LineMode::Wrapped
            } else {
                LineMode::Single
            },
            horizontal: match x_alignment {
                1 => HorizontalAlignment::Right,
                2 => HorizontalAlignment::Center,
                _ => HorizontalAlignment::Left,
            },
            vertical: match y_alignment {
                1 => VerticalAlignment::Center,
                2 => VerticalAlignment::Bottom,
                _ => VerticalAlignment::Top,
            },
        }
    }
}

pub(crate) fn char_index_at_utf16(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, character) in text.chars().enumerate() {
        if units >= utf16 {
            return index;
        }
        units += character.len_utf16();
    }
    text.chars().count()
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Scroll {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

#[derive(Clone, Copy)]
pub(crate) struct FaceChain<'a> {
    pub(crate) primary: &'a RasterFont,
    pub(crate) em_per_size: f32,
    pub(crate) letter_spacing_em: f32,
    pub(crate) fallbacks: &'a [&'a RasterFont],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FieldRect {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) right: f32,
    pub(crate) bottom: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x0: u32,
    pub(crate) y0: u32,
    pub(crate) x1: u32,
    pub(crate) y1: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Viewport {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug)]
struct PlacedGlyph {
    face: usize,
    glyph: u32,
    line: usize,
    x: f32,
    rise: f32,
}

#[derive(Clone, Debug)]
struct ClusterBox {
    chars: Range<usize>,
    left: f32,
    right: f32,
    rtl: bool,
}

#[derive(Clone, Debug)]
struct LaidLine {
    chars: Range<usize>,
    visible_end: usize,
    ends_paragraph: bool,
    rtl: bool,
    top: f32,
    left: f32,
    width: f32,
    boxes: Vec<ClusterBox>,
}

pub(crate) struct FieldLayout<'a> {
    faces: Vec<&'a RasterFont>,
    ppem: f32,
    ascent: f32,
    line_height: f32,
    glyphs: Vec<PlacedGlyph>,
    lines: Vec<LaidLine>,
    caret: Option<FieldRect>,
    highlights: Vec<FieldRect>,
    scroll: Scroll,
}

struct Cluster {
    chars: Range<usize>,
    bytes: Range<usize>,
    face: usize,
    level: Level,
    advance: f32,
    breakable_space: bool,
    tab: bool,
}

struct LineSpan {
    clusters: Range<usize>,
    visible_clusters: Range<usize>,
}

struct Paragraph<'t> {
    text: &'t str,
    bidi: BidiInfo<'t>,
    clusters: Vec<Cluster>,
}

struct FaceSet<'a> {
    chain: FaceChain<'a>,
    faces: Vec<&'a RasterFont>,
    shapers: Vec<Option<rustybuzz::Face<'a>>>,
    choices: HashMap<String, usize>,
}

impl<'a> FaceSet<'a> {
    fn new(chain: FaceChain<'a>) -> Self {
        let mut set = Self {
            chain,
            faces: Vec::new(),
            shapers: Vec::new(),
            choices: HashMap::new(),
        };
        set.index_of(chain.primary);
        set
    }

    fn index_of(&mut self, font: &'a RasterFont) -> usize {
        if let Some(index) = self
            .faces
            .iter()
            .position(|known| std::ptr::eq(*known, font))
        {
            return index;
        }
        self.faces.push(font);
        self.shapers
            .push(rustybuzz::Face::from_slice(font.data(), font.index()));
        self.faces.len() - 1
    }

    fn choose(&mut self, cluster: &str) -> usize {
        if let Some(&index) = self.choices.get(cluster) {
            return index;
        }
        let chosen = match presentation_of(cluster) {
            Presentation::Emoji => self
                .bundled_face(cluster, Presentation::Emoji)
                .or_else(|| host_face(cluster, Presentation::Emoji))
                .or_else(|| self.bundled_face(cluster, Presentation::Text))
                .or_else(|| host_face(cluster, Presentation::Text)),
            Presentation::Text => self
                .bundled_face(cluster, Presentation::Text)
                .or_else(|| host_face(cluster, Presentation::Text)),
        };
        let index = chosen.map_or(0, |font| self.index_of(font));
        self.choices.insert(cluster.to_owned(), index);
        index
    }

    fn bundled_face(&self, cluster: &str, presentation: Presentation) -> Option<&'a RasterFont> {
        let chain = self.chain;
        std::iter::once(chain.primary)
            .chain(chain.fallbacks.iter().copied())
            .find(|font| {
                (presentation == Presentation::Text || font.has_color()) && covers(font, cluster)
            })
    }
}

fn host_face(cluster: &str, presentation: Presentation) -> Option<&'static RasterFont> {
    let first = cluster
        .chars()
        .find(|&character| !is_ignorable(character))?;
    host_fonts::fallback_for(first, presentation)
}

fn covers(font: &RasterFont, cluster: &str) -> bool {
    let Ok(face) = rustybuzz::ttf_parser::Face::parse(font.data(), font.index()) else {
        return false;
    };
    cluster
        .chars()
        .filter(|&character| !is_ignorable(character))
        .all(|character| face.glyph_index(shaping_char(character)).is_some())
}

fn presentation_of(cluster: &str) -> Presentation {
    let Some(first) = cluster.chars().next() else {
        return Presentation::Text;
    };
    if cluster.contains('\u{FE0E}') {
        return Presentation::Text;
    }
    let emoji_default = matches!(
        first.emoji_status(),
        EmojiStatus::EmojiPresentation
            | EmojiStatus::EmojiPresentationAndModifierBase
            | EmojiStatus::EmojiPresentationAndEmojiComponent
            | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
    );
    if emoji_default || cluster.contains('\u{FE0F}') {
        Presentation::Emoji
    } else {
        Presentation::Text
    }
}

fn is_ignorable(character: char) -> bool {
    shaping_char(character) == HIDDEN_CONTROL
        || matches!(
            character,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFF0}'..='\u{FFF8}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

fn shaping_char(character: char) -> char {
    match character {
        '\t' => ' ',
        '\u{2028}' | '\u{2029}' | '\u{FFF9}'..='\u{FFFB}' | '\u{1BCA0}'..='\u{1BCA3}' => {
            HIDDEN_CONTROL
        }
        character if character.is_control() => HIDDEN_CONTROL,
        character => character,
    }
}

fn breakable_space(character: char) -> bool {
    character.is_whitespace() && !matches!(character, '\u{A0}' | '\u{2007}' | '\u{202F}')
}

fn next_tab_stop(x: f32) -> f32 {
    ((x + TAB_STOP) / TAB_STOP).floor() * TAB_STOP
}

fn shape(
    face: &rustybuzz::Face<'_>,
    characters: impl Iterator<Item = (usize, char)>,
    rtl: bool,
) -> rustybuzz::GlyphBuffer {
    let mut buffer = UnicodeBuffer::new();
    for (index, character) in characters {
        buffer.add(shaping_char(character), index as u32);
    }
    buffer.set_direction(if rtl {
        Direction::RightToLeft
    } else {
        Direction::LeftToRight
    });
    rustybuzz::shape(face, &[], buffer)
}

fn break_lines(clusters: &mut [Cluster], max_width: f32) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut start = 0;
    while start < clusters.len() {
        let mut x = 0.0f32;
        let mut last_break = None;
        let mut end = clusters.len();
        let mut index = start;
        while index < clusters.len() {
            let cluster = &mut clusters[index];
            if cluster.tab {
                cluster.advance = next_tab_stop(x) - x;
            }
            if cluster.breakable_space {
                x += cluster.advance;
                index += 1;
                last_break = Some(index);
                continue;
            }
            if index > start && x + cluster.advance > max_width {
                end = match last_break {
                    Some(point) if point > start => point,
                    _ => index,
                };
                break;
            }
            x += cluster.advance;
            index += 1;
        }
        let mut visible_end = end;
        if end < clusters.len() {
            while visible_end > start && clusters[visible_end - 1].breakable_space {
                visible_end -= 1;
            }
        }
        spans.push(LineSpan {
            clusters: start..end,
            visible_clusters: start..visible_end,
        });
        start = end;
    }
    if spans.is_empty() {
        spans.push(LineSpan {
            clusters: 0..0,
            visible_clusters: 0..0,
        });
    }
    spans
}

struct Typesetter<'a> {
    faces: FaceSet<'a>,
    ppem: f32,
    letter_spacing: f32,
    max_width: f32,
    lines: Vec<LaidLine>,
    glyphs: Vec<PlacedGlyph>,
}

impl<'a> Typesetter<'a> {
    fn paragraph(&mut self, text: &str, first_char: usize) {
        let mut paragraph = self.segment(text, first_char);
        self.measure(&mut paragraph);
        let spans = break_lines(&mut paragraph.clusters, self.max_width);
        let rtl = paragraph
            .bidi
            .paragraphs
            .first()
            .is_some_and(|info| info.level.is_rtl());
        let span_count = spans.len();
        for (index, span) in spans.into_iter().enumerate() {
            let clusters = &paragraph.clusters;
            let start = clusters
                .get(span.clusters.start)
                .map_or(first_char, |cluster| cluster.chars.start);
            let end = span
                .clusters
                .end
                .checked_sub(1)
                .and_then(|last| clusters.get(last))
                .map_or(start, |cluster| cluster.chars.end);
            let visible_end = span
                .visible_clusters
                .end
                .checked_sub(1)
                .and_then(|last| clusters.get(last))
                .map_or(start, |cluster| cluster.chars.end);
            let mut line = LaidLine {
                chars: start..end,
                visible_end,
                ends_paragraph: index + 1 == span_count,
                rtl,
                top: 0.0,
                left: 0.0,
                width: 0.0,
                boxes: Vec::new(),
            };
            self.place_line(&paragraph, span.visible_clusters, &mut line);
            self.lines.push(line);
        }
    }

    fn segment<'t>(&mut self, text: &'t str, first_char: usize) -> Paragraph<'t> {
        let bidi = BidiInfo::new(text, None);
        let char_starts: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
        let char_at = |byte: usize| first_char + char_starts.partition_point(|&start| start < byte);
        let clusters = text
            .grapheme_indices(true)
            .map(|(byte, grapheme)| Cluster {
                chars: char_at(byte)..char_at(byte + grapheme.len()),
                bytes: byte..byte + grapheme.len(),
                face: self.faces.choose(grapheme),
                level: bidi.levels.get(byte).copied().unwrap_or_else(Level::ltr),
                advance: 0.0,
                breakable_space: grapheme.chars().all(breakable_space),
                tab: grapheme.starts_with('\t'),
            })
            .collect();
        Paragraph {
            text,
            bidi,
            clusters,
        }
    }

    fn measure(&self, paragraph: &mut Paragraph<'_>) {
        let mut start = 0;
        while start < paragraph.clusters.len() {
            let (face, level) = (
                paragraph.clusters[start].face,
                paragraph.clusters[start].level,
            );
            let end = start
                + paragraph.clusters[start..]
                    .iter()
                    .take_while(|cluster| cluster.face == face && cluster.level == level)
                    .count();
            let run = &mut paragraph.clusters[start..end];
            if let Some(shaper) = self.faces.shapers[face].as_ref() {
                let scale = self.ppem / self.faces.faces[face].units_per_em();
                let bytes = run[0].bytes.start..run[run.len() - 1].bytes.end;
                let first = run[0].chars.start;
                let characters = paragraph.text[bytes]
                    .chars()
                    .enumerate()
                    .map(|(offset, character)| (first + offset, character));
                let shaped = shape(shaper, characters, level.is_rtl());
                for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                    let index =
                        run.partition_point(|cluster| cluster.chars.end <= info.cluster as usize);
                    if let Some(cluster) = run.get_mut(index) {
                        cluster.advance += position.x_advance as f32 * scale;
                    }
                }
            }
            for cluster in run.iter_mut() {
                cluster.advance += self.letter_spacing;
            }
            start = end;
        }
    }

    fn place_line(
        &mut self,
        paragraph: &Paragraph<'_>,
        visible: Range<usize>,
        line: &mut LaidLine,
    ) {
        let clusters = &paragraph.clusters[visible];
        let (Some(first), Some(last), Some(info)) = (
            clusters.first(),
            clusters.last(),
            paragraph.bidi.paragraphs.first(),
        ) else {
            return;
        };
        let line_index = self.lines.len();
        let (levels, runs) = paragraph
            .bidi
            .visual_runs(info, first.bytes.start..last.bytes.end);
        let mut pen_x = 0.0f32;
        for run in runs {
            let level = levels[run.start];
            let first = clusters.partition_point(|cluster| cluster.bytes.start < run.start);
            let end = clusters.partition_point(|cluster| cluster.bytes.start < run.end);
            let mut face_runs: Vec<&[Cluster]> = clusters[first..end]
                .chunk_by(|a, b| a.face == b.face)
                .collect();
            if level.is_rtl() {
                face_runs.reverse();
            }
            for face_run in face_runs {
                pen_x = self.place_run(paragraph.text, face_run, level, line_index, pen_x, line);
            }
        }
        line.width = pen_x;
    }

    fn place_run(
        &mut self,
        text: &str,
        run: &[Cluster],
        level: Level,
        line_index: usize,
        mut pen_x: f32,
        line: &mut LaidLine,
    ) -> f32 {
        let face = run[0].face;
        let Some(shaper) = self.faces.shapers[face].as_ref() else {
            return pen_x;
        };
        let scale = self.ppem / self.faces.faces[face].units_per_em();
        let characters = run.iter().flat_map(|cluster| {
            text[cluster.bytes.clone()]
                .chars()
                .enumerate()
                .map(|(offset, character)| (cluster.chars.start + offset, character))
        });
        let shaped = shape(shaper, characters, level.is_rtl());
        let run_end = run[run.len() - 1].chars.end;
        let mut starts: Vec<usize> = shaped
            .glyph_infos()
            .iter()
            .map(|info| info.cluster as usize)
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let group_end = |start: usize| {
            starts
                .get(starts.partition_point(|&next| next <= start))
                .copied()
                .unwrap_or(run_end)
        };
        let tab_advance = |start: usize| {
            run.binary_search_by_key(&start, |cluster| cluster.chars.start)
                .ok()
                .map(|index| &run[index])
                .filter(|cluster| cluster.tab)
                .map(|tab| tab.advance)
        };
        let mut open: Option<ClusterBox> = None;
        let mut tab = None;
        for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            let cluster_start = info.cluster as usize;
            if open
                .as_ref()
                .is_none_or(|current| current.chars.start != cluster_start)
            {
                if let Some(mut done) = open.take() {
                    pen_x += self.letter_spacing;
                    done.right = pen_x;
                    line.boxes.push(done);
                }
                open = Some(ClusterBox {
                    chars: cluster_start..group_end(cluster_start),
                    left: pen_x,
                    right: pen_x,
                    rtl: level.is_rtl(),
                });
                tab = tab_advance(cluster_start);
            }
            self.glyphs.push(PlacedGlyph {
                face,
                glyph: info.glyph_id,
                line: line_index,
                x: pen_x + position.x_offset as f32 * scale,
                rise: position.y_offset as f32 * scale,
            });
            pen_x += tab.unwrap_or(position.x_advance as f32 * scale);
        }
        if let Some(mut done) = open.take() {
            pen_x += self.letter_spacing;
            done.right = pen_x;
            line.boxes.push(done);
        }
        pen_x
    }
}

pub(crate) fn lay_out<'a>(
    text: &str,
    selection: Selection,
    style: &FieldStyle,
    chain: FaceChain<'a>,
    previous: Scroll,
) -> FieldLayout<'a> {
    let selection = match selection {
        Selection::All if text.is_empty() => Selection::Caret(0),
        selection => selection,
    };
    let ppem = style.font_size * chain.em_per_size;
    let ascent = chain.primary.ascent_per_em() * ppem;
    let line_height = ascent + chain.primary.descent_per_em() * ppem;
    let display: String = text
        .chars()
        .map(|character| match (style.lines, character) {
            (LineMode::Single, '\n') => ' ',
            (_, character) => character,
        })
        .collect();
    let mut typesetter = Typesetter {
        faces: FaceSet::new(chain),
        ppem,
        letter_spacing: chain.letter_spacing_em * ppem,
        max_width: match style.lines {
            LineMode::Single => f32::INFINITY,
            LineMode::Wrapped => style.width.max(1.0),
        },
        lines: Vec::new(),
        glyphs: Vec::new(),
    };
    let mut first_char = 0;
    for paragraph in display.split('\n') {
        typesetter.paragraph(paragraph, first_char);
        first_char += paragraph.chars().count() + 1;
    }
    let mut layout = FieldLayout {
        faces: typesetter.faces.faces,
        ppem,
        ascent,
        line_height,
        glyphs: typesetter.glyphs,
        lines: typesetter.lines,
        caret: None,
        highlights: Vec::new(),
        scroll: Scroll::default(),
    };
    layout.place(selection, style, previous);
    layout
}

impl FieldLayout<'_> {
    fn place(&mut self, selection: Selection, style: &FieldStyle, previous: Scroll) {
        let caret = match selection {
            Selection::Caret(index) => {
                let line = self.line_of(index);
                Some((line, self.caret_offset(line, index)))
            }
            Selection::All => None,
        };
        let total_height = self.lines.len() as f32 * self.line_height;
        let block_top = if style.lines == LineMode::Wrapped && total_height > style.height {
            let maximum = total_height - style.height;
            let mut scroll_y = previous.y.clamp(0.0, maximum);
            if let Some((line, _)) = caret {
                let top = line as f32 * self.line_height;
                scroll_y = scroll_y.min(top);
                scroll_y = scroll_y.max(top + self.line_height - style.height);
            }
            self.scroll.y = scroll_y.clamp(0.0, maximum);
            -self.scroll.y
        } else {
            match style.vertical {
                VerticalAlignment::Top => 0.0,
                VerticalAlignment::Center => (style.height - total_height) * 0.5,
                VerticalAlignment::Bottom => style.height - total_height,
            }
        };
        let overflow = match (style.lines, self.lines.first()) {
            (LineMode::Single, Some(line)) => line.width + CARET_WIDTH - style.width,
            _ => 0.0,
        };
        if overflow > 0.0 {
            let mut scroll_x = previous.x.clamp(0.0, overflow);
            if let Some((_, x)) = caret {
                scroll_x = scroll_x.min(x);
                scroll_x = scroll_x.max(x + CARET_WIDTH - style.width);
            }
            self.scroll.x = scroll_x.clamp(0.0, overflow);
        }
        for (index, line) in self.lines.iter_mut().enumerate() {
            line.top = block_top + index as f32 * self.line_height;
            line.left = if overflow > 0.0 {
                -self.scroll.x
            } else {
                let spare = (style.width - line.width).max(0.0);
                match style.horizontal {
                    HorizontalAlignment::Left => 0.0,
                    HorizontalAlignment::Right => spare,
                    HorizontalAlignment::Center => spare * 0.5,
                }
            };
            for cluster in &mut line.boxes {
                cluster.left += line.left;
                cluster.right += line.left;
            }
        }
        self.caret = caret.map(|(line, offset)| {
            let line = &self.lines[line];
            let x = (line.left + offset)
                .round()
                .clamp(0.0, (style.width - CARET_WIDTH).max(0.0));
            FieldRect {
                left: x,
                top: line.top,
                right: x + CARET_WIDTH,
                bottom: line.top + self.line_height,
            }
        });
        if selection == Selection::All {
            self.highlights = self
                .lines
                .iter()
                .filter(|line| line.width > 0.0)
                .map(|line| FieldRect {
                    left: line.left,
                    top: line.top,
                    right: line.left + line.width,
                    bottom: line.top + self.line_height,
                })
                .collect();
        }
    }

    fn line_of(&self, index: usize) -> usize {
        self.lines
            .iter()
            .position(|line| {
                index < line.chars.end || (index == line.chars.end && line.ends_paragraph)
            })
            .unwrap_or(self.lines.len().saturating_sub(1))
    }

    fn caret_offset(&self, line: usize, index: usize) -> f32 {
        let line = &self.lines[line];
        let (start_edge, end_edge) = if line.rtl {
            (line.width, 0.0)
        } else {
            (0.0, line.width)
        };
        let containing = |char_index: usize| {
            line.boxes
                .iter()
                .find(|cluster| cluster.chars.contains(&char_index))
        };
        if index <= line.chars.start {
            return match containing(index) {
                Some(cluster) if cluster.rtl => cluster.right,
                Some(cluster) => cluster.left,
                None => start_edge,
            };
        }
        match containing(index - 1) {
            Some(cluster) if cluster.chars.end > index => {
                if cluster.rtl {
                    cluster.right
                } else {
                    cluster.left
                }
            }
            Some(cluster) if cluster.rtl => cluster.left,
            Some(cluster) => cluster.right,
            None if index >= line.visible_end => end_edge,
            None => start_edge,
        }
    }

    pub(crate) fn scroll(&self) -> Scroll {
        self.scroll
    }

    pub(crate) fn hit_test(&self, x: f32, y: f32) -> usize {
        let Some(first) = self.lines.first() else {
            return 0;
        };
        let row = ((y - first.top) / self.line_height).floor().max(0.0) as usize;
        let line = &self.lines[row.min(self.lines.len() - 1)];
        let line_end = if line.ends_paragraph {
            line.chars.end
        } else {
            line.visible_end
        };
        let edge = |cluster: &ClusterBox, left_side: bool| {
            if left_side != cluster.rtl {
                cluster.chars.start
            } else {
                cluster.chars.end
            }
        };
        let (Some(leftmost), Some(rightmost)) = (line.boxes.first(), line.boxes.last()) else {
            return line.chars.start;
        };
        let index = if x < leftmost.left {
            edge(leftmost, true)
        } else if x >= rightmost.right {
            edge(rightmost, false)
        } else {
            line.boxes
                .iter()
                .find(|cluster| x < cluster.right)
                .map_or(line_end, |cluster| {
                    edge(cluster, x < (cluster.left + cluster.right) * 0.5)
                })
        };
        index.clamp(line.chars.start, line_end)
    }

    pub(crate) fn caret_pixels(&self, viewport: Viewport) -> Option<PixelRect> {
        self.caret.and_then(|rect| pixel_rect(rect, viewport))
    }

    pub(crate) fn paint(
        &self,
        viewport: Viewport,
        text_color: [u8; 4],
        highlight_color: [u8; 4],
    ) -> Vec<[u8; 4]> {
        let mut pixels = vec![[0u8; 4]; viewport.width as usize * viewport.height as usize];
        let highlight = premultiply(highlight_color);
        for area in self
            .highlights
            .iter()
            .filter_map(|rect| pixel_rect(*rect, viewport))
        {
            for y in area.y0..area.y1 {
                for x in area.x0..area.x1 {
                    let index = (y * viewport.width + x) as usize;
                    pixels[index] = over(highlight, pixels[index]);
                }
            }
        }
        let reaches: Vec<Option<FieldRect>> = self
            .faces
            .iter()
            .map(|font| ink_reach(font, self.ppem))
            .collect();
        let mut sized: Vec<Option<ScaledFont>> = self.faces.iter().map(|_| None).collect();
        for glyph in &self.glyphs {
            let line = &self.lines[glyph.line];
            let (x, baseline) = (line.left + glyph.x, line.top + self.ascent - glyph.rise);
            if reaches[glyph.face]
                .is_some_and(|reach| !reach_overlaps(reach, x, baseline, viewport))
            {
                continue;
            }
            let scaled = match &mut sized[glyph.face] {
                Some(scaled) => scaled,
                empty => match self.faces[glyph.face].at_em(self.ppem) {
                    Some(scaled) => empty.insert(scaled),
                    None => continue,
                },
            };
            let Some(image) = scaled.render(glyph.glyph, x, baseline) else {
                continue;
            };
            for row in 0..image.height {
                let y = i64::from(image.top) + i64::from(row) - i64::from(viewport.y);
                if y < 0 || y >= i64::from(viewport.height) {
                    continue;
                }
                for column in 0..image.width {
                    let x = i64::from(image.left) + i64::from(column) - i64::from(viewport.x);
                    if x < 0 || x >= i64::from(viewport.width) {
                        continue;
                    }
                    let source = (row * image.width + column) as usize;
                    let ink = match &image.pixels {
                        GlyphPixels::Coverage(values) => premultiply([
                            text_color[0],
                            text_color[1],
                            text_color[2],
                            scale_byte(values[source], text_color[3]),
                        ]),
                        GlyphPixels::Color(values) => {
                            values[source].map(|channel| scale_byte(channel, text_color[3]))
                        }
                    };
                    if ink[3] != 0 {
                        let index = (y as usize) * viewport.width as usize + x as usize;
                        pixels[index] = over(ink, pixels[index]);
                    }
                }
            }
        }
        pixels
    }
}

fn ink_reach(font: &RasterFont, ppem: f32) -> Option<FieldRect> {
    if font.glyph_source() == GlyphSource::ColorStrikes {
        return None;
    }
    let bounds = rustybuzz::ttf_parser::Face::parse(font.data(), font.index())
        .ok()?
        .global_bounding_box();
    let scale = ppem / font.units_per_em();
    Some(FieldRect {
        left: f32::from(bounds.x_min) * scale - INK_MARGIN,
        top: -f32::from(bounds.y_max) * scale - INK_MARGIN,
        right: f32::from(bounds.x_max) * scale + INK_MARGIN,
        bottom: -f32::from(bounds.y_min) * scale + INK_MARGIN,
    })
}

fn reach_overlaps(reach: FieldRect, x: f32, baseline: f32, viewport: Viewport) -> bool {
    x + reach.right > viewport.x as f32
        && x + reach.left < (i64::from(viewport.x) + i64::from(viewport.width)) as f32
        && baseline + reach.bottom > viewport.y as f32
        && baseline + reach.top < (i64::from(viewport.y) + i64::from(viewport.height)) as f32
}

fn pixel_rect(rect: FieldRect, viewport: Viewport) -> Option<PixelRect> {
    let clamp = |value: f32, origin: i32, limit: u32| {
        (value.round() as i64 - i64::from(origin)).clamp(0, i64::from(limit)) as u32
    };
    let area = PixelRect {
        x0: clamp(rect.left, viewport.x, viewport.width),
        y0: clamp(rect.top, viewport.y, viewport.height),
        x1: clamp(rect.right, viewport.x, viewport.width),
        y1: clamp(rect.bottom, viewport.y, viewport.height),
    };
    (area.x0 < area.x1 && area.y0 < area.y1).then_some(area)
}

pub(crate) fn scale_byte(value: u8, scale: u8) -> u8 {
    ((u32::from(value) * u32::from(scale) + 127) / 255) as u8
}

pub(crate) fn premultiply(color: [u8; 4]) -> [u8; 4] {
    [
        scale_byte(color[0], color[3]),
        scale_byte(color[1], color[3]),
        scale_byte(color[2], color[3]),
        color[3],
    ]
}

pub(crate) fn over(source: [u8; 4], destination: [u8; 4]) -> [u8; 4] {
    let keep = 255 - source[3];
    std::array::from_fn(|channel| {
        source[channel].saturating_add(scale_byte(destination[channel], keep))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: f32 = 24.0;

    fn host_chain() -> Option<FaceChain<'static>> {
        let Some(primary) = host_fonts::system_font() else {
            eprintln!("SKIP: no host font for text layout");
            return None;
        };
        Some(FaceChain {
            primary,
            em_per_size: primary.em_per_height(),
            letter_spacing_em: 0.0,
            fallbacks: &[],
        })
    }

    fn style(width: u32, height: u32, lines: LineMode) -> FieldStyle {
        FieldStyle {
            font_size: SIZE,
            width: width as f32,
            height: height as f32,
            lines,
            horizontal: HorizontalAlignment::Left,
            vertical: VerticalAlignment::Top,
        }
    }

    fn at_end(text: &str) -> Selection {
        Selection::Caret(text.chars().count())
    }

    fn viewport(style: &FieldStyle) -> Viewport {
        Viewport {
            x: 0,
            y: 0,
            width: style.width as u32,
            height: style.height as u32,
        }
    }

    fn ink(layout: &FieldLayout<'_>, style: &FieldStyle) -> Vec<(u32, u32)> {
        let view = viewport(style);
        layout
            .paint(view, [255, 255, 255, 255], [0, 0, 255, 255])
            .iter()
            .enumerate()
            .filter(|(_, pixel)| pixel[3] != 0)
            .map(|(index, _)| (index as u32 % view.width, index as u32 / view.width))
            .collect()
    }

    fn caret(layout: &FieldLayout<'_>) -> FieldRect {
        layout.caret.expect("caret")
    }

    #[test]
    fn roblox_alignment_and_wrapping_flags_map_like_roblox_keyboard() {
        let single = FieldStyle::of_text_box(18.0, (300, 40), false, false, 1, 2);
        assert_eq!(single.lines, LineMode::Single);
        assert_eq!(single.horizontal, HorizontalAlignment::Right);
        assert_eq!(single.vertical, VerticalAlignment::Bottom);
        for (multiline, wrapped) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                FieldStyle::of_text_box(18.0, (300, 40), multiline, wrapped, 2, 1).lines,
                LineMode::Wrapped
            );
        }
        assert_eq!(
            FieldStyle::of_text_box(f32::NAN, (1, 1), false, false, 0, 0).font_size,
            DEFAULT_FONT_SIZE
        );
        assert_eq!(
            FieldStyle::of_text_box(155.0, (1, 1), false, false, 0, 0).font_size,
            MAX_FONT_SIZE
        );
        assert_eq!(char_index_at_utf16("a\u{1F4AB}b", 3), 2);
        assert_eq!(char_index_at_utf16("ab", 9), 2);
    }

    #[test]
    fn the_font_size_sets_the_line_height_whatever_the_field_height() {
        let Some(chain) = host_chain() else {
            return;
        };
        for height in [20, 46, 200] {
            let field = style(300, height, LineMode::Single);
            let layout = lay_out("Hg", at_end("Hg"), &field, chain, Scroll::default());
            assert!((layout.line_height - SIZE).abs() < 0.01);
            assert_eq!(
                caret(&layout).bottom - caret(&layout).top,
                layout.line_height
            );
        }
    }

    #[test]
    fn single_line_text_wider_than_the_field_scrolls_to_keep_the_caret_visible() {
        let Some(chain) = host_chain() else {
            return;
        };
        let text = "the quick brown fox jumps over the lazy dog then END";
        let field = style(240, 40, LineMode::Single);
        let end = lay_out(text, at_end(text), &field, chain, Scroll::default());
        let caret_at_end = caret(&end);
        assert!(end.scroll.x > 0.0);
        assert!(caret_at_end.right <= 240.0 && caret_at_end.left >= 240.0 - CARET_WIDTH - 1.0);
        let prefix = lay_out(
            "the quick brown fox",
            at_end("the quick brown fox"),
            &field,
            chain,
            Scroll::default(),
        );
        assert_ne!(ink(&end, &field), ink(&prefix, &field));

        let end_ink = ink(&end, &field);
        let rightmost = end_ink.iter().map(|&(x, _)| x).max().expect("ink");
        assert!(
            rightmost as f32 >= caret_at_end.left - SIZE,
            "the text end is drawn next to the caret, ink ends at {rightmost}"
        );

        let start = lay_out(text, Selection::Caret(0), &field, chain, Scroll::default());
        assert_eq!(start.scroll.x, 0.0);
        assert_eq!(caret(&start).left, 0.0);
        assert_ne!(
            ink(&start, &field),
            end_ink,
            "the glyphs move with the scroll"
        );

        let visible = text.chars().count() - 3;
        let kept = lay_out(text, Selection::Caret(visible), &field, chain, end.scroll);
        assert_eq!(
            kept.scroll, end.scroll,
            "moving the caret inside the view keeps the scroll"
        );
    }

    #[test]
    fn wrapped_text_breaks_at_words_and_scrolls_to_the_caret_line() {
        let Some(chain) = host_chain() else {
            return;
        };
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let characters: Vec<char> = text.chars().collect();
        let tall = style(160, 400, LineMode::Wrapped);
        let layout = lay_out(text, at_end(text), &tall, chain, Scroll::default());
        assert!(layout.lines.len() > 2);
        for line in &layout.lines[1..] {
            assert_eq!(
                characters[line.chars.start - 1],
                ' ',
                "a line starts mid-word at {}",
                line.chars.start
            );
        }
        for line in &layout.lines {
            assert!(line.width <= 160.0 + 0.01, "line overflows: {}", line.width);
        }

        let short = style(160, 30, LineMode::Wrapped);
        let scrolled = lay_out(text, at_end(text), &short, chain, Scroll::default());
        let caret_box = caret(&scrolled);
        assert!(scrolled.scroll.y > 0.0);
        assert!(caret_box.top >= -0.01 && caret_box.bottom <= 30.0 + 0.01);
        assert!(
            ink(&scrolled, &short)
                .iter()
                .any(|&(_, y)| y as f32 >= caret_box.top && (y as f32) < caret_box.bottom),
            "the caret line is drawn"
        );
    }

    #[test]
    fn vertical_alignment_positions_the_whole_block_of_lines() {
        let Some(chain) = host_chain() else {
            return;
        };
        let text = "first\nsecond";
        let mut field = style(200, 200, LineMode::Wrapped);
        let tops = |field: &FieldStyle| {
            let layout = lay_out(text, at_end(text), field, chain, Scroll::default());
            (
                layout.lines[0].top,
                layout.lines[1].top + layout.line_height,
            )
        };
        field.vertical = VerticalAlignment::Top;
        assert_eq!(tops(&field).0, 0.0);
        field.vertical = VerticalAlignment::Bottom;
        assert!((tops(&field).1 - 200.0).abs() < 0.01);
        field.vertical = VerticalAlignment::Center;
        let (top, bottom) = tops(&field);
        assert!((top - (200.0 - bottom)).abs() < 0.01);
    }

    #[test]
    fn the_caret_sits_at_the_cursor_and_an_empty_box_keeps_one() {
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(320, 40, LineMode::Single);
        let x = |text: &str, index: usize| {
            caret(&lay_out(
                text,
                Selection::Caret(index),
                &field,
                chain,
                Scroll::default(),
            ))
            .left
        };
        assert_eq!(x("hihihi", 2), x("hi", 2));
        assert!(x("hihihi", 2) < x("hihihi", 6));
        assert_eq!(x("", 0), 0.0);
        for (alignment, expected) in [
            (HorizontalAlignment::Left, 0.0),
            (HorizontalAlignment::Right, 320.0 - CARET_WIDTH),
            (HorizontalAlignment::Center, 160.0),
        ] {
            let aligned = FieldStyle {
                horizontal: alignment,
                ..field
            };
            let empty = lay_out("", Selection::Caret(0), &aligned, chain, Scroll::default());
            assert_eq!(caret(&empty).left, expected, "{alignment:?}");
        }
    }

    #[test]
    fn aligned_text_is_drawn_on_the_field_edge_it_is_aligned_to() {
        let Some(chain) = host_chain() else {
            return;
        };
        let bearing = SIZE * 0.2;
        for (alignment, width) in [
            (HorizontalAlignment::Left, 320),
            (HorizontalAlignment::Right, 320),
            (HorizontalAlignment::Center, 320),
        ] {
            let field = FieldStyle {
                horizontal: alignment,
                ..style(width, 40, LineMode::Single)
            };
            let layout = lay_out("hihihi", at_end("hihihi"), &field, chain, Scroll::default());
            let columns: Vec<u32> = ink(&layout, &field).iter().map(|&(x, _)| x).collect();
            let (left, right) = (
                *columns.iter().min().expect("ink") as f32,
                *columns.iter().max().expect("ink") as f32,
            );
            match alignment {
                HorizontalAlignment::Left => assert!(left < bearing, "left ink at {left}"),
                HorizontalAlignment::Right => {
                    assert!(width as f32 - 1.0 - right < bearing, "right ink at {right}")
                }
                HorizontalAlignment::Center => assert!(
                    ((left + right) * 0.5 - width as f32 * 0.5).abs() < bearing,
                    "centred ink spans {left}..{right}"
                ),
            }
        }
    }

    #[test]
    fn select_all_highlights_every_line_and_hides_the_caret() {
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(200, 100, LineMode::Wrapped);
        let layout = lay_out("one\ntwo", Selection::All, &field, chain, Scroll::default());
        assert!(layout.caret.is_none());
        assert_eq!(layout.highlights.len(), 2);
        let painted = layout.paint(viewport(&field), [255, 255, 255, 0], [0, 0, 255, 102]);
        let highlight = premultiply([0, 0, 255, 102]);
        let rect = layout.highlights[0];
        let inside = ((rect.top as u32 + 1) * 200 + rect.left as u32) as usize;
        assert_eq!(painted[inside], highlight);
        assert!(painted[(99 * 200 + 199) as usize][3] == 0);
    }

    #[test]
    fn clicks_map_back_to_the_caret_position_they_hit() {
        let Some(chain) = host_chain() else {
            return;
        };
        let text = "The quick brown fox";
        let field = style(400, 40, LineMode::Single);
        for index in 0..=text.chars().count() {
            let layout = lay_out(
                text,
                Selection::Caret(index),
                &field,
                chain,
                Scroll::default(),
            );
            let caret_box = caret(&layout);
            let hit = layout.hit_test(caret_box.left + 1.0, caret_box.top + 2.0);
            assert_eq!(hit, index, "a click on the caret after {index} characters");
        }
        let layout = lay_out(text, at_end(text), &field, chain, Scroll::default());
        assert_eq!(layout.hit_test(-5.0, 5.0), 0);
        assert_eq!(layout.hit_test(399.0, 5.0), text.chars().count());
    }

    #[test]
    fn tabs_advance_to_tab_stops_and_controls_draw_nothing() {
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(200, 40, LineMode::Single);
        let layout = lay_out("A\tB", at_end("A\tB"), &field, chain, Scroll::default());
        let b = layout.lines[0]
            .boxes
            .iter()
            .find(|cluster| cluster.chars.start == 2)
            .expect("B");
        assert_eq!(b.left, TAB_STOP);
        let a_right = layout.lines[0].boxes[0].right.ceil() as u32 + 1;
        assert!(ink(&layout, &field)
            .iter()
            .all(|&(x, _)| x < a_right || x >= TAB_STOP as u32));
        let hidden = lay_out("\u{1}", at_end("\u{1}"), &field, chain, Scroll::default());
        assert!(ink(&hidden, &field).is_empty());
    }

    #[test]
    fn glyphs_missing_from_the_primary_face_come_from_host_fallbacks() {
        let Some(chain) = host_chain() else {
            return;
        };
        let covered = |text: &str| covers(chain.primary, text);
        if covered("\u{4F60}") || host_fonts::fallback_for('\u{4F60}', Presentation::Text).is_none()
        {
            eprintln!("SKIP: the host has no separate CJK fallback font");
            return;
        }
        let field = style(200, 40, LineMode::Single);
        let cjk = lay_out(
            "\u{4F60}",
            at_end("\u{4F60}"),
            &field,
            chain,
            Scroll::default(),
        );
        let tofu = lay_out(
            "\u{10FFFD}",
            at_end("\u{10FFFD}"),
            &field,
            chain,
            Scroll::default(),
        );
        assert!(cjk.glyphs.iter().all(|glyph| glyph.face != 0));
        assert_ne!(ink(&cjk, &field), ink(&tofu, &field));
    }

    #[test]
    fn right_to_left_text_is_reordered_and_arabic_joins() {
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(300, 40, LineMode::Single);
        let hebrew = "\u{5E9}\u{5DC}\u{5D5}\u{5DD}";
        if !covered_somewhere(chain, hebrew) {
            eprintln!("SKIP: no font covers Hebrew");
        } else {
            let layout = lay_out(
                hebrew,
                Selection::Caret(0),
                &field,
                chain,
                Scroll::default(),
            );
            let boxes = &layout.lines[0].boxes;
            let first = boxes
                .iter()
                .find(|cluster| cluster.chars.start == 0)
                .expect("first");
            let last = boxes
                .iter()
                .find(|cluster| cluster.chars.start == 3)
                .expect("last");
            assert!(
                first.left > last.left,
                "the first letter is drawn rightmost"
            );
            assert!(first.rtl && last.rtl);
            assert_eq!(caret(&layout).left, first.right.round());
        }
        let arabic = "\u{628}\u{628}\u{628}";
        if !covered_somewhere(chain, arabic) {
            eprintln!("SKIP: no font covers Arabic");
            return;
        }
        let layout = lay_out(arabic, at_end(arabic), &field, chain, Scroll::default());
        let glyphs: Vec<u32> = layout.glyphs.iter().map(|glyph| glyph.glyph).collect();
        assert_eq!(glyphs.len(), 3);
        assert_ne!(glyphs[0], glyphs[2], "initial and final forms differ");
    }

    #[test]
    fn invisible_format_characters_draw_nothing() {
        for character in [
            '\u{00AD}',
            '\u{061C}',
            '\u{202A}',
            '\u{202E}',
            '\u{2066}',
            '\u{2069}',
            '\u{FEFF}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{1BCA0}',
            '\u{E0001}',
        ] {
            assert!(is_ignorable(character), "{character:?} needs no font");
        }
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(200, 40, LineMode::Single);
        let plain = lay_out("ab", at_end("ab"), &field, chain, Scroll::default());
        let marked_text = "\u{202A}a\u{FFF9}b\u{FFFB}\u{1BCA0}\u{202C}";
        let marked = lay_out(
            marked_text,
            at_end(marked_text),
            &field,
            chain,
            Scroll::default(),
        );
        assert_eq!(marked.lines[0].width, plain.lines[0].width);
        assert_eq!(ink(&marked, &field), ink(&plain, &field));
    }

    #[test]
    fn a_roblox_face_is_drawn_at_its_size_ratio_with_per_glyph_chain_fallback() {
        let Some(primary) = host_fonts::system_font() else {
            eprintln!("SKIP: no host font to stand in for a Roblox face");
            return;
        };
        let missing = '\u{4F60}';
        let Some(covering) = host_fonts::fallback_for(missing, Presentation::Text)
            .filter(|_| !covers(primary, "\u{4F60}"))
        else {
            eprintln!("SKIP: no host font covers what the primary face lacks");
            return;
        };
        let bundled = RasterFont::open(covering.data().to_vec().into(), covering.index())
            .expect("bundled fallback opens");
        let fallbacks = [&bundled];
        let ratio = 0.793_650_8;
        let chain = FaceChain {
            primary,
            em_per_size: ratio,
            letter_spacing_em: 0.0,
            fallbacks: &fallbacks,
        };
        let field = style(400, 40, LineMode::Single);
        let digits = "0123456789";
        let layout = lay_out(digits, at_end(digits), &field, chain, Scroll::default());
        assert_eq!(layout.ppem, SIZE * ratio);
        let face = rustybuzz::ttf_parser::Face::parse(primary.data(), primary.index())
            .expect("primary parses");
        let advances: f32 = digits
            .chars()
            .map(|digit| {
                let glyph = face.glyph_index(digit).expect("digit glyph");
                f32::from(face.glyph_hor_advance(glyph).expect("advance"))
            })
            .sum();
        let expected = advances * SIZE * ratio / primary.units_per_em();
        assert!(
            (layout.lines[0].width - expected).abs() < 1.0,
            "digits span {} px, not {expected} px",
            layout.lines[0].width
        );

        let mixed = format!("a{missing}b");
        let layout = lay_out(&mixed, at_end(&mixed), &field, chain, Scroll::default());
        let faces: Vec<&RasterFont> = layout.glyphs.iter().map(|g| layout.faces[g.face]).collect();
        assert!(std::ptr::eq(faces[0], primary) && std::ptr::eq(faces[2], primary));
        assert!(
            std::ptr::eq(faces[1], &bundled),
            "the missing glyph comes from the chain's own fallback"
        );
        assert!(layout.glyphs.iter().all(|glyph| glyph.glyph != 0));
    }

    fn covered_somewhere(chain: FaceChain<'_>, text: &str) -> bool {
        covers(chain.primary, text)
            || text
                .chars()
                .all(|character| host_fonts::fallback_for(character, Presentation::Text).is_some())
    }

    #[test]
    fn colour_emoji_keep_their_colours() {
        let Some(chain) = host_chain() else {
            return;
        };
        if host_fonts::fallback_for('\u{1F600}', Presentation::Emoji).is_none() {
            eprintln!("SKIP: the host has no colour emoji font FreeType can draw");
            return;
        }
        let field = style(100, 40, LineMode::Single);
        let layout = lay_out(
            "\u{1F600}",
            at_end("\u{1F600}"),
            &field,
            chain,
            Scroll::default(),
        );
        let painted = layout.paint(viewport(&field), [255, 255, 255, 255], [0, 0, 0, 0]);
        assert!(
            painted
                .iter()
                .any(|pixel| pixel[3] != 0 && (pixel[0] != pixel[1] || pixel[1] != pixel[2])),
            "an emoji is drawn in colour, not in the text colour"
        );
    }

    #[test]
    fn single_line_layout_time_grows_linearly_with_the_text() {
        let Some(chain) = host_chain() else {
            return;
        };
        let field = style(600, 40, LineMode::Single);
        let fastest = |length: usize| {
            let text: String = "lorem ipsum dolor sit amet "
                .chars()
                .cycle()
                .take(length)
                .collect();
            (0..5)
                .map(|_| {
                    let started = std::time::Instant::now();
                    let layout = lay_out(&text, at_end(&text), &field, chain, Scroll::default());
                    layout.hit_test(100.0, 10.0);
                    started.elapsed()
                })
                .min()
                .expect("timed layouts")
        };
        let short = fastest(1_000);
        let long = fastest(8_000);
        assert!(
            long < short * 14,
            "eight times the text took {long:?} against {short:?}"
        );
    }

    #[test]
    fn a_cropped_viewport_paints_the_same_pixels_as_the_whole_field() {
        let Some(chain) = host_chain() else {
            return;
        };
        let text = "Wide glyphs WWW mmm @@@ and more words than the field can show";
        let field = style(240, 60, LineMode::Wrapped);
        let layout = lay_out(text, at_end(text), &field, chain, Scroll::default());
        let whole = viewport(&field);
        let painted = layout.paint(whole, [255, 255, 255, 255], [0, 0, 255, 102]);
        let crop = Viewport {
            x: 37,
            y: 11,
            width: 101,
            height: 23,
        };
        let cropped = layout.paint(crop, [255, 255, 255, 255], [0, 0, 255, 102]);
        assert!(
            cropped.iter().any(|pixel| pixel[3] != 0),
            "the crop holds text"
        );
        for row in 0..crop.height {
            for column in 0..crop.width {
                let inside =
                    ((crop.y as u32 + row) * whole.width + crop.x as u32 + column) as usize;
                assert_eq!(
                    cropped[(row * crop.width + column) as usize],
                    painted[inside],
                    "pixel {column},{row} of the crop"
                );
            }
        }
    }

    #[test]
    fn premultiplied_over_matches_straight_alpha_blending() {
        assert_eq!(over([0, 0, 0, 0], [10, 20, 30, 40]), [10, 20, 30, 40]);
        assert_eq!(over([255, 0, 0, 255], [10, 20, 30, 40]), [255, 0, 0, 255]);
        assert_eq!(
            over(premultiply([255, 255, 255, 128]), [0, 0, 0, 255]),
            [128, 128, 128, 255]
        );
    }
}
