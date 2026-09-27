use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ash::vk;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::error::OsError;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, AsyncRequestSerial};
use winit::platform::pump_events::{EventLoopExtPumpEvents as _, PumpStatus};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;
use winit::platform::startup_notify::{
    self, EventLoopExtStartupNotify as _, WindowAttributesExtStartupNotify as _,
    WindowExtStartupNotify as _,
};
use winit::platform::wayland::WindowAttributesExtWayland as _;
use winit::window::{ActivationToken, Window, WindowId};

use super::{GlyphAtlas, GraphicsError, HostEventLoop, TextMeasure, VulkanRenderer};
use crate::framework::view_registry::{LayoutParams, RenderNode, MATCH_PARENT, WRAP_CONTENT};
use crate::framework::MainLooperWake;
use crate::status::{transfer_text, StatusUpdate};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const ACTIVATION_WAIT: Duration = Duration::from_millis(500);
const WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(820.0, 460.0);
const PADDING: i32 = 32;
const SECTION_GAP: i32 = 16;
const BAR_HEIGHT: i32 = 14;
const MAX_WARNINGS: usize = 3;
const BACKGROUND: i32 = 0xFFF3_F5F8_u32 as i32;
const BAR_TRACK: i32 = 0xFFD5_DBE5_u32 as i32;
const BAR_FILL: i32 = 0xFF2F_6FD6_u32 as i32;
const WARNING_BACKGROUND: i32 = 0xFFFF_F2CC_u32 as i32;
const ERROR_BACKGROUND: i32 = 0xFFFD_E2E1_u32 as i32;
const LINEAR_LAYOUT: &str = "android.widget.LinearLayout";
const FRAME_LAYOUT: &str = "android.widget.FrameLayout";
const TEXT_VIEW: &str = "android.widget.TextView";
const STARTING: &str = "Starting Roblox…";
const ERROR_HEADING: &str = "Roblox could not start:";
const LOG_HINT: &str = "Details are in";
const CLOSE_HINT: &str = "Close this window to exit.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowClosed;

impl std::fmt::Display for WindowClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Eclipse window was closed before Roblox started")
    }
}

impl std::error::Error for WindowClosed {}

pub struct LaunchWindow {
    event_loop: HostEventLoop,
    screen: StatusScreen,
    pumping: bool,
}

impl LaunchWindow {
    pub fn open(title: &str) -> Result<Self, GraphicsError> {
        let mut launch = Self {
            event_loop: super::host_event_loop()?,
            screen: StatusScreen::new(title),
            pumping: false,
        };
        launch.pump(Some(Duration::ZERO));
        match launch.screen.create_error.take() {
            Some(error) => Err(GraphicsError::CreateWindow(error)),
            None => Ok(launch),
        }
    }

    pub fn wait_for<T>(
        &mut self,
        updates: &Receiver<StatusUpdate>,
        work: &JoinHandle<T>,
    ) -> Result<(), WindowClosed> {
        loop {
            self.screen.apply_all(updates);
            if self.screen.session == Session::Dismissed {
                return Err(WindowClosed);
            }
            if work.is_finished() {
                self.screen.apply_all(updates);
                return Ok(());
            }
            self.pump(Some(POLL_INTERVAL));
        }
    }

    pub fn refresh(&mut self, updates: &Receiver<StatusUpdate>) -> Result<(), WindowClosed> {
        self.screen.apply_all(updates);
        self.pump(Some(Duration::ZERO));
        match self.screen.session {
            Session::Dismissed => Err(WindowClosed),
            Session::Showing | Session::Ending => Ok(()),
        }
    }

    pub fn event_loop(&mut self) -> (&mut HostEventLoop, Option<ActivationToken>) {
        if self.screen.request_activation() {
            let deadline = Instant::now() + ACTIVATION_WAIT;
            while self.pumping && self.screen.activation_pending() {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                self.pump(Some(left.min(POLL_INTERVAL)));
            }
        }
        let token = self.screen.take_activation_token();
        self.screen.end();
        while self.pumping {
            self.pump(Some(Duration::ZERO));
        }
        (&mut self.event_loop, token)
    }

