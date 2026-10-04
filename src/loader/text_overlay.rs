use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::framework::{ActiveTextOverlay, TextSelection};
use crate::text_layout::{FieldStyle, PixelRect, Scroll, Selection, Viewport};

fn encode_png_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    fn crc32(buf: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in buf {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn adler32(buf: &[u8]) -> u32 {
        let (mut a, mut b) = (1u32, 0u32);
        for &x in buf {
            a = (a + u32::from(x)) % 65521;
            b = (b + a) % 65521;
        }
        (b << 16) | a
    }
    fn chunk(out: &mut Vec<u8>, typ: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(typ);
        out.extend_from_slice(data);
        let mut crc_in = typ.to_vec();
        crc_in.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }
    let mut out = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);

    let mut raw = Vec::with_capacity((w * h * 4 + h) as usize);
    for y in 0..h as usize {
        raw.push(0);
        let row = &rgba[y * w as usize * 4..(y + 1) * w as usize * 4];
        raw.extend_from_slice(row);
    }

    let mut zlib = vec![0x78u8, 0x01];
    let mut i = 0;
    while i < raw.len() {
        let block = (raw.len() - i).min(65535);
        let bfinal = u8::from(i + block >= raw.len());
        zlib.push(bfinal);
        zlib.extend_from_slice(&(block as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[i..i + block]);
        i += block;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    out
}

const CARET_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
const SELECTION_HIGHLIGHT_ALPHA: u8 = 0x66;
const TEXT_TEST_FONT: i32 = 46;

fn caret_visible(since_change: Duration) -> bool {
    (since_change.as_millis() / CARET_BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ByteOrder {
    Rgba,
    Bgra,
}

impl ByteOrder {
    pub(super) fn arrange(self, [r, g, b, a]: [u8; 4]) -> [u8; 4] {
        match self {
            Self::Rgba => [r, g, b, a],
            Self::Bgra => [b, g, r, a],
        }
    }
}

fn text_rgba(argb: i32) -> [u8; 4] {
    let [a, r, g, b] = (argb as u32).to_be_bytes();
    [r, g, b, a]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SurfaceSize {
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SurfaceRect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TextRequest {
    widget: i64,
    text: String,
    selection: Selection,
    font: i32,
    style: FieldStyle,
    viewport: Viewport,
    text_color: i32,
    order: ByteOrder,
}

impl TextRequest {
    pub(super) fn of(overlay: &ActiveTextOverlay, rect: SurfaceRect, order: ByteOrder) -> Self {
        let (field_x, field_y, width, height) = overlay.geometry;
        Self {
            widget: overlay.widget,
            text: crate::framework::displayed_text_box_text(&overlay.text, overlay.input_type)
                .into_owned(),
            selection: match overlay.selection {
                TextSelection::All => Selection::All,
                TextSelection::Cursor(utf16) => Selection::Caret(
                    crate::text_layout::char_index_at_utf16(&overlay.text, utf16),
                ),
            },
            font: overlay.font,
            style: FieldStyle::of_text_box(
                overlay.font_size,
                (width, height),
                overlay.multiline,
                overlay.text_wrapped,
                overlay.x_alignment,
                overlay.y_alignment,
            ),
            viewport: Viewport {
                x: rect.x.saturating_sub(field_x),
                y: rect.y.saturating_sub(field_y),
                width: rect.width,
                height: rect.height,
            },
            text_color: overlay.text_color,
            order,
        }
    }
}

pub(super) struct TextLayer {
    request: TextRequest,
    pub(super) generation: u64,
    pub(super) pixels: Vec<[u8; 4]>,
    pub(super) caret: Option<PixelRect>,
    pub(super) caret_color: [u8; 4],
}

fn build_text_layer(
    request: &TextRequest,
    generation: u64,
    previous: Scroll,
) -> Option<(TextLayer, Scroll)> {
    let chain = crate::framework::roblox_fonts::face_chain(request.font)?;
    let layout = crate::text_layout::lay_out(
        &request.text,
        request.selection,
        &request.style,
        chain,
        previous,
    );
    let color = text_rgba(request.text_color);
    let highlight = [
        color[0],
        color[1],
        color[2],
        crate::text_layout::scale_byte(color[3], SELECTION_HIGHLIGHT_ALPHA),
    ];
    let pixels = layout
        .paint(request.viewport, color, highlight)
        .into_iter()
        .map(|pixel| request.order.arrange(pixel))
        .collect();
    let layer = TextLayer {
        request: request.clone(),
        generation,
        pixels,
        caret: layout.caret_pixels(request.viewport),
        caret_color: request.order.arrange(color),
    };
    Some((layer, layout.scroll()))
}

struct TextWorkerState {
    wanted: Option<TextRequest>,
    generation: u64,
    changed_at: Instant,
    ready: Option<Arc<TextLayer>>,
}

struct TextWorker {
    state: Mutex<TextWorkerState>,
    wake: Condvar,
}

fn text_worker() -> Option<&'static TextWorker> {
    static WORKER: OnceLock<Option<&'static TextWorker>> = OnceLock::new();
    *WORKER.get_or_init(|| {
        let worker: &'static TextWorker = Box::leak(Box::new(TextWorker {
            state: Mutex::new(TextWorkerState {
                wanted: None,
                generation: 0,
                changed_at: Instant::now(),
                ready: None,
            }),
            wake: Condvar::new(),
        }));
        match std::thread::Builder::new()
            .name("eclipse-text".to_owned())
            .spawn(move || run_text_worker(worker))
        {
            Ok(_) => Some(worker),
            Err(error) => {
                tracing::error!(%error, "text-overlay: could not start the text layout thread");
                None
            }
        }
    })
}

fn run_text_worker(worker: &TextWorker) {
    let mut built = 0;
    let mut scroll: Option<(i64, Scroll)> = None;
    loop {
        let (request, generation) = {
            let mut state = worker
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while state.generation == built {
                state = worker
                    .wake
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            (state.wanted.clone(), state.generation)
        };
        built = generation;
        let Some(request) = request else {
            continue;
        };
        let previous = scroll
            .filter(|(widget, _)| *widget == request.widget)
            .map_or_else(Scroll::default, |(_, scroll)| scroll);
        let layer = build_text_layer(&request, generation, previous).map(|(layer, next)| {
            scroll = Some((request.widget, next));
            crate::framework::record_text_scroll(request.widget, next);
            Arc::new(layer)
        });
        worker
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ready = layer;
    }
}

pub(super) fn current_text_layer(
    request: &TextRequest,
    now: Instant,
) -> Option<(Arc<TextLayer>, bool)> {
    let worker = text_worker()?;
    let mut state = worker
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.wanted.as_ref() != Some(request) {
        state.wanted = Some(request.clone());
        state.generation += 1;
        state.changed_at = now;
        worker.wake.notify_one();
    }
    let layer = state.ready.clone().filter(|layer| {
        layer.request.widget == request.widget
            && layer.request.viewport == request.viewport
            && layer.request.order == request.order
    })?;
    Some((
        layer,
        caret_visible(now.saturating_duration_since(state.changed_at)),
    ))
}

fn overlay_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_NO_VK_OVERLAY").is_none())
}

pub(super) fn probe_enabled() -> bool {
    static EN: OnceLock<bool> = OnceLock::new();
    *EN.get_or_init(|| std::env::var_os("ECLIPSE_VK_PROBE").is_some())
}

fn screenshot_enabled() -> bool {
    static SHOT: OnceLock<bool> = OnceLock::new();
    *SHOT.get_or_init(|| std::env::var_os("ECLIPSE_VK_SCREENSHOT").is_some())
}

fn test_text() -> Option<&'static str> {
    static TEXT: OnceLock<Option<String>> = OnceLock::new();
    TEXT.get_or_init(|| std::env::var("ECLIPSE_VK_TEXT_TEST").ok())
        .as_deref()
}

pub(super) fn wanted() -> bool {
    probe_enabled()
        || (overlay_enabled()
            && (crate::framework::active_text_field() != 0 || test_text().is_some()))
}

fn login_field_rect(size: SurfaceSize) -> SurfaceRect {
    let x = 181u32.min(size.width.saturating_sub(1));
    let y = 149u32.min(size.height.saturating_sub(1));
    SurfaceRect {
        x: x as i32,
        y: y as i32,
        width: 438u32.min(size.width - x),
        height: 46u32.min(size.height - y),
    }
}

fn resolve_field_rect(
    geometry: Option<(i32, i32, u32, u32)>,
    size: SurfaceSize,
) -> Option<SurfaceRect> {
    let (gx, gy, gw, gh) = geometry?;
    let span = |origin: i32, length: u32, limit: u32| {
        let start = i64::from(origin).max(0);
        let end = (i64::from(origin) + i64::from(length)).min(i64::from(limit));
        (start < end).then(|| (start as i32, (end - start) as u32))
    };
    let (x, width) = span(gx, gw, size.width)?;
    let (y, height) = span(gy, gh, size.height)?;
    Some(SurfaceRect {
        x,
        y,
        width,
        height,
    })
}

fn full_surface_rect(size: SurfaceSize) -> Option<SurfaceRect> {
    (size.width != 0 && size.height != 0).then_some(SurfaceRect {
        x: 0,
        y: 0,
        width: size.width,
        height: size.height,
    })
}

fn select_text_probe_rect(
    geometry: Option<(i32, i32, u32, u32)>,
    size: SurfaceSize,
    drawing_text: bool,
    probing: bool,
    full_screenshot: bool,
) -> Option<SurfaceRect> {
    let live = resolve_field_rect(geometry, size);
    if drawing_text {
        return live;
    }
    if !probing {
        return None;
    }
    if full_screenshot {
        return full_surface_rect(size);
    }
    live.or_else(|| {
        let rect = login_field_rect(size);
        (rect.width != 0 && rect.height != 0).then_some(rect)
    })
}

pub(super) fn write_field_probe(data: &[u8], w: usize, h: usize, order: ByteOrder) {
    let png_rgba: Vec<u8> = data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&pixel| {
            let [r, g, b, _] = order.arrange(pixel);
            [r, g, b, 255]
        })
        .collect();
    let png = encode_png_rgba(&png_rgba, w as u32, h as u32);
    if let Err(error) = std::fs::write("/tmp/eclipse_field_probe.png", png) {
        tracing::warn!(%error, "text-overlay: could not write the field probe image");
    }

    static LOG_TICK: AtomicU64 = AtomicU64::new(0);
    if !LOG_TICK.fetch_add(1, Ordering::Relaxed).is_multiple_of(60) {
        return;
    }
    const BUCKETS: usize = 64;
    let mut col_ink = [0u32; BUCKETS];
    let mut total_ink = 0u32;
    let (y0, y1) = (h * 3 / 10, h * 7 / 10);
    for y in y0..y1 {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let lum = (u32::from(data[i]) + u32::from(data[i + 1]) + u32::from(data[i + 2])) / 3;
            if lum > 90 {
                total_ink += 1;
                col_ink[x * BUCKETS / w] += 1;
            }
        }
    }
    let max = col_ink.iter().copied().max().unwrap_or(1).max(1);
    let levels = [' ', '.', ':', '-', '=', '+', '*', '#', '@'];
    let spark: String = col_ink
        .iter()
        .map(|&c| levels[(c as usize * (levels.len() - 1) / max as usize).min(levels.len() - 1)])
        .collect();
    tracing::info!(total_ink, "text-overlay field-probe ink |{spark}|");
}

