use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, NamedKey};
use winit::platform::wayland::WindowAttributesExtWayland as _;
use winit::window::{Window, WindowId};

use super::launch_window::{
    action_at, displayable, node, scaled, wrap, ClickTracker, BUTTON_BACKGROUND,
};
use super::{GlyphAtlas, TextMeasure, VulkanRenderer};
use crate::framework::dialogs::{dispatch_dialog_action, DialogAction};
use crate::framework::view_registry::{self, LayoutParams, RenderNode, MATCH_PARENT, WRAP_CONTENT};
use crate::framework::window_registry::{DialogView, WindowHandle};
use crate::runtime::Vm;

const WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(560.0, 360.0);
const PADDING: i32 = 24;
const GAP: i32 = 12;
const BACKGROUND: i32 = 0xFFF3_F5F8_u32 as i32;
const ITEM_BACKGROUND: i32 = 0xFFE4_E8EF_u32 as i32;
const LINEAR_LAYOUT: &str = "android.widget.LinearLayout";
const TEXT_VIEW: &str = "android.widget.TextView";
const DEFAULT_TITLE: &str = "Roblox";

#[derive(Debug, Clone, PartialEq)]
struct DialogContent {
    title: String,
    message: Option<String>,
    body: Vec<RenderNode>,
    items: Vec<String>,
    buttons: Vec<(view_registry::ViewHandle, String)>,
}

impl DialogContent {
    fn of(view: &DialogView) -> Self {
        let body = view
            .root_view
            .map(view_registry::snapshot_subtree)
            .filter(|nodes| nodes.len() > 1)
            .unwrap_or_default();
        let buttons = view
            .buttons
            .iter()
            .filter_map(|&button| {
                let label = view_registry::with_view(button, |state| state.text.clone())
                    .ok()
                    .flatten()?;
                (!label.is_empty()).then_some((button, label))
            })
            .collect();
        Self {
            title: view.title.clone(),
            message: view.message.clone(),
            body,
            items: view.items.clone(),
            buttons,
        }
    }

    fn window_title(&self) -> String {
        let title = if self.title.is_empty() {
            DEFAULT_TITLE
        } else {
            &self.title
        };
        crate::window_title(title)
    }

    fn nodes(
        &self,
        atlas: &GlyphAtlas,
        width: u32,
        scale: f64,
    ) -> (Vec<RenderNode>, Vec<Option<DialogAction>>) {
        let mut sheet = Sheet::new(scale);
        let measure = TextMeasure { atlas };
        let width = i32::try_from(width).unwrap_or(i32::MAX) - 2 * scaled(PADDING, scale);
        if !self.title.is_empty() {
            sheet.paragraph(&self.title, measure, width, BACKGROUND, None);
        }
        if let Some(message) = &self.message {
            sheet.paragraph(message, measure, width, BACKGROUND, None);
        }
        sheet.graft(&self.body);
        for (index, item) in self.items.iter().enumerate() {
            sheet.paragraph(
                item,
                measure,
                width,
                ITEM_BACKGROUND,
                Some(DialogAction::Item(index)),
            );
        }
        for &(button, ref label) in &self.buttons {
            sheet.paragraph(
                label,
                measure,
                width,
                BUTTON_BACKGROUND,
                Some(DialogAction::Click(button)),
            );
        }
        (sheet.nodes, sheet.actions)
    }

    fn action_at(
        &self,
        atlas: &GlyphAtlas,
        extent: ash::vk::Extent2D,
        scale: f64,
        point: (f32, f32),
    ) -> Option<DialogAction> {
        let (nodes, actions) = self.nodes(atlas, extent.width, scale);
        action_at(&nodes, &actions, atlas, extent, point)
    }
}

struct Sheet {
    scale: f64,
    nodes: Vec<RenderNode>,
    actions: Vec<Option<DialogAction>>,
}

impl Sheet {
    fn new(scale: f64) -> Self {
        let root = node(
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
        );
        Self {
            scale,
            nodes: vec![root],
            actions: vec![None],
        }
    }

    fn push(&mut self, child: RenderNode, action: Option<DialogAction>) {
        let index = self.nodes.len();
        self.nodes.push(child);
        self.actions.push(action);
        self.nodes[0].children.push(index);
    }

    fn paragraph(
        &mut self,
        text: &str,
        measure: TextMeasure<'_>,
        width: i32,
        background: i32,
        action: Option<DialogAction>,
    ) {
        let gap = scaled(GAP, self.scale);
        for (line_index, line) in wrap(&displayable(text, measure.atlas), measure, width as f32)
            .into_iter()
            .enumerate()
        {
            let layout = LayoutParams {
                width: if action.is_some() {
                    MATCH_PARENT
                } else {
                    WRAP_CONTENT
                },
                height: WRAP_CONTENT,
                margins: [0, if line_index == 0 { gap } else { 0 }, 0, 0],
                ..LayoutParams::default()
            };
            self.push(node(TEXT_VIEW, Some(line), 1, layout, background), action);
        }
    }