    pub fn show_error(&mut self, message: &str, log: Option<&Path>) {
        self.screen.content.error = Some(Failure {
            message: message.to_owned(),
            log: log.map(Path::to_path_buf),
        });
        self.screen.content_changed();
        if self.screen.session == Session::Dismissed {
            return;
        }
        if self.pumping {
            while self.pumping {
                self.pump(None);
            }
            return;
        }
        self.screen.session = Session::Showing;
        if let Err(error) = self.event_loop.run_app_on_demand(&mut self.screen) {
            tracing::warn!(%error, "the window that shows why Roblox could not start failed");
        }
    }

    fn pump(&mut self, timeout: Option<Duration>) {
        self.pumping = match self.event_loop.pump_app_events(timeout, &mut self.screen) {
            PumpStatus::Continue => true,
            PumpStatus::Exit(_) => false,
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    Showing,
    Ending,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Activation {
    Unrequested,
    Pending(AsyncRequestSerial),
    Granted(ActivationToken),
}

struct StatusScreen {
    title: String,
    renderer: Option<VulkanRenderer>,
    window: Option<Window>,
    create_error: Option<OsError>,
    content: Content,
    session: Session,
    scale: f64,
    activation: Activation,
}

impl StatusScreen {
    fn new(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            renderer: None,
            window: None,
            create_error: None,
            content: Content::default(),
            session: Session::Showing,
            scale: 1.0,
            activation: Activation::Unrequested,
        }
    }

    fn apply_all(&mut self, updates: &Receiver<StatusUpdate>) {
        let mut changed = false;
        for update in updates.try_iter() {
            self.content.apply(update);
            changed = true;
        }
        if changed {
            self.content_changed();
        }
    }

    fn content_changed(&self) {
        let Some(window) = &self.window else {
            return;
        };
        if !self.draws_text() {
            window.set_title(&format!("{} — {}", self.title, self.content.summary()));
        }
        window.request_redraw();
    }

    fn draws_text(&self) -> bool {
        self.renderer
            .as_ref()
            .is_some_and(|renderer| renderer.text.is_some())
    }

    fn request_activation(&mut self) -> bool {
        let Some(window) = &self.window else {
            return false;
        };
        let Ok(serial) = window.request_activation_token() else {
            return false;
        };
        self.activation = Activation::Pending(serial);
        true
    }

    fn activation_pending(&self) -> bool {
        matches!(self.activation, Activation::Pending(_))
    }

    fn take_activation_token(&mut self) -> Option<ActivationToken> {
        match std::mem::replace(&mut self.activation, Activation::Unrequested) {
            Activation::Granted(token) => Some(token),
            Activation::Unrequested | Activation::Pending(_) => None,
        }
    }

    fn end(&mut self) {
        self.close_window();
        if self.session == Session::Showing {
            self.session = Session::Ending;
        }
    }

    fn close_window(&mut self) {
        self.renderer = None;
        self.window = None;
    }

    fn rescale(&mut self, scale: f64) {
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        if let Err(error) = renderer.set_text_scale(scale) {
            tracing::warn!(%error, scale, "the launch window keeps its text at the previous size");
        }
    }

    fn draw(&mut self) {
        let (Some(window), Some(renderer)) = (self.window.as_ref(), self.renderer.as_mut()) else {
            return;
        };
        let extent = renderer.swapchain_extent;
        let nodes = match renderer.text.as_ref() {
            Some(text) => self.content.nodes(&text.atlas, extent, self.scale),
            None => Vec::new(),
        };
        if let Err(error) = renderer.draw_nodes(window, &nodes) {
            tracing::warn!(%error, "drawing the launch status failed");
        }
    }
}

impl ApplicationHandler<MainLooperWake> for StatusScreen {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.session != Session::Showing {
            return;
        }
        let mut attributes = Window::default_attributes()
            .with_title(self.title.clone())
            .with_name(crate::APP_ID, "eclipse")
            .with_inner_size(WINDOW_SIZE);
        if let Some(token) = event_loop.read_token_from_env() {
            startup_notify::reset_activation_token_env();
            attributes = attributes.with_activation_token(token);
        }
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                self.create_error = Some(error);
                self.session = Session::Dismissed;
                event_loop.exit();
                return;
            }
        };
        match VulkanRenderer::new(&window) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                tracing::warn!(%error, "the launch window cannot draw; its title shows the status")
            }
        }
        let scale = window.scale_factor();
        self.window = Some(window);
        self.scale = 1.0;
        self.rescale(scale);
        self.content_changed();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.close_window();
                self.session = Session::Dismissed;
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.mark_resized(size.width, size.height);
                }
                self.content_changed();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.rescale(scale_factor);
                self.content_changed();
            }
            WindowEvent::ActivationTokenDone { serial, token } => {
                if self.activation == Activation::Pending(serial) {
                    self.activation = Activation::Granted(token);
                }
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.session != Session::Showing {
            event_loop.exit();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    message: String,
    log: Option<PathBuf>,
}

#[derive(Default)]
struct Content {
    step: Option<String>,
    transfer: Option<(u64, Option<u64>)>,
    warnings: VecDeque<String>,
    error: Option<Failure>,
}

impl Content {
    fn apply(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Step(text) => {
                self.step = Some(text);
                self.transfer = None;
            }
            StatusUpdate::Transfer { done, total } => self.transfer = Some((done, total)),
            StatusUpdate::Warning(text) => {
                if self.warnings.len() == MAX_WARNINGS {
                    self.warnings.pop_front();
                }
                self.warnings.push_back(text);
            }
        }
    }

    fn summary(&self) -> String {
        match &self.error {
            Some(Failure {
                message,
                log: Some(log),
            }) => format!("{ERROR_HEADING} {message} ({LOG_HINT} {})", log.display()),
            Some(Failure { message, log: None }) => format!("{ERROR_HEADING} {message}"),
            None => {
                let step = self.step.as_deref().unwrap_or(STARTING);
                match self.transfer {
                    Some((done, total)) => format!("{step} {}", transfer_text(done, total)),
                    None => step.to_owned(),
                }
            }
        }
    }

    fn nodes(&self, atlas: &GlyphAtlas, extent: vk::Extent2D, scale: f64) -> Vec<RenderNode> {
        let mut screen = Screen::new(scale);
        let width = extent.width as i32 - 2 * screen.px(PADDING);
        let gap = screen.px(SECTION_GAP);
        let measure = TextMeasure { atlas };
        match &self.error {
            Some(failure) => {
                screen.paragraph(ERROR_HEADING, measure, width, ERROR_BACKGROUND, 0);
                screen.paragraph(&failure.message, measure, width, ERROR_BACKGROUND, 0);
                if let Some(log) = &failure.log {
                    let hint = format!("{LOG_HINT} {}", log.display());
                    screen.paragraph(&hint, measure, width, BACKGROUND, gap);
                }
                screen.paragraph(CLOSE_HINT, measure, width, BACKGROUND, gap);
            }
            None => {
                let step = self.step.as_deref().unwrap_or(STARTING);
                screen.paragraph(step, measure, width, BACKGROUND, 0);
                if let Some((done, total)) = self.transfer {
                    if let Some(total) = total.filter(|&total| total > 0) {
                        screen.bar(done, total, width);
                    }
                    screen.paragraph(&transfer_text(done, total), measure, width, BACKGROUND, 0);
                }
            }
        }
        for warning in &self.warnings {
            screen.paragraph(warning, measure, width, WARNING_BACKGROUND, gap);
        }
        screen.nodes
    }
}

