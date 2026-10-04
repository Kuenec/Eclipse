use wayland_client::delegate_noop;
use wayland_protocols::wp::content_type::v1::client::wp_content_type_manager_v1::WpContentTypeManagerV1;
use wayland_protocols::wp::content_type::v1::client::wp_content_type_v1::{Type, WpContentTypeV1};
use winit::window::Window;

use super::wayland_window::{RequestError, SurfaceRequests, WaylandWindow};

delegate_noop!(SurfaceRequests: WpContentTypeManagerV1);
delegate_noop!(SurfaceRequests: WpContentTypeV1);

pub(super) fn mark_as_game(window: &Window) {
    let target = match WaylandWindow::of(window) {
        Ok(Some(target)) => target,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "the compositor is not told that the game window shows a game");
            return;
        }
    };
    match mark_surface_as_game(&target) {
        Ok(()) => tracing::info!("told the compositor that the game window shows a game"),
        Err(error @ RequestError::Absent(_)) => {
            tracing::info!(%error, "the compositor is not told that the game window shows a game");
        }
        Err(error) => {
            tracing::warn!(%error, "the compositor is not told that the game window shows a game");
        }
    }
}

fn mark_surface_as_game(target: &WaylandWindow<'_>) -> Result<(), RequestError> {
    target.send(1..=1, |manager: &WpContentTypeManagerV1, surface, queue| {
        manager
            .get_surface_content_type(surface, queue, ())
            .set_content_type(Type::Game);
        manager.destroy();
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::fake_compositor::{FakeCompositor, Request};
    use wayland_client::protocol::{wl_display, wl_registry, wl_surface};
    use wayland_protocols::wp::content_type::v1::client::{
        wp_content_type_manager_v1, wp_content_type_v1,
    };

    const GAME: u32 = 3;

    fn calls(requests: &[Request]) -> Vec<(&str, u16)> {
        requests.iter().map(Request::call).collect()
    }

    fn game_window(compositor: &FakeCompositor) -> WaylandWindow<'_> {
        let (display, window) = compositor.handles();
        unsafe { WaylandWindow::from_handles_that_outlive_it(display, window) }
            .expect("the fake compositor hands out Wayland handles")
    }

    #[test]
    fn the_game_surface_gets_one_game_content_type_after_one_roundtrip() {
        let mut compositor = FakeCompositor::offering(&[("wp_content_type_manager_v1", 1)]);
        mark_surface_as_game(&game_window(&compositor)).expect("a content-type hint");
        let requests = compositor.requests_since_last_call();
        assert_eq!(
            calls(&requests),
            [
                ("wl_display", wl_display::REQ_GET_REGISTRY_OPCODE),
                ("wl_display", wl_display::REQ_SYNC_OPCODE),
                ("wl_registry", wl_registry::REQ_BIND_OPCODE),
                (
                    "wp_content_type_manager_v1",
                    wp_content_type_manager_v1::REQ_GET_SURFACE_CONTENT_TYPE_OPCODE
                ),
                (
                    "wp_content_type_v1",
                    wp_content_type_v1::REQ_SET_CONTENT_TYPE_OPCODE
                ),
                (
                    "wp_content_type_manager_v1",
                    wp_content_type_manager_v1::REQ_DESTROY_OPCODE
                ),
            ]
        );
        let [content_type, surface] = requests[3].words[..] else {
            panic!("get_surface_content_type takes a new id and a surface");
        };
        assert_eq!(surface, compositor.surface_id());
        assert_eq!(
            (requests[4].object, requests[4].words.as_slice()),
            (content_type, [GAME].as_slice())
        );
    }

    #[test]
    fn a_compositor_without_content_types_gets_no_request_and_the_same_one_roundtrip() {
        let mut compositor = FakeCompositor::offering(&[]);
        let error = mark_surface_as_game(&game_window(&compositor)).expect_err("no content type");
        assert!(
            matches!(error, RequestError::Absent("wp_content_type_manager_v1")),
            "{error}"
        );
        assert_eq!(
            calls(&compositor.requests_since_last_call()),
            [
                ("wl_display", wl_display::REQ_GET_REGISTRY_OPCODE),
                ("wl_display", wl_display::REQ_SYNC_OPCODE),
            ]
        );
    }

    #[test]
    fn the_game_keeps_its_surface_after_the_hint() {
        let mut compositor = FakeCompositor::offering(&[("wp_content_type_manager_v1", 1)]);
        mark_surface_as_game(&game_window(&compositor)).expect("a content-type hint");
        compositor.requests_since_last_call();
        compositor.commit_surface();
        assert_eq!(
            calls(&compositor.requests_since_last_call()),
            [("wl_surface", wl_surface::REQ_COMMIT_OPCODE)]
        );
    }
}
