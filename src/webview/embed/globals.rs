use wayland_client::backend::protocol::Interface;
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_data_device_manager::WlDataDeviceManager, wl_output::WlOutput,
    wl_seat::WlSeat, wl_shm::WlShm, wl_subcompositor::WlSubcompositor,
};
use wayland_client::Proxy;
use wayland_protocols::wp::commit_timing::v1::client::wp_commit_timing_manager_v1::WpCommitTimingManagerV1;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::fifo::v1::client::wp_fifo_manager_v1::WpFifoManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::wp::linux_drm_syncobj::v1::client::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1;
use wayland_protocols::wp::pointer_gestures::zv1::client::zwp_pointer_gestures_v1::ZwpPointerGesturesV1;
use wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation;
use wayland_protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_manager_v1::ZwpPrimarySelectionDeviceManagerV1;
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::shell::client::xdg_wm_base::XdgWmBase;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Provision {
    Forwarded,
    Emulated,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Global {
    pub(super) name: u32,
    pub(super) interface: &'static Interface,
    pub(super) version: u32,
    pub(super) provision: Provision,
}

pub(super) const WM_BASE_VERSION: u32 = 6;

fn offers() -> [(&'static Interface, u32, Provision); 19] {
    use Provision::{Emulated, Forwarded};
    [
        (WlCompositor::interface(), 6, Forwarded),
        (WlSubcompositor::interface(), 1, Forwarded),
        (WlShm::interface(), 2, Forwarded),
        (WlSeat::interface(), 9, Forwarded),
        (WlOutput::interface(), 4, Forwarded),
        (WlDataDeviceManager::interface(), 3, Forwarded),
        (ZwpLinuxDmabufV1::interface(), 5, Forwarded),
        (WpViewporter::interface(), 1, Forwarded),
        (WpFractionalScaleManagerV1::interface(), 1, Forwarded),
        (WpCursorShapeManagerV1::interface(), 2, Forwarded),
        (WpPresentation::interface(), 2, Forwarded),
        (WpSinglePixelBufferManagerV1::interface(), 1, Forwarded),
        (
            ZwpPrimarySelectionDeviceManagerV1::interface(),
            1,
            Forwarded,
        ),
        (ZwpPointerGesturesV1::interface(), 3, Forwarded),
        (WpLinuxDrmSyncobjManagerV1::interface(), 1, Forwarded),
        (WpFifoManagerV1::interface(), 1, Forwarded),
        (WpCommitTimingManagerV1::interface(), 1, Forwarded),
        (ZwpTextInputManagerV3::interface(), 1, Forwarded),
        (XdgWmBase::interface(), WM_BASE_VERSION, Emulated),
    ]
}

pub(super) const REQUIRED: [&str; 5] = [
    "wl_compositor",
    "wl_subcompositor",
    "wl_shm",
    "wl_data_device_manager",
    "xdg_wm_base",
];

pub(super) fn offer(name: u32, interface: &str, version: u32) -> Option<Global> {
    let (interface, cap, provision) = offers()
        .into_iter()
        .find(|(offered, _, _)| offered.name == interface)?;
    let version = version.min(cap).min(interface.version);
    (version > 0).then_some(Global {
        name,
        interface,
        version,
        provision,
    })
}

pub(super) fn missing_required<'a>(
    advertised: impl Iterator<Item = &'a str> + Clone,
) -> Option<&'static str> {
    REQUIRED
        .into_iter()
        .find(|required| !advertised.clone().any(|name| name == *required))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayland_client::backend::protocol::ArgumentType;

    #[test]
    fn the_helper_sees_only_allowed_globals_capped_at_tested_versions() {
        let upstream = [
            (1, "wl_compositor", 6),
            (2, "wl_seat", 10),
            (3, "xdg_wm_base", 7),
            (4, "zxdg_exporter_v2", 1),
            (5, "xdg_activation_v1", 1),
            (6, "zxdg_decoration_manager_v1", 1),
            (7, "zwp_tablet_manager_v2", 1),
            (8, "wl_drm", 2),
            (9, "wl_output", 4),
            (10, "wl_output", 2),
            (11, "xdg_wm_dialog_v1", 1),
            (12, "zwp_linux_dmabuf_v1", 4),
        ];
        let offered: Vec<(u32, &str, u32, Provision)> = upstream
            .into_iter()
            .filter_map(|(name, interface, version)| offer(name, interface, version))
            .map(|global| {
                (
                    global.name,
                    global.interface.name,
                    global.version,
                    global.provision,
                )
            })
            .collect();
        assert_eq!(
            offered,
            [
                (1, "wl_compositor", 6, Provision::Forwarded),
                (2, "wl_seat", 9, Provision::Forwarded),
                (3, "xdg_wm_base", 6, Provision::Emulated),
                (9, "wl_output", 4, Provision::Forwarded),
                (10, "wl_output", 2, Provision::Forwarded),
                (12, "zwp_linux_dmabuf_v1", 4, Provision::Forwarded),
            ]
        );
        assert!(offer(13, "wl_compositor", 0).is_none());
    }

    #[test]
    fn embedding_needs_the_globals_gtk_cannot_start_without() {
        let all = REQUIRED.to_vec();
        assert_eq!(missing_required(all.iter().copied()), None);
        let without_subsurfaces: Vec<&str> = all
            .iter()
            .copied()
            .filter(|name| *name != "wl_subcompositor")
            .collect();
        assert_eq!(
            missing_required(without_subsurfaces.iter().copied()),
            Some("wl_subcompositor")
        );
    }

    #[test]
    fn every_forwarded_object_argument_names_its_interface() {
        let mut stack: Vec<&'static Interface> = offers()
            .into_iter()
            .filter(|(_, _, provision)| *provision == Provision::Forwarded)
            .map(|(interface, _, _)| interface)
            .collect();
        let mut visited = Vec::new();
        while let Some(current) = stack.pop() {
            if visited.contains(&current.name) {
                continue;
            }
            visited.push(current.name);
            for request in current.requests {
                let objects = request
                    .signature
                    .iter()
                    .filter(|argument| matches!(argument, ArgumentType::Object(_)))
                    .count();
                assert_eq!(
                    request.arg_interfaces.len(),
                    objects,
                    "{}.{}",
                    current.name,
                    request.name
                );
                for argument in request.arg_interfaces {
                    assert_ne!(
                        argument.name, "<anonymous>",
                        "{}.{} takes an untyped object, which the proxy cannot check",
                        current.name, request.name
                    );
                }
            }
            let children = current.requests.iter().chain(current.events);
            stack.extend(children.filter_map(|message| message.child_interface));
        }
        assert!(visited.contains(&"wl_surface"), "{visited:?}");
    }
}