struct Screen {
    scale: f64,
    nodes: Vec<RenderNode>,
}

impl Screen {
    fn new(scale: f64) -> Self {
        Self {
            scale,
            nodes: vec![node(
                LINEAR_LAYOUT,
                None,
                0,
                LayoutParams {
                    width: MATCH_PARENT,
                    height: MATCH_PARENT,
                    padding: [scaled(PADDING, scale); 4],
                    ..LayoutParams::default()
                },
                BACKGROUND,
            )],
        }
    }

    fn px(&self, value: i32) -> i32 {
        scaled(value, self.scale)
    }

    fn push(&mut self, child: RenderNode, parent: usize) -> usize {
        let index = self.nodes.len();
        self.nodes.push(child);
        self.nodes[parent].children.push(index);
        index
    }

    fn paragraph(
        &mut self,
        text: &str,
        measure: TextMeasure<'_>,
        width: i32,
        background: i32,
        gap: i32,
    ) {
        for (index, line) in wrap(&displayable(text, measure.atlas), measure, width as f32)
            .into_iter()
            .enumerate()
        {
            let margins = [0, if index == 0 { gap } else { 0 }, 0, 0];
            let layout = LayoutParams {
                width: WRAP_CONTENT,
                height: WRAP_CONTENT,
                margins,
                ..LayoutParams::default()
            };
            self.push(node(TEXT_VIEW, Some(line), 1, layout, background), 0);
        }
    }

