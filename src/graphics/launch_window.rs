use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ash::vk;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::error::OsError;
use winit::event::{ElementState, MouseButton, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, AsyncRequestSerial, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents as _, PumpStatus};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;
use winit::platform::startup_notify::{
    self, EventLoopExtStartupNotify as _, WindowAttributesExtStartupNotify as _,
    WindowExtStartupNotify as _,
};
use winit::platform::wayland::{ActiveEventLoopExtWayland as _, WindowAttributesExtWayland as _};
use winit::window::{ActivationToken, Window, WindowId};

use super::activation::Token;
use super::{layout_views, GlyphAtlas, GraphicsError, HostEventLoop, TextMeasure, VulkanRenderer};
use crate::framework::view_registry::{LayoutParams, RenderNode, MATCH_PARENT, WRAP_CONTENT};
use crate::framework::HostWake;
use crate::input::{PrimaryTouch, TouchTracker};
use crate::status::{Progress, StatusUpdate};

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
pub(super) const BUTTON_BACKGROUND: i32 = 0xFFC9_D8F2_u32 as i32;
const LINEAR_LAYOUT: &str = "android.widget.LinearLayout";
const FRAME_LAYOUT: &str = "android.widget.FrameLayout";
const TEXT_VIEW: &str = "android.widget.TextView";
const STARTING: &str = "Starting Roblox…";
const LOG_HINT: &str = "Details are in";
const CLOSE: &str = "Close (Esc)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureHeading {
    CouldNotStart,
    Stopped,
}

impl FailureHeading {
    fn text(self) -> &'static str {
        match self {
            Self::CouldNotStart => "Roblox could not start:",
            Self::Stopped => "Roblox stopped:",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowClosed;

impl std::fmt::Display for WindowClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the Eclipse window was closed before Roblox started")
    }
}

impl std::error::Error for WindowClosed {}

pub enum WindowCommand {
    Raise {
        token: Option<Token>,
        done: Option<Sender<()>>,
    },
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub question: String,
    pub confirm: String,
    pub cancel: String,
}