fn text_test_overlay(size: SurfaceSize) -> Option<ActiveTextOverlay> {
    let text = test_text()?;
    let rect = login_field_rect(size);
    Some(ActiveTextOverlay {
        widget: 0,
        text: text.to_owned(),
        selection: TextSelection::Cursor(text.encode_utf16().count()),
        geometry: (rect.x, rect.y, rect.width, rect.height),
        input_type: 0,
        font: TEXT_TEST_FONT,
        font_size: 25.0,
        multiline: false,
        text_wrapped: false,
        text_color: -1,
        x_alignment: 0,
        y_alignment: 1,
    })
}

pub(super) struct TextPlan {
    pub(super) overlay: Option<ActiveTextOverlay>,
    pub(super) rect: SurfaceRect,
    pub(super) order: ByteOrder,
}

pub(super) fn plan(size: SurfaceSize, order: ByteOrder) -> Option<TextPlan> {
    let live = if overlay_enabled() {
        crate::framework::active_text_overlay()
    } else {
        None
    };
    let overlay = live.or_else(|| text_test_overlay(size));
    if overlay.is_none() && !probe_enabled() {
        return None;
    }
    let geometry = match &overlay {
        Some(overlay) => Some(overlay.geometry),
        None => crate::framework::textbox_geometry(),
    };
    let rect = select_text_probe_rect(
        geometry,
        size,
        overlay.is_some(),
        probe_enabled(),
        screenshot_enabled(),
    )?;
    Some(TextPlan {
        overlay,
        rect,
        order,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SCREEN: SurfaceSize = SurfaceSize {
        width: 800,
        height: 600,
    };

    fn as_tuple(rect: Option<SurfaceRect>) -> Option<(i32, i32, u32, u32)> {
        rect.map(|rect| (rect.x, rect.y, rect.width, rect.height))
    }

    pub(crate) fn focused_box(text: &str, input_type: i32) -> ActiveTextOverlay {
        ActiveTextOverlay {
            widget: 7,
            text: text.to_string(),
            selection: TextSelection::Cursor(text.encode_utf16().count()),
            geometry: (0, 0, 240, 46),
            input_type,
            font: TEXT_TEST_FONT,
            font_size: 25.0,
            multiline: false,
            text_wrapped: false,
            text_color: -1,
            x_alignment: 0,
            y_alignment: 1,
        }
    }

    pub(crate) fn host_font_available() -> bool {
        let available = crate::host_fonts::system_font().is_some();
        if !available {
            eprintln!("SKIP: no host font for the text overlay");
        }
        available
    }

    pub(crate) fn field_rect(overlay: &ActiveTextOverlay, size: SurfaceSize) -> SurfaceRect {
        resolve_field_rect(Some(overlay.geometry), size).expect("field on screen")
    }

    pub(crate) fn text_layer(
        overlay: &ActiveTextOverlay,
        rect: SurfaceRect,
        order: ByteOrder,
    ) -> TextLayer {
        let request = TextRequest::of(overlay, rect, order);
        build_text_layer(&request, 1, Scroll::default())
            .expect("text layer")
            .0
    }

    pub(crate) fn blend_reference(
        pixels: &mut [u8],
        layer: &TextLayer,
        width: u32,
        caret_on: bool,
    ) {
        let caret = layer.caret.filter(|_| caret_on);
        let caret_ink = crate::text_layout::premultiply(layer.caret_color);
        for (index, (destination, ink)) in pixels
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(&layer.pixels)
            .enumerate()
        {
            let (x, y) = (index as u32 % width, index as u32 / width);
            let mut blended = crate::text_layout::over(*ink, *destination);
            if caret.is_some_and(|rect| x >= rect.x0 && x < rect.x1 && y >= rect.y0 && y < rect.y1)
            {
                blended = crate::text_layout::over(caret_ink, blended);
            }
            blended[3] = destination[3];
            *destination = blended;
        }
    }

    #[test]
    fn resolve_field_rect_draws_nothing_without_a_live_textbox_session() {
        assert_eq!(as_tuple(resolve_field_rect(None, SCREEN)), None);

        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 0, 46)), SCREEN)),
            None
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 0)), SCREEN)),
            None
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 300, 390, 46)), SCREEN)),
            Some((181, 300, 390, 46))
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((181, 149, 438, 46)), SCREEN)),
            Some((181, 149, 438, 46))
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((700, 560, 438, 46)), SCREEN)),
            Some((700, 560, 100, 40))
        );

        assert_eq!(
            as_tuple(resolve_field_rect(Some((-5, -7, 300, 40)), SCREEN)),
            Some((0, 0, 295, 33))
        );
        assert_eq!(
            as_tuple(resolve_field_rect(Some((900, 0, 10, 10)), SCREEN)),
            None
        );
    }

    #[test]
    fn full_frame_probe_never_expands_or_invents_a_text_draw_rect() {
        let live = Some((181, 300, 390, 46));

        assert_eq!(
            as_tuple(select_text_probe_rect(live, SCREEN, true, true, true)),
            Some((181, 300, 390, 46))
        );

        assert_eq!(
            as_tuple(select_text_probe_rect(live, SCREEN, false, true, true)),
            Some((0, 0, 800, 600))
        );

        assert_eq!(
            as_tuple(select_text_probe_rect(None, SCREEN, true, true, true)),
            None
        );
    }

    #[test]
    fn caret_blinks_every_half_second_of_wall_time() {
        for (millis, visible) in [
            (0, true),
            (208, true),
            (499, true),
            (500, false),
            (999, false),
            (1000, true),
            (1500, false),
        ] {
            assert_eq!(
                caret_visible(Duration::from_millis(millis)),
                visible,
                "caret visibility {millis} ms after the last edit"
            );
        }
    }

    #[test]
    fn text_colour_follows_the_byte_order() {
        let color = text_rgba(0x80FF_4020u32 as i32);
        assert_eq!(color, [0xFF, 0x40, 0x20, 0x80]);
        assert_eq!(ByteOrder::Rgba.arrange(color), [0xFF, 0x40, 0x20, 0x80]);
        assert_eq!(ByteOrder::Bgra.arrange(color), [0x20, 0x40, 0xFF, 0x80]);
    }

    #[test]
    fn a_box_partly_off_screen_is_laid_out_at_its_real_origin() {
        let mut overlay = focused_box("hihihi", 0);
        overlay.geometry = (-50, -10, 300, 40);
        let rect = field_rect(&overlay, SCREEN);
        assert_eq!(as_tuple(Some(rect)), Some((0, 0, 250, 30)));
        let request = TextRequest::of(&overlay, rect, ByteOrder::Rgba);
        assert_eq!(
            request.viewport,
            Viewport {
                x: 50,
                y: 10,
                width: 250,
                height: 30
            }
        );
        assert_eq!(
            request.style.width, 300.0,
            "the layout uses the whole box, not the visible part"
        );
        if !host_font_available() {
            return;
        }
        let whole = {
            let mut shown = focused_box("hihihi", 0);
            shown.geometry = (100, 100, 300, 40);
            text_layer(&shown, field_rect(&shown, SCREEN), ByteOrder::Rgba)
        };
        let cropped = text_layer(&overlay, rect, ByteOrder::Rgba);
        for y in 0..30usize {
            for x in 0..250usize {
                assert_eq!(
                    cropped.pixels[y * 250 + x],
                    whole.pixels[(y + 10) * 300 + x + 50],
                    "pixel {x},{y} of the visible part"
                );
            }
        }
    }

    #[test]
    fn an_empty_focused_box_shows_a_caret() {
        if !host_font_available() {
            return;
        }
        let overlay = focused_box("", 0);
        let rect = field_rect(&overlay, SCREEN);
        let layer = text_layer(&overlay, rect, ByteOrder::Rgba);
        let caret = layer.caret.expect("an empty focused box keeps its caret");
        assert!(caret.x0 < 3 && caret.x1 > caret.x0 && caret.y1 > caret.y0);
        assert!(layer.pixels.iter().all(|pixel| pixel[3] == 0));
    }

    #[test]
    fn the_text_worker_builds_layers_off_the_present_thread_and_restarts_the_blink() {
        if !host_font_available() {
            return;
        }
        let overlay = focused_box("worker", 0);
        let rect = field_rect(&overlay, SCREEN);
        let request = TextRequest::of(&overlay, rect, ByteOrder::Rgba);
        let edited_at = Instant::now();
        let started = Instant::now();
        let (layer, caret_on) = loop {
            if let Some(ready) = current_text_layer(&request, edited_at) {
                break ready;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the worker never published a layer"
            );
            std::thread::yield_now();
        };
        assert_eq!(layer.request, request);
        assert!(caret_on, "the caret shows right after an edit");
        assert!(layer.pixels.iter().any(|pixel| pixel[3] != 0));
        let blink_at = |millis: u64| {
            current_text_layer(&request, edited_at + Duration::from_millis(millis))
                .expect("layer")
                .1
        };
        assert!(!blink_at(600), "the caret hides 500 ms after the last edit");
        assert!(blink_at(1000), "and shows again after another 500 ms");
        let edited = TextRequest::of(&focused_box("worker!", 0), rect, ByteOrder::Rgba);
        let (_, after_edit) = current_text_layer(&edited, edited_at + Duration::from_millis(1600))
            .expect("the previous layer stays on screen while the edit is built");
        assert!(
            after_edit,
            "an edit restarts the blink with the caret shown"
        );
    }
}