    fn bar(&mut self, done: u64, total: u64, width: i32) {
        let (height, gap) = (self.px(BAR_HEIGHT), self.px(SECTION_GAP));
        let track = self.push(
            node(
                FRAME_LAYOUT,
                None,
                1,
                LayoutParams {
                    width: width.max(1),
                    height,
                    margins: [0, gap, 0, gap / 2],
                    ..LayoutParams::default()
                },
                BAR_TRACK,
            ),
            0,
        );
        let filled =
            (u128::from(done.min(total)) * width.max(0) as u128 / u128::from(total)) as i32;
        self.push(
            node(
                FRAME_LAYOUT,
                None,
                2,
                LayoutParams {
                    width: filled,
                    height,
                    ..LayoutParams::default()
                },
                BAR_FILL,
            ),
            track,
        );
    }
}

fn scaled(value: i32, scale: f64) -> i32 {
    (f64::from(value) * scale).round() as i32
}

fn node(
    class: &str,
    text: Option<String>,
    depth: u32,
    layout: LayoutParams,
    background: i32,
) -> RenderNode {
    RenderNode {
        handle: 0,
        class_name: class.to_owned(),
        text,
        depth,
        layout,
        clickable: false,
        background_color: Some(background),
        children: Vec::new(),
    }
}

fn displayable(text: &str, atlas: &GlyphAtlas) -> String {
    let mut shown = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            _ if ch == ' ' || atlas.glyphs.contains_key(&ch) => shown.push(ch),
            '\n' | '\t' => shown.push(' '),
            '…' => shown.push_str("..."),
            '—' | '–' | '→' => shown.push('-'),
            '‘' | '’' => shown.push('\''),
            '“' | '”' => shown.push('"'),
            '✓' => {}
            _ => shown.push('?'),
        }
    }
    shown
}