    fn graft(&mut self, body: &[RenderNode]) {
        if body.is_empty() {
            return;
        }
        let offset = self.nodes.len();
        for original in body {
            let mut grafted = original.clone();
            grafted.depth += 1;
            grafted.children = original
                .children
                .iter()
                .map(|child| child + offset)
                .collect();
            if grafted.background_color.is_none() {
                grafted.background_color = Some(if original.clickable {
                    BUTTON_BACKGROUND
                } else {
                    BACKGROUND
                });
            }
            let action = original
                .clickable
                .then_some(DialogAction::Click(original.handle));
            self.nodes.push(grafted);
            self.actions.push(action);
        }
        self.nodes[0].children.push(offset);
    }
}

struct OpenDialog {
    handle: WindowHandle,
    content: DialogContent,
    renderer: Option<VulkanRenderer>,
    window: Window,
    scale: f64,
    clicks: ClickTracker<DialogAction>,
}

impl OpenDialog {
    fn open(event_loop: &ActiveEventLoop, view: &DialogView) -> Option<Self> {
        let content = DialogContent::of(view);
        let attributes = Window::default_attributes()
            .with_title(content.window_title())
            .with_name(crate::APP_ID, "eclipse")
            .with_inner_size(WINDOW_SIZE);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                tracing::error!(%error, dialog = view.handle, "the dialog window could not be created");
                return None;
            }
        };
        let renderer = match VulkanRenderer::new(&window) {
            Ok(renderer) => Some(renderer),
            Err(error) => {
                tracing::error!(%error, dialog = view.handle, "the dialog window cannot draw");
                None
            }
        };
        let mut dialog = Self {
            handle: view.handle,
            content,
            window,
            renderer,
            scale: 1.0,
            clicks: ClickTracker::default(),
        };
        dialog.rescale(dialog.window.scale_factor());
        dialog.window.request_redraw();
        Some(dialog)
    }

    fn rescale(&mut self, scale: f64) {
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        if let Some(renderer) = self.renderer.as_mut() {
            if let Err(error) = renderer.set_text_scale(scale) {
                tracing::warn!(%error, scale, "the dialog keeps its text at the previous size");
            }
        }
    }

    fn nodes(&self) -> Option<Vec<RenderNode>> {
        let renderer = self.renderer.as_ref()?;
        let text = renderer.text.as_ref()?;
        let width = renderer.swapchain_extent.width;
        let (nodes, _) = self.content.nodes(&text.atlas, width, self.scale);
        Some(nodes)
    }

    fn draw(&mut self) {
        let Some(nodes) = self.nodes() else {
            return;
        };
        if let Some(renderer) = self.renderer.as_mut() {
            if let Err(error) = renderer.draw_nodes(&self.window, &nodes) {
                tracing::warn!(%error, dialog = self.handle, "drawing the dialog failed");
            }
        }
    }

    fn window_event(&mut self, vm: &Vm, event: WindowEvent) {
        let target_at = |point| {
            let renderer = self.renderer.as_ref()?;
            let text = renderer.text.as_ref()?;
            self.content
                .action_at(&text.atlas, renderer.swapchain_extent, self.scale, point)
        };
        if let Some(action) = self.clicks.window_event(&event, target_at) {
            self.dispatch(vm, action);
        }
        match event {
            WindowEvent::CloseRequested => self.dispatch(vm, DialogAction::Cancel),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                self.dispatch(vm, DialogAction::Cancel);
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.mark_resized(size.width, size.height);
                }
                self.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.rescale(scale_factor);
                self.window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            _ => {}
        }
    }

    fn dispatch(&self, vm: &Vm, action: DialogAction) {
        if let Err(error) = dispatch_dialog_action(vm, self.handle, action) {
            tracing::warn!(%error, dialog = self.handle, ?action, "the dialog action failed");
        }
    }
}

#[derive(Default)]
pub(super) struct DialogWindows {
    open: Vec<OpenDialog>,
}

impl DialogWindows {
    pub(super) fn sync(&mut self, event_loop: &ActiveEventLoop, showing: &[DialogView]) {
        self.open
            .retain(|dialog| showing.iter().any(|view| view.handle == dialog.handle));
        for view in showing {
            match self
                .open
                .iter_mut()
                .find(|dialog| dialog.handle == view.handle)
            {
                Some(dialog) => {
                    let content = DialogContent::of(view);
                    if content != dialog.content {
                        dialog.window.set_title(&content.window_title());
                        dialog.content = content;
                        dialog.window.request_redraw();
                    }
                }
                None => self.open.extend(OpenDialog::open(event_loop, view)),
            }
        }
    }