impl Prompt {
    fn choice(&self, answer: Answer) -> String {
        match answer {
            Answer::Confirm => format!("{} (Enter)", self.confirm),
            Answer::Cancel => format!("{} (Esc)", self.cancel),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    pub answer: Answer,
    pub token: Option<Token>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGone;

#[derive(Clone)]
pub struct WindowControl {
    commands: Sender<WindowCommand>,
    wake: EventLoopProxy<HostWake>,
}

impl WindowControl {
    pub fn send(&self, command: WindowCommand) -> Result<(), WindowGone> {
        self.commands.send(command).map_err(|_| WindowGone)?;
        self.wake
            .send_event(HostWake::Control)
            .map_err(|_| WindowGone)
    }
}

pub(super) fn raise(window: Option<&Window>, token: Option<&Token>, done: Option<Sender<()>>) {
    if let Some(window) = window {
        super::activation::raise(window, token);
    }
    if let Some(done) = done {
        done.send(()).ok();
    }
}

pub struct LaunchWindow {
    event_loop: &'static mut HostEventLoop,
    screen: StatusScreen,
    commands: Sender<WindowCommand>,
    pumping: bool,
}

impl LaunchWindow {
    pub fn open(title: &str) -> Result<Self, GraphicsError> {
        let (commands, received) = mpsc::channel();
        let mut launch = Self {
            event_loop: super::host_event_loop()?,
            screen: StatusScreen::new(title, received),
            commands,
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
            if self.pumping {
                self.pump(Some(POLL_INTERVAL));
            } else {
                self.screen.wait_without_window(POLL_INTERVAL);
            }
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

    pub fn control(&self) -> WindowControl {
        WindowControl {
            commands: self.commands.clone(),
            wake: self.event_loop.create_proxy(),
        }
    }

    pub fn event_loop(
        &mut self,
    ) -> (
        &mut HostEventLoop,
        Option<ActivationToken>,
        &Receiver<WindowCommand>,
    ) {
        let token = self.activation_token();
        self.close();
        (&mut self.event_loop, token, &self.screen.commands)
    }

    pub fn ask(&mut self, prompt: Prompt) -> Result<Answered, WindowClosed> {
        self.screen.content.prompt = Some(prompt);
        self.screen.answer = None;
        self.screen.content_changed();
        let answer = loop {
            if let Some(answer) = self.screen.answer {
                break answer;
            }
            if self.screen.session != Session::Showing || !self.pumping {
                return Err(WindowClosed);
            }
            self.pump(None);
        };
        let token = self.activation_token().and_then(|token| {
            Token::parse(token.into_raw())
                .inspect_err(|error| {
                    tracing::warn!(%error, "the compositor's activation token cannot be passed on");
                })
                .ok()
        });
        self.screen.content.prompt = None;
        Ok(Answered { answer, token })
    }

    pub fn close(&mut self) {
        self.screen.end();
        while self.pumping {
            self.pump(Some(Duration::ZERO));
        }
    }

    fn activation_token(&mut self) -> Option<ActivationToken> {
        if self.screen.request_activation() {
            let deadline = Instant::now() + ACTIVATION_WAIT;
            while self.pumping && self.screen.activation_pending() {
                let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                    break;
                };
                self.pump(Some(left.min(POLL_INTERVAL)));
            }
        }
        self.screen.take_activation_token()
    }

    pub fn show_error(&mut self, heading: FailureHeading, message: &str, log: Option<&Path>) {
        self.screen.content.error = Some(Failure {
            heading,
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
            tracing::warn!(%error, "the window that shows why Roblox failed could not run");
        }
        if let Some(log) = self.screen.log_of_unseen_failure() {
            eprintln!("{LOG_HINT} {}", log.display());
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
enum WindowMapping {
    OnCreate,
    OnFirstFrame,
}

impl WindowMapping {
    fn of(event_loop: &ActiveEventLoop) -> Self {
        if event_loop.is_wayland() {
            Self::OnFirstFrame
        } else {
            Self::OnCreate
        }
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
    commands: Receiver<WindowCommand>,
    answer: Option<Answer>,
    clicks: ClickTracker<Answer>,
}

impl StatusScreen {
    fn new(title: &str, commands: Receiver<WindowCommand>) -> Self {
        Self {
            title: title.to_owned(),
            renderer: None,
            window: None,
            create_error: None,
            content: Content::default(),
            session: Session::Showing,
            scale: 1.0,
            activation: Activation::Unrequested,
            commands,
            answer: None,
            clicks: ClickTracker::default(),
        }
    }

    fn awaiting_answer(&self) -> bool {
        self.content.error.is_none() && self.content.prompt.is_some() && self.answer.is_none()
    }

    fn give(&mut self, answer: Answer) {
        if self.content.error.is_some() {
            self.dismiss();
        } else if self.awaiting_answer() {
            self.answer = Some(answer);
        }
    }

    fn dismiss(&mut self) {
        self.close_window();
        self.session = Session::Dismissed;
    }

    fn wait_without_window(&mut self, timeout: Duration) {
        match self.commands.recv_timeout(timeout) {
            Ok(WindowCommand::Raise {
                done: Some(done), ..
            }) => {
                done.send(()).ok();
            }
            Ok(WindowCommand::Raise { done: None, .. }) | Err(_) => {}
            Ok(WindowCommand::Close) => self.session = Session::Dismissed,
        }
    }

    fn run_commands(&mut self) {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                WindowCommand::Raise { token, done } => {
                    raise(self.window.as_ref(), token.as_ref(), done);
                }
                WindowCommand::Close => self.dismiss(),
            }
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

    fn shows(&self, id: WindowId) -> bool {
        self.window.as_ref().map(Window::id) == Some(id)
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

    fn renderer_unavailable(&mut self, error: &GraphicsError, mapping: WindowMapping) {
        match mapping {
            WindowMapping::OnCreate => {
                tracing::warn!(%error, "the launch window cannot draw; its title shows the status");
            }
            WindowMapping::OnFirstFrame => {
                tracing::warn!(
                    %error,
                    "the launch window cannot draw, and Wayland shows no window before its first \
                     frame; the status goes only to the terminal and the log"
                );
                self.end();
            }
        }
    }

    fn log_of_unseen_failure(&self) -> Option<&Path> {
        if self.session == Session::Dismissed && self.create_error.is_none() {
            return None;
        }
        self.content.error.as_ref()?.log.as_deref()
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
        let extent = match renderer.current_extent(window) {
            Ok(Some(extent)) => extent,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "drawing the launch status failed");
                return;
            }
        };
        let nodes = match renderer.text.as_ref() {
            Some(text) => self.content.nodes(&text.atlas, extent, self.scale),
            None => Vec::new(),
        };
        if let Err(error) = renderer.draw_nodes(window, &nodes) {
            tracing::warn!(%error, "drawing the launch status failed");
        }
    }

    fn handle_window_event(&mut self, id: WindowId, event: WindowEvent) {
        if !self.shows(id) {
            return;
        }
        let target_at = |point| {
            let renderer = self.renderer.as_ref()?;
            let text = renderer.text.as_ref()?;
            self.content
                .answer_at(&text.atlas, renderer.swapchain_extent, self.scale, point)
        };
        if let Some(answer) = self.clicks.window_event(&event, target_at) {
            self.give(answer);
        }
        match event {
            WindowEvent::CloseRequested => {
                self.give(Answer::Cancel);
                self.dismiss();
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                if let Some(answer) = key_answer(&event.logical_key) {
                    self.give(answer);
                }
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
}

impl ApplicationHandler<HostWake> for StatusScreen {
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
            Err(error) => self.renderer_unavailable(&error, WindowMapping::of(event_loop)),
        }
        if self.session != Session::Showing {
            event_loop.exit();
            return;
        }
        let scale = window.scale_factor();
        self.window = Some(window);
        self.scale = 1.0;
        self.rescale(scale);
        self.content_changed();
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        self.handle_window_event(id, event);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.session == Session::Showing {
            self.run_commands();
        }
        if self.session != Session::Showing {
            event_loop.exit();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    heading: FailureHeading,
    message: String,
    log: Option<PathBuf>,
}

fn key_answer(key: &Key) -> Option<Answer> {
    match key {
        Key::Named(NamedKey::Enter) => Some(Answer::Confirm),
        Key::Named(NamedKey::Escape) => Some(Answer::Cancel),
        _ => None,
    }
}

#[derive(Default)]
struct Content {
    step: Option<String>,
    progress: Option<Progress>,
    note: Option<String>,
    warnings: VecDeque<String>,
    error: Option<Failure>,
    prompt: Option<Prompt>,
}

impl Content {
    fn apply(&mut self, update: StatusUpdate) {
        match update {
            StatusUpdate::Step(text) => {
                self.step = Some(text);
                self.progress = None;
            }
            StatusUpdate::Progress(progress) => self.progress = Some(progress),
            StatusUpdate::Note(text) => self.note = Some(text),
            StatusUpdate::Warning(text) => {
                if self.warnings.len() == MAX_WARNINGS {
                    self.warnings.pop_front();
                }
                self.warnings.push_back(text);
            }
        }
    }

    fn showing_prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref().filter(|_| self.error.is_none())
    }

    fn summary(&self) -> String {
        if let Some(prompt) = self.showing_prompt() {
            return format!(
                "{} Enter: {}, Esc: {}",
                prompt.question, prompt.confirm, prompt.cancel
            );
        }
        match &self.error {
            Some(Failure {
                heading,
                message,
                log: Some(log),
            }) => format!(
                "{} {message} ({LOG_HINT} {})",
                heading.text(),
                log.display()
            ),
            Some(Failure {
                heading,
                message,
                log: None,
            }) => format!("{} {message}", heading.text()),
            None => {
                let step = self.step.as_deref().unwrap_or(STARTING);
                match self.progress {
                    Some(progress) => format!("{step} {}", progress.text()),
                    None => step.to_owned(),
                }
            }
        }
    }

    fn nodes(&self, atlas: &GlyphAtlas, extent: vk::Extent2D, scale: f64) -> Vec<RenderNode> {
        self.screen(atlas, extent, scale).nodes
    }

    fn answer_at(
        &self,
        atlas: &GlyphAtlas,
        extent: vk::Extent2D,
        scale: f64,
        point: (f32, f32),
    ) -> Option<Answer> {
        let screen = self.screen(atlas, extent, scale);
        action_at(&screen.nodes, &screen.answers, atlas, extent, point)
    }

    fn screen(&self, atlas: &GlyphAtlas, extent: vk::Extent2D, scale: f64) -> Screen {
        if let Some(prompt) = self.showing_prompt() {
            return Screen::asking(prompt, atlas, extent, scale);
        }
        let mut screen = Screen::new(scale);
        let width = extent.width as i32 - 2 * screen.px(PADDING);
        let gap = screen.px(SECTION_GAP);
        let measure = TextMeasure { atlas };
        match &self.error {
            Some(failure) => {
                screen.paragraph(failure.heading.text(), measure, width, ERROR_BACKGROUND, 0);
                screen.paragraph(&failure.message, measure, width, ERROR_BACKGROUND, 0);
                if let Some(log) = &failure.log {
                    let hint = format!("{LOG_HINT} {}", log.display());
                    screen.paragraph(&hint, measure, width, BACKGROUND, gap);
                }
            }
            None => {
                let step = self.step.as_deref().unwrap_or(STARTING);
                screen.paragraph(step, measure, width, BACKGROUND, 0);
                if let Some(progress) = self.progress {
                    if let Some((done, total)) = progress.bar() {
                        screen.bar(done, total, width);
                    }
                    screen.paragraph(&progress.text(), measure, width, BACKGROUND, 0);
                }
                if let Some(note) = &self.note {
                    screen.paragraph(note, measure, width, BACKGROUND, gap);
                }
            }
        }
        for warning in &self.warnings {
            screen.paragraph(warning, measure, width, WARNING_BACKGROUND, gap);
        }
        if self.error.is_some() {
            screen.button(CLOSE, Answer::Cancel, measure, width);
        }
        screen
    }
}

struct Screen {
    scale: f64,
    nodes: Vec<RenderNode>,
    answers: Vec<Option<Answer>>,
}

impl Screen {
    fn asking(prompt: &Prompt, atlas: &GlyphAtlas, extent: vk::Extent2D, scale: f64) -> Self {
        let mut screen = Self::new(scale);
        let width = extent.width as i32 - 2 * screen.px(PADDING);
        let measure = TextMeasure { atlas };
        screen.paragraph(&prompt.question, measure, width, BACKGROUND, 0);
        for answer in [Answer::Confirm, Answer::Cancel] {
            screen.button(&prompt.choice(answer), answer, measure, width);
        }
        screen
    }

    fn new(scale: f64) -> Self {
        Self {
            scale,
            answers: vec![None],
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
        self.answers.push(None);
        self.nodes[parent].children.push(index);
        index
    }

    fn button(&mut self, label: &str, answer: Answer, measure: TextMeasure<'_>, width: i32) {
        let gap = self.px(SECTION_GAP);
        for (index, line) in wrap(&displayable(label, measure.atlas), measure, width as f32)
            .into_iter()
            .enumerate()
        {
            let layout = LayoutParams {
                width: MATCH_PARENT,
                height: WRAP_CONTENT,
                margins: [0, if index == 0 { gap } else { 0 }, 0, 0],
                ..LayoutParams::default()
            };
            let pushed = self.push(node(TEXT_VIEW, Some(line), 1, layout, BUTTON_BACKGROUND), 0);
            self.answers[pushed] = Some(answer);
        }
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

pub(super) struct ClickTracker<A> {
    cursor: Option<(f32, f32)>,
    pressed: Option<A>,
    fingers: TouchTracker,
}

impl<A> Default for ClickTracker<A> {
    fn default() -> Self {
        Self {
            cursor: None,
            pressed: None,
            fingers: TouchTracker::default(),
        }
    }
}

impl<A: Copy + PartialEq> ClickTracker<A> {
    pub(super) fn window_event(
        &mut self,
        event: &WindowEvent,
        target_at: impl Fn((f32, f32)) -> Option<A>,
    ) -> Option<A> {
        match *event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x as f32, position.y as f32));
                None
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                None
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.button(state, target_at),
            WindowEvent::Touch(touch) => self.touch(
                touch.id,
                touch.phase,
                (touch.location.x as f32, touch.location.y as f32),
                target_at,
            ),
            _ => None,
        }
    }

    fn button(
        &mut self,
        state: ElementState,
        target_at: impl Fn((f32, f32)) -> Option<A>,
    ) -> Option<A> {
        let under_cursor = self.cursor.and_then(target_at);
        match state {
            ElementState::Pressed => {
                self.pressed = under_cursor;
                None
            }
            ElementState::Released => self
                .pressed
                .take()
                .filter(|&pressed| Some(pressed) == under_cursor),
        }
    }

    fn touch(
        &mut self,
        finger: u64,
        phase: TouchPhase,
        (x, y): (f32, f32),
        target_at: impl Fn((f32, f32)) -> Option<A>,
    ) -> Option<A> {
        let mut clicked = None;
        let motions = self.fingers.touch(finger, phase, x, y);
        for primary in motions.filter_map(|motion| motion.primary()) {
            let state = match primary {
                PrimaryTouch::Press { x, y } => {
                    self.cursor = Some((x, y));
                    ElementState::Pressed
                }
                PrimaryTouch::Move { x, y } => {
                    self.cursor = Some((x, y));
                    continue;
                }
                PrimaryTouch::Release { x, y } => {
                    self.cursor = Some((x, y));
                    ElementState::Released
                }
                PrimaryTouch::Cancel => {
                    self.pressed = None;
                    continue;
                }
            };
            clicked = self.button(state, &target_at).or(clicked);
        }
        clicked
    }
}

pub(super) fn action_at<A: Copy>(
    nodes: &[RenderNode],
    actions: &[Option<A>],
    atlas: &GlyphAtlas,
    extent: vk::Extent2D,
    (x, y): (f32, f32),
) -> Option<A> {
    let views = layout_views(nodes, extent, Some(TextMeasure { atlas }));
    views
        .iter()
        .zip(actions)
        .rev()
        .find(|(view, action)| {
            action.is_some()
                && x >= view.x
                && x < view.x + view.w
                && y >= view.y
                && y < view.y + view.h
        })
        .and_then(|(_, action)| *action)
}

pub(super) fn scaled(value: i32, scale: f64) -> i32 {
    (f64::from(value) * scale).round() as i32
}

pub(super) fn node(
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

pub(super) fn displayable(text: &str, atlas: &GlyphAtlas) -> String {
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

pub(super) fn wrap(text: &str, measure: TextMeasure<'_>, width: f32) -> Vec<String> {
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
    use winit::dpi::PhysicalPosition;
    use winit::event::{DeviceId, Touch};

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
        content.apply(StatusUpdate::Progress(Progress::Transfer {
            done: 50 * 1024 * 1024,
            total: Some(100 * 1024 * 1024),
        }));
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

        content.apply(StatusUpdate::Step(
            "Extracting Roblox bundled assets".to_owned(),
        ));
        content.apply(StatusUpdate::Progress(Progress::Extraction {
            done: 60 * 1024 * 1024,
            total: 80 * 1024 * 1024,
        }));
        let nodes = content.nodes(&atlas, extent, 1.0);
        assert_eq!(
            texts(&nodes),
            [
                "Extracting Roblox bundled assets",
                "Prepared 60.0 of 80.0 MiB (75%)"
            ]
        );
        let fill = nodes
            .iter()
            .find(|node| node.background_color == Some(BAR_FILL))
            .expect("a progress bar");
        assert_eq!(fill.layout.width, (800 - 2 * PADDING) * 3 / 4);

        for index in 0..=MAX_WARNINGS {
            content.apply(StatusUpdate::Warning(format!("warning {index}")));
        }
        content.apply(StatusUpdate::Step("Launching Roblox".to_owned()));
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0)),
            ["Launching Roblox", "warning 1", "warning 2", "warning 3"]
        );

        content.error = Some(Failure {
            heading: FailureHeading::CouldNotStart,
            message: "Roblox is not installed".to_owned(),
            log: None,
        });
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0)),
            [
                "Roblox could not start:",
                "Roblox is not installed",
                "warning 1",
                "warning 2",
                "warning 3",
                CLOSE
            ]
        );
        content.error = Some(Failure {
            heading: FailureHeading::CouldNotStart,
            message: "Roblox is not installed".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        });
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0))[..3],
            [
                "Roblox could not start:",
                "Roblox is not installed",
                "Details are in /data/logs/eclipse.log"
            ]
        );
        content.error = Some(Failure {
            heading: FailureHeading::Stopped,
            message: "Roblox crashed (signal 6, SIGABRT: Roblox aborted)".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        });
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0))[..2],
            [
                "Roblox stopped:",
                "Roblox crashed (signal 6, SIGABRT: Roblox aborted)"
            ]
        );
    }

    #[test]
    fn the_latest_note_follows_the_status_above_the_warnings_until_an_error() {
        let atlas = monospace_atlas();
        let extent = vk::Extent2D {
            width: 800,
            height: 600,
        };
        let mut content = Content::default();
        content.apply(StatusUpdate::Step("Checking APKCombo".to_owned()));
        content.apply(StatusUpdate::Note("First run".to_owned()));
        content.apply(StatusUpdate::Warning("could not update Roblox".to_owned()));
        let nodes = content.nodes(&atlas, extent, 1.0);
        assert_eq!(
            texts(&nodes),
            ["Checking APKCombo", "First run", "could not update Roblox"]
        );
        let note = nodes
            .iter()
            .find(|node| node.text.as_deref() == Some("First run"))
            .expect("the note");
        assert_eq!(note.background_color, Some(BACKGROUND));
        assert_eq!(note.layout.margins[1], SECTION_GAP);

        content.apply(StatusUpdate::Note("Settings".to_owned()));
        content.apply(StatusUpdate::Step("Downloading Roblox".to_owned()));
        content.apply(StatusUpdate::Progress(Progress::Transfer {
            done: 50 * 1024 * 1024,
            total: Some(100 * 1024 * 1024),
        }));
        assert_eq!(
            texts(&content.nodes(&atlas, extent, 1.0)),
            [
                "Downloading Roblox",
                "Downloaded 50.0 of 100.0 MiB (50%)",
                "Settings",
                "could not update Roblox"
            ]
        );

        content.error = Some(Failure {
            heading: FailureHeading::CouldNotStart,
            message: "APKCombo is unreachable".to_owned(),
            log: None,
        });
        assert!(!texts(&content.nodes(&atlas, extent, 1.0)).contains(&"Settings"));
    }

    #[test]
    fn spacing_follows_the_display_scale() {
        let atlas = monospace_atlas();
        let extent = vk::Extent2D {
            width: 1600,
            height: 900,
        };
        let mut content = Content::default();
        content.apply(StatusUpdate::Progress(Progress::Transfer {
            done: 1,
            total: Some(2),
        }));
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

    fn prompt() -> Prompt {
        Prompt {
            question: "Leave the current experience?".to_owned(),
            confirm: "Leave and join".to_owned(),
            cancel: "Stay".to_owned(),
        }
    }

    fn asking_screen() -> StatusScreen {
        let mut screen = StatusScreen::new("Eclipse", mpsc::channel().1);
        screen.content.prompt = Some(prompt());
        screen
    }

    const PROMPT_EXTENT: vk::Extent2D = vk::Extent2D {
        width: 800,
        height: 600,
    };

    fn asking_content() -> Content {
        Content {
            prompt: Some(prompt()),
            ..Content::default()
        }
    }

    fn center_of(content: &Content, atlas: &GlyphAtlas, text: &str) -> (f32, f32) {
        let nodes = content.nodes(atlas, PROMPT_EXTENT, 1.0);
        let views = layout_views(&nodes, PROMPT_EXTENT, Some(TextMeasure { atlas }));
        let view = &views[nodes
            .iter()
            .position(|node| node.text.as_deref() == Some(text))
            .expect("a laid-out row")];
        (view.x + view.w / 2.0, view.y + view.h / 2.0)
    }

    fn position((x, y): (f32, f32)) -> PhysicalPosition<f64> {
        PhysicalPosition::new(f64::from(x), f64::from(y))
    }

    fn cursor_at(point: (f32, f32)) -> WindowEvent {
        WindowEvent::CursorMoved {
            device_id: DeviceId::dummy(),
            position: position(point),
        }
    }

    fn left_button(state: ElementState) -> WindowEvent {
        WindowEvent::MouseInput {
            device_id: DeviceId::dummy(),
            state,
            button: MouseButton::Left,
        }
    }

    fn finger(id: u64, phase: TouchPhase, point: (f32, f32)) -> WindowEvent {
        WindowEvent::Touch(Touch {
            device_id: DeviceId::dummy(),
            phase,
            location: position(point),
            force: None,
            id,
        })
    }

    #[test]
    fn the_prompt_shows_the_question_and_both_choices_until_an_error_replaces_it() {
        let atlas = monospace_atlas();
        let mut content = Content::default();
        content.apply(StatusUpdate::Step("Launching Roblox".to_owned()));
        content.prompt = Some(prompt());
        let nodes = content.nodes(&atlas, PROMPT_EXTENT, 1.0);
        assert_eq!(
            texts(&nodes),
            [
                "Leave the current experience?",
                "Leave and join (Enter)",
                "Stay (Esc)"
            ]
        );
        let buttons: Vec<_> = nodes
            .iter()
            .filter(|node| node.background_color == Some(BUTTON_BACKGROUND))
            .collect();
        assert_eq!(buttons.len(), 2);
        assert!(buttons
            .iter()
            .all(|button| button.layout.width == MATCH_PARENT));

        content.error = Some(Failure {
            heading: FailureHeading::CouldNotStart,
            message: "Roblox did not close".to_owned(),
            log: None,
        });
        assert_eq!(
            texts(&content.nodes(&atlas, PROMPT_EXTENT, 1.0))[..2],
            ["Roblox could not start:", "Roblox did not close"]
        );
    }

    #[test]
    fn enter_escape_and_closing_answer_the_prompt_once() {
        assert_eq!(
            key_answer(&Key::Named(NamedKey::Enter)),
            Some(Answer::Confirm)
        );
        assert_eq!(
            key_answer(&Key::Named(NamedKey::Escape)),
            Some(Answer::Cancel)
        );
        assert_eq!(key_answer(&Key::Named(NamedKey::Space)), None);

        let mut screen = asking_screen();
        screen.give(Answer::Confirm);
        screen.give(Answer::Cancel);
        assert_eq!(screen.answer, Some(Answer::Confirm));

        let mut closed = asking_screen();
        closed.give(Answer::Cancel);
        assert_eq!(closed.answer, Some(Answer::Cancel));

        let mut status = StatusScreen::new("Eclipse", mpsc::channel().1);
        status.give(Answer::Confirm);
        assert_eq!(status.answer, None, "only a prompt takes answers");
    }

    #[test]
    fn a_click_answers_only_when_pressed_and_released_on_the_same_choice() {
        let atlas = monospace_atlas();
        let content = asking_content();
        let at = |point| content.answer_at(&atlas, PROMPT_EXTENT, 1.0, point);
        let confirm = center_of(&content, &atlas, "Leave and join (Enter)");
        let cancel = center_of(&content, &atlas, "Stay (Esc)");
        let question = center_of(&content, &atlas, "Leave the current experience?");
        assert_eq!(at(confirm), Some(Answer::Confirm));
        assert_eq!(at(cancel), Some(Answer::Cancel));
        assert_eq!(at(question), None);
        assert_eq!(at((1.0, 1.0)), None);
        assert_eq!(at((799.0, 599.0)), None);

        for (pressed_at, released_at, answer) in [
            (confirm, question, None),
            (question, cancel, None),
            (confirm, cancel, None),
            (confirm, confirm, Some(Answer::Confirm)),
            (cancel, cancel, Some(Answer::Cancel)),
        ] {
            let mut clicks = ClickTracker::default();
            let answers: Vec<_> = [
                cursor_at(pressed_at),
                left_button(ElementState::Pressed),
                cursor_at(released_at),
                left_button(ElementState::Released),
            ]
            .iter()
            .filter_map(|event| clicks.window_event(event, at))
            .collect();
            assert_eq!(
                answers,
                Vec::from_iter(answer),
                "{pressed_at:?} then {released_at:?}"
            );
        }
    }

    #[test]
    fn the_first_finger_answers_like_the_left_button_and_others_are_ignored() {
        use TouchPhase::{Cancelled, Ended, Moved, Started};

        let atlas = monospace_atlas();
        let content = asking_content();
        let at = |point| content.answer_at(&atlas, PROMPT_EXTENT, 1.0, point);
        let confirm = center_of(&content, &atlas, "Leave and join (Enter)");
        let cancel = center_of(&content, &atlas, "Stay (Esc)");
        let question = center_of(&content, &atlas, "Leave the current experience?");

        for (name, events, answer) in [
            (
                "tap",
                vec![finger(3, Started, confirm), finger(3, Ended, confirm)],
                Some(Answer::Confirm),
            ),
            (
                "drag off the choice",
                vec![
                    finger(3, Started, cancel),
                    finger(3, Moved, confirm),
                    finger(3, Ended, confirm),
                ],
                None,
            ),
            (
                "cancelled touch",
                vec![
                    finger(3, Started, confirm),
                    finger(3, Cancelled, confirm),
                    cursor_at(confirm),
                    left_button(ElementState::Released),
                ],
                None,
            ),
            (
                "second finger",
                vec![
                    finger(3, Started, question),
                    finger(4, Started, confirm),
                    finger(4, Ended, confirm),
                    finger(3, Ended, question),
                ],
                None,
            ),
            (
                "first finger under a second",
                vec![
                    finger(3, Started, cancel),
                    finger(4, Started, confirm),
                    finger(4, Ended, confirm),
                    finger(3, Ended, cancel),
                ],
                Some(Answer::Cancel),
            ),
        ] {
            let mut clicks = ClickTracker::default();
            let answers: Vec<_> = events
                .iter()
                .filter_map(|event| clicks.window_event(event, at))
                .collect();
            assert_eq!(answers, Vec::from_iter(answer), "{name}");
        }
    }

    fn failure() -> Failure {
        Failure {
            heading: FailureHeading::Stopped,
            message: "Roblox exited unexpectedly with status 1".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        }
    }

    #[test]
    fn a_failure_offers_a_close_button() {
        let atlas = monospace_atlas();
        let content = Content {
            error: Some(failure()),
            prompt: Some(prompt()),
            ..Content::default()
        };
        let nodes = content.nodes(&atlas, PROMPT_EXTENT, 1.0);
        let close = nodes
            .iter()
            .find(|node| node.text.as_deref() == Some(CLOSE))
            .expect("a close button");
        assert_eq!(close.background_color, Some(BUTTON_BACKGROUND));
        let at = |point| content.answer_at(&atlas, PROMPT_EXTENT, 1.0, point);
        assert_eq!(at(center_of(&content, &atlas, CLOSE)), Some(Answer::Cancel));
        assert_eq!(at(center_of(&content, &atlas, "Roblox stopped:")), None);
    }

    #[test]
    fn escape_enter_or_the_close_button_dismiss_a_shown_failure() {
        for answer in [Answer::Cancel, Answer::Confirm] {
            let mut screen = StatusScreen::new("Eclipse", mpsc::channel().1);
            screen.content.error = Some(failure());
            screen.give(answer);
            assert_eq!(screen.session, Session::Dismissed, "{answer:?}");
            assert_eq!(screen.answer, None, "{answer:?}");
        }
    }

    #[test]
    fn the_title_carries_the_question_and_both_keys_when_text_cannot_be_drawn() {
        let content = Content {
            prompt: Some(prompt()),
            ..Content::default()
        };
        assert_eq!(
            content.summary(),
            "Leave the current experience? Enter: Leave and join, Esc: Stay"
        );
    }

    #[test]
    fn a_window_that_cannot_map_without_drawing_ends_instead_of_waiting_for_a_close() {
        let error = GraphicsError::Vulkan("no physical device".to_owned());
        let mut screen = StatusScreen::new("Eclipse", mpsc::channel().1);
        screen.renderer_unavailable(&error, WindowMapping::OnFirstFrame);
        assert_eq!(screen.session, Session::Ending);

        let mut screen = StatusScreen::new("Eclipse", mpsc::channel().1);
        screen.renderer_unavailable(&error, WindowMapping::OnCreate);
        assert_eq!(screen.session, Session::Showing);
    }

    #[test]
    fn a_launch_without_a_window_still_answers_raises_and_closes() {
        let (commands, received) = mpsc::channel();
        let mut screen = StatusScreen::new("Eclipse", received);
        screen.end();
        let (done, raised) = mpsc::channel();
        commands
            .send(WindowCommand::Raise {
                token: None,
                done: Some(done),
            })
            .unwrap();
        screen.wait_without_window(Duration::from_secs(5));
        assert_eq!(raised.try_recv(), Ok(()));
        screen.wait_without_window(Duration::from_millis(10));
        assert_eq!(screen.session, Session::Ending);
        commands.send(WindowCommand::Close).unwrap();
        screen.wait_without_window(Duration::from_secs(5));
        assert_eq!(screen.session, Session::Dismissed);
    }

    #[test]
    fn a_status_screen_ignores_the_events_of_other_windows() {
        let mut screen = asking_screen();
        screen.handle_window_event(WindowId::from(7), WindowEvent::CloseRequested);
        assert_eq!(screen.answer, None);
        assert_eq!(screen.session, Session::Showing);
    }

    #[test]
    fn a_failure_no_window_showed_names_its_log_for_the_terminal() {
        let failure = Failure {
            heading: FailureHeading::CouldNotStart,
            message: "Roblox is not installed".to_owned(),
            log: Some(PathBuf::from("/data/logs/eclipse.log")),
        };

        let mut unmapped = StatusScreen::new("Eclipse", mpsc::channel().1);
        unmapped.content.error = Some(failure.clone());
        unmapped.renderer_unavailable(
            &GraphicsError::Vulkan("no physical device".to_owned()),
            WindowMapping::OnFirstFrame,
        );
        assert_eq!(
            unmapped.log_of_unseen_failure(),
            Some(Path::new("/data/logs/eclipse.log"))
        );

        let mut closed = StatusScreen::new("Eclipse", mpsc::channel().1);
        closed.content.error = Some(failure);
        closed.session = Session::Dismissed;
        assert_eq!(closed.log_of_unseen_failure(), None);
    }

    #[test]
    fn the_title_carries_the_status_when_text_cannot_be_drawn() {
        let mut content = Content::default();
        assert_eq!(content.summary(), STARTING);
        content.apply(StatusUpdate::Step("Downloading Roblox…".to_owned()));
        content.apply(StatusUpdate::Progress(Progress::Transfer {
            done: 50 * 1024 * 1024,
            total: Some(100 * 1024 * 1024),
        }));
        assert_eq!(
            content.summary(),
            "Downloading Roblox… Downloaded 50.0 of 100.0 MiB (50%)"
        );
        content.error = Some(Failure {
            heading: FailureHeading::CouldNotStart,
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