fn wrap(text: &str, measure: TextMeasure<'_>, width: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|word| !word.is_empty()) {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if measure.width(&candidate) <= width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        for ch in word.chars() {
            line.push(ch);
            if measure.width(&line) > width && line.chars().count() > 1 {
                line.pop();
                lines.push(std::mem::replace(&mut line, ch.to_string()));
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::GlyphInfo;

    fn monospace_atlas() -> GlyphAtlas {
        let glyph = GlyphInfo {
            ax: 0,
            ay: 0,
            aw: 1,
            ah: 1,
            bearing_x: 0.0,
            bearing_y: 0.0,
            advance: 10.0,
        };
        GlyphAtlas {
            width: 1,
            height: 1,
            pixels: vec![0],
            glyphs: (32u8..=126).map(|byte| (char::from(byte), glyph)).collect(),
            ascent: 20.0,
            line_height: 30.0,
        }
    }

    fn texts(nodes: &[RenderNode]) -> Vec<&str> {
        nodes
            .iter()
            .filter_map(|node| node.text.as_deref())
            .collect()
    }

    #[test]
    fn long_messages_wrap_at_words_and_split_words_wider_than_a_line() {
        let atlas = monospace_atlas();
        let measure = TextMeasure { atlas: &atlas };
        let width = measure.width("0123456789");
        assert_eq!(
            wrap("could not update Roblox", measure, width),
            ["could not", "update", "Roblox"]
        );
        assert_eq!(
            wrap("/a/very/long/path", measure, width),
            ["/a/very/lo", "ng/path"]
        );
        assert_eq!(wrap("", measure, width), [""]);
    }

    #[test]
    fn characters_the_font_lacks_are_shown_as_ascii() {
        let atlas = monospace_atlas();
        assert_eq!(
            displayable("Checking APKCombo… ✓ — “ok” é", &atlas),
            "Checking APKCombo...  - \"ok\" ?"
        );
    }

    #[test]
    fn the_screen_shows_the_step_progress_warnings_and_errors() {
        let atlas = monospace_atlas();
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let mut content = Content::default();
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0)),
            ["Starting Roblox..."]
        );

        content.apply(StatusUpdate::Step("Downloading Roblox".to_owned()));
        content.apply(StatusUpdate::Transfer {
            done: 50 * 1024 * 1024,
            total: Some(100 * 1024 * 1024),
        });
        let nodes = content.nodes(&atlas, extent, 1.0);
        assert_eq!(
            texts(&nodes),
            ["Downloading Roblox", "Downloaded 50.0 of 100.0 MiB (50%)"]
        );
        let fill = nodes
            .iter()
            .find(|node| node.background_color == Some(BAR_FILL))
            .expect("a progress bar");
        assert_eq!(fill.layout.width, (800 - 2 * PADDING) / 2);

        for index in 0..=MAX_WARNINGS {
            content.apply(StatusUpdate::Warning(format!("warning {index}")));
        }
        content.apply(StatusUpdate::Step("Launching Roblox".to_owned()));
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0)),
            ["Launching Roblox", "warning 1", "warning 2", "warning 3"]
        );

        content.error = Some(Failure {
            message: "Roblox is not installed".to_owned(),
            log: None,
        });
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0))[..3],
            [ERROR_HEADING, "Roblox is not installed", CLOSE_HINT]
        );
        content.error = Some(Failure {
            message: "Roblox is not installed".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        });
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0))[..4],
            [
                ERROR_HEADING,
                "Roblox is not installed",
                "Details are in /data/logs/eclipse.log",
                CLOSE_HINT
            ]
        );
    }

    #[test]
    fn spacing_follows_the_display_scale() {
        let atlas = monospace_atlas();
        let extent = vk::Extent2D {
            width: 1600,
            height: 900,
        };
        let mut content = Content::default();
        content.apply(StatusUpdate::Transfer {
            done: 1,
            total: Some(2),
        });
        for (scale, padding) in [(1.0, PADDING), (2.0, 2 * PADDING), (1.5, 48)] {
            let nodes = content.nodes(&atlas, extent, scale);
            assert_eq!(nodes[0].layout.padding, [padding; 4], "{scale}");
            let track = nodes
                .iter()
                .find(|node| node.background_color == Some(BAR_TRACK))
                .expect("a progress bar");
            assert_eq!(track.layout.width, 1600 - 2 * padding, "{scale}");
            assert_eq!(track.layout.height, scaled(BAR_HEIGHT, scale), "{scale}");
        }
    }

    #[test]
    fn the_title_carries_the_status_when_text_cannot_be_drawn() {
        let mut content = Content::default();
        assert_eq!(content.summary(), STARTING);
        content.apply(StatusUpdate::Step("Downloading Roblox…".to_owned()));
        content.apply(StatusUpdate::Transfer {
            done: 50 * 1024 * 1024,
            total: Some(100 * 1024 * 1024),
        });
        assert_eq!(
            content.summary(),
            "Downloading Roblox… Downloaded 50.0 of 100.0 MiB (50%)"
        );
        content.error = Some(Failure {
            message: "APKCombo is unreachable".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        });
        assert_eq!(
            content.summary(),
            "Roblox could not start: APKCombo is unreachable (Details are in \
             /data/logs/eclipse.log)"
        );
    }
}