    pub(super) fn owns(&self, id: WindowId) -> bool {
        self.open.iter().any(|dialog| dialog.window.id() == id)
    }

    pub(super) fn window_event(&mut self, vm: &Vm, id: WindowId, event: WindowEvent) {
        if let Some(dialog) = self.open.iter_mut().find(|dialog| dialog.window.id() == id) {
            dialog.window_event(vm, event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{layout_views, GlyphInfo};
    use winit::dpi::PhysicalPosition;
    use winit::event::{DeviceId, Touch, TouchPhase};

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

    fn content() -> DialogContent {
        DialogContent {
            title: "Disconnected".to_owned(),
            message: Some("You were kicked from this experience".to_owned()),
            body: Vec::new(),
            items: vec!["Report".to_owned()],
            buttons: vec![(41, "Leave".to_owned())],
        }
    }

    #[test]
    fn the_sheet_shows_the_title_message_items_and_buttons_in_order() {
        let atlas = monospace_atlas();
        let (nodes, actions) = content().nodes(&atlas, 1120, 1.0);
        let texts: Vec<_> = nodes
            .iter()
            .filter_map(|node| node.text.as_deref())
            .collect();
        assert_eq!(
            texts,
            [
                "Disconnected",
                "You were kicked from this experience",
                "Report",
                "Leave"
            ]
        );
        assert_eq!(
            actions.iter().flatten().copied().collect::<Vec<_>>(),
            [DialogAction::Item(0), DialogAction::Click(41)]
        );
    }

    #[test]
    fn a_click_on_a_button_row_names_that_button() {
        let atlas = monospace_atlas();
        let extent = ash::vk::Extent2D {
            width: 1120,
            height: 720,
        };
        let (nodes, actions) = content().nodes(&atlas, extent.width, 1.0);
        let views = layout_views(&nodes, extent, Some(TextMeasure { atlas: &atlas }));
        let leave = nodes
            .iter()
            .position(|node| node.text.as_deref() == Some("Leave"))
            .expect("the Leave row");
        let center = (
            views[leave].x + views[leave].w / 2.0,
            views[leave].y + views[leave].h / 2.0,
        );
        assert_eq!(
            action_at(&nodes, &actions, &atlas, extent, center),
            Some(DialogAction::Click(41))
        );
        assert_eq!(
            action_at(&nodes, &actions, &atlas, extent, (1.0, 1.0)),
            None
        );
    }

    #[test]
    fn a_tap_on_a_button_row_dispatches_that_button() {
        let atlas = monospace_atlas();
        let extent = ash::vk::Extent2D {
            width: 1120,
            height: 720,
        };
        let content = content();
        let (nodes, _) = content.nodes(&atlas, extent.width, 1.0);
        let views = layout_views(&nodes, extent, Some(TextMeasure { atlas: &atlas }));
        let center_of = |text: &str| {
            let view = &views[nodes
                .iter()
                .position(|node| node.text.as_deref() == Some(text))
                .expect("a laid-out row")];
            PhysicalPosition::new(
                f64::from(view.x + view.w / 2.0),
                f64::from(view.y + view.h / 2.0),
            )
        };
        let finger = |phase, location| {
            WindowEvent::Touch(Touch {
                device_id: DeviceId::dummy(),
                phase,
                location,
                force: None,
                id: 9,
            })
        };
        let target_at = |point| content.action_at(&atlas, extent, 1.0, point);
        for (row, action) in [
            ("Leave", Some(DialogAction::Click(41))),
            ("Report", Some(DialogAction::Item(0))),
            ("Disconnected", None),
        ] {
            let mut clicks = ClickTracker::default();
            let tap = [
                finger(TouchPhase::Started, center_of(row)),
                finger(TouchPhase::Ended, center_of(row)),
            ];
            let actions: Vec<_> = tap
                .iter()
                .filter_map(|event| clicks.window_event(event, target_at))
                .collect();
            assert_eq!(actions, Vec::from_iter(action), "{row}");
        }
    }

    #[test]
    fn dialog_windows_carry_the_eclipse_title_prefix() {
        assert_eq!(content().window_title(), "Eclipse — Disconnected");
        let untitled = DialogContent {
            title: String::new(),
            ..content()
        };
        assert_eq!(untitled.window_title(), "Eclipse — Roblox");
    }
}
