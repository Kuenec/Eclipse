use std::collections::HashMap;

use wayland_client::backend::protocol::{Argument, MessageDesc};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_subsurface;
use wayland_client::Proxy;
use wayland_protocols::xdg::shell::client::{
    xdg_popup::{self, XdgPopup},
    xdg_positioner::{self, XdgPositioner},
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base,
};

use super::super::positioner::{Adjustment, Edge, Positioner, Rect};
use super::super::{outgoing, Upstream};
use super::{violation, Kind, Request, Session, SurfaceLink, SurfaceRole, Violation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    WmBase,
    Positioner,
    XdgSurface,
    Toplevel,
    Popup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellRole {
    Toplevel(u32),
    Popup(u32),
}

struct ShellSurface {
    surface: SurfaceLink,
    role: Option<ShellRole>,
    pending_geometry: Option<Rect>,
    geometry: Option<Rect>,
    configured: bool,
    capabilities_sent: bool,
    subsurface: Option<(u64, ObjectId)>,
    position: Option<(i32, i32)>,
    mapped: Option<u64>,
}

struct Popup {
    xdg_surface: u32,
    parent: u32,
    placed: Rect,
    grab: bool,
    positioner: Positioner,
}

#[derive(Default)]
pub(super) struct Shell {
    positioners: HashMap<u32, Positioner>,
    surfaces: HashMap<u32, ShellSurface>,
    toplevels: HashMap<u32, u32>,
    popups: HashMap<u32, Popup>,
    configure_serial: u32,
    maps: u64,
}

const TILED_STATES_SINCE: u32 = 2;

const UNBOUNDED: Rect = Rect {
    x: i32::MIN / 4,
    y: i32::MIN / 4,
    width: i32::MAX / 2,
    height: i32::MAX / 2,
};

impl Session {
    pub(super) fn shell_request(
        &mut self,
        up: &Upstream,
        role: Role,
        request: Request,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let id = request.object;
        let object = &self.objects[&id];
        let (interface, version) = (object.interface, object.version);
        let fail = |reason| violation(id, interface, desc, reason);
        let args = request.args.as_slice();
        match (role, request.opcode, args) {
            (Role::WmBase, xdg_wm_base::REQ_DESTROY_OPCODE, _) => self.forget(id),
            (Role::WmBase, xdg_wm_base::REQ_PONG_OPCODE, _) => {}
            (Role::WmBase, xdg_wm_base::REQ_CREATE_POSITIONER_OPCODE, [Argument::NewId(new)]) => {
                self.adopt(
                    *new,
                    XdgPositioner::interface(),
                    version,
                    Kind::Shell(Role::Positioner),
                )
                .map_err(fail)?;
                self.shell.positioners.insert(*new, Positioner::default());
            }
            (
                Role::WmBase,
                xdg_wm_base::REQ_GET_XDG_SURFACE_OPCODE,
                [Argument::NewId(new), Argument::Object(surface)],
            ) => {
                let state = self
                    .surfaces
                    .get(surface)
                    .ok_or_else(|| fail("a surface the game's connection does not hold"))?;
                if !matches!(state.role, SurfaceRole::None | SurfaceRole::Xdg)
                    || state.xdg_surface.is_some()
                {
                    return Err(fail("a surface that already has a role"));
                }
                let link = SurfaceLink {
                    surface: *surface,
                    instance: state.instance,
                };
                self.adopt(
                    *new,
                    XdgSurface::interface(),
                    version,
                    Kind::Shell(Role::XdgSurface),
                )
                .map_err(fail)?;
                if let Some(state) = self.surfaces.get_mut(surface) {
                    state.role = SurfaceRole::Xdg;
                    state.xdg_surface = Some(*new);
                }
                self.shell.surfaces.insert(
                    *new,
                    ShellSurface {
                        surface: link,
                        role: None,
                        pending_geometry: None,
                        geometry: None,
                        configured: false,
                        capabilities_sent: false,
                        subsurface: None,
                        position: None,
                        mapped: None,
                    },
                );
            }
            (Role::Positioner, xdg_positioner::REQ_DESTROY_OPCODE, _) => {
                self.shell.positioners.remove(&id);
                self.forget(id);
            }
            (Role::Positioner, opcode, args) => {
                let positioner = self
                    .shell
                    .positioners
                    .get_mut(&id)
                    .ok_or_else(|| fail("a positioner without state"))?;
                set_positioner(positioner, opcode, args).map_err(fail)?;
            }
            (Role::XdgSurface, xdg_surface::REQ_DESTROY_OPCODE, _) => {
                self.shell_surface_destroyed(up, id);
                self.forget(id);
            }
            (Role::XdgSurface, xdg_surface::REQ_GET_TOPLEVEL_OPCODE, [Argument::NewId(new)]) => {
                self.check_roleless(id).map_err(fail)?;
                self.adopt(
                    *new,
                    XdgToplevel::interface(),
                    version,
                    Kind::Shell(Role::Toplevel),
                )
                .map_err(fail)?;
                self.shell.toplevels.insert(*new, id);
                self.give_role(up, id, ShellRole::Toplevel(*new))
                    .map_err(fail)?;
            }
            (
                Role::XdgSurface,
                xdg_surface::REQ_GET_POPUP_OPCODE,
                [Argument::NewId(new), Argument::Object(parent), Argument::Object(positioner)],
            ) => {
                self.check_roleless(id).map_err(fail)?;
                let parent_has_role = self
                    .shell
                    .surfaces
                    .get(parent)
                    .is_some_and(|parent| parent.role.is_some());
                if *parent == id || !parent_has_role {
                    return Err(fail("a popup without a parent window"));
                }
                let positioner = self
                    .shell
                    .positioners
                    .get(positioner)
                    .copied()
                    .ok_or_else(|| fail("an unknown positioner"))?;
                if positioner.size.0 <= 0 || positioner.size.1 <= 0 {
                    return Err(fail("a positioner without a size"));
                }
                self.adopt(
                    *new,
                    XdgPopup::interface(),
                    version,
                    Kind::Shell(Role::Popup),
                )
                .map_err(fail)?;
                self.shell.popups.insert(
                    *new,
                    Popup {
                        xdg_surface: id,
                        parent: *parent,
                        placed: Rect::default(),
                        grab: false,
                        positioner,
                    },
                );
                self.give_role(up, id, ShellRole::Popup(*new))
                    .map_err(fail)?;
            }
            (
                Role::XdgSurface,
                xdg_surface::REQ_SET_WINDOW_GEOMETRY_OPCODE,
                [Argument::Int(x), Argument::Int(y), Argument::Int(width), Argument::Int(height)],
            ) => {
                if *width <= 0 || *height <= 0 {
                    return Err(fail("an empty window geometry"));
                }
                if let Some(state) = self.shell.surfaces.get_mut(&id) {
                    state.pending_geometry = Some(Rect {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                    });
                }
            }
            (Role::XdgSurface, xdg_surface::REQ_ACK_CONFIGURE_OPCODE, _) => {}
            (Role::Toplevel, xdg_toplevel::REQ_DESTROY_OPCODE, _) => {
                if let Some(xdg) = self.shell.toplevels.remove(&id) {
                    self.role_ended(up, xdg);
                }
                self.forget(id);
                self.sync_focus();
            }
            (Role::Toplevel, _, _) => {}
            (Role::Popup, xdg_popup::REQ_DESTROY_OPCODE, _) => {
                if let Some(popup) = self.shell.popups.remove(&id) {
                    self.role_ended(up, popup.xdg_surface);
                }
                self.forget(id);
                self.sync_focus();
            }
            (Role::Popup, xdg_popup::REQ_GRAB_OPCODE, _) => {
                if let Some(popup) = self.shell.popups.get_mut(&id) {
                    popup.grab = true;
                }
                self.sync_focus();
            }
            (
                Role::Popup,
                xdg_popup::REQ_REPOSITION_OPCODE,
                [Argument::Object(positioner), Argument::Uint(token)],
            ) => {
                let positioner = self
                    .shell
                    .positioners
                    .get(positioner)
                    .copied()
                    .ok_or_else(|| fail("an unknown positioner"))?;
                if let Some(popup) = self.shell.popups.get_mut(&id) {
                    popup.positioner = positioner;
                }
                self.configure_popup(id, Some(*token));
                self.place_all(up);
            }
            _ => return Err(fail("unexpected arguments")),
        }
        Ok(())
    }

    fn check_roleless(&self, xdg: u32) -> Result<(), &'static str> {
        match self.shell.surfaces.get(&xdg) {
            Some(state) if state.role.is_none() && self.surface_alive(state.surface) => Ok(()),
            Some(_) => Err("an xdg_surface that already has a role or lost its surface"),
            None => Err("an unknown xdg_surface"),
        }
    }

    fn give_role(&mut self, up: &Upstream, xdg: u32, role: ShellRole) -> Result<(), &'static str> {
        let Some(state) = self.shell.surfaces.get(&xdg) else {
            return Err("an unknown xdg_surface");
        };
        let surface = self
            .upstream_id(state.surface.surface)
            .filter(|_| self.surface_alive(state.surface))
            .ok_or("an xdg_surface whose surface is gone")?;
        let subsurface = up
            .create_role_subsurface(&surface)
            .map_err(|_| "the game window is gone")?;
        let instance = self.next_instance();
        self.owned.insert(instance, subsurface.clone());
        if let Some(state) = self.shell.surfaces.get_mut(&xdg) {
            state.role = Some(role);
            state.configured = false;
            state.position = None;
            state.mapped = None;
            state.subsurface = Some((instance, subsurface));
        }
        Ok(())
    }

    fn role_ended(&mut self, up: &Upstream, xdg: u32) {
        let Some(state) = self.shell.surfaces.get_mut(&xdg) else {
            return;
        };
        state.role = None;
        state.configured = false;
        state.mapped = None;
        state.position = None;
        if let Some((instance, subsurface)) = state.subsurface.take() {
            up.destroy(&subsurface);
            self.owned.remove(&instance);
        }
    }

    fn shell_surface_destroyed(&mut self, up: &Upstream, xdg: u32) {
        self.role_ended(up, xdg);
        let Some(state) = self.shell.surfaces.remove(&xdg) else {
            return;
        };
        if let Some(surface) = self.surfaces.get_mut(&state.surface.surface) {
            if surface.instance == state.surface.instance && surface.xdg_surface == Some(xdg) {
                surface.xdg_surface = None;
            }
        }
        self.sync_focus();
    }

    pub(super) fn surface_destroying(&mut self, up: &Upstream, surface: u32) {
        let Some(xdg) = self
            .surfaces
            .get(&surface)
            .and_then(|state| state.xdg_surface)
        else {
            return;
        };
        self.role_ended(up, xdg);
        self.sync_focus();
    }

    pub(super) fn committed(&mut self, up: &Upstream, surface: u32) {
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return;
        };
        let attached = state.attached.take();
        let Some(xdg) = state.xdg_surface else {
            return;
        };
        let Some(state) = self.shell.surfaces.get_mut(&xdg) else {
            return;
        };
        let mut moved = false;
        if let Some(geometry) = state.pending_geometry.take() {
            moved = state.geometry != Some(geometry);
            state.geometry = Some(geometry);
        }
        let Some(role) = state.role else {
            return;
        };
        let mapping_changed = match attached {
            Some(true) if state.mapped.is_none() && state.configured => {
                self.shell.maps += 1;
                state.mapped = Some(self.shell.maps);
                true
            }
            Some(false) if state.mapped.is_some() => {
                state.mapped = None;
                true
            }
            _ => false,
        };
        let configured = state.configured;
        if !configured {
            match role {
                ShellRole::Toplevel(toplevel) => self.configure_toplevel(toplevel),
                ShellRole::Popup(popup) => self.configure_popup(popup, None),
            }
        }
        if moved {
            self.place_all(up);
        } else {
            self.place(up, xdg);
        }
        if mapping_changed {
            self.sync_focus();
        }
    }

    fn geometry_origin(&self, xdg: u32) -> (i32, i32) {
        let (mut x, mut y) = (0, 0);
        let mut current = xdg;
        for _ in 0..=self.shell.popups.len() {
            let popup = match self
                .shell
                .surfaces
                .get(&current)
                .and_then(|state| state.role)
            {
                Some(ShellRole::Popup(popup)) => self.shell.popups.get(&popup),
                _ => None,
            };
            let Some(popup) = popup else {
                return (x, y);
            };
            x += popup.placed.x;
            y += popup.placed.y;
            current = popup.parent;
        }
        (x, y)
    }

    fn place(&mut self, up: &Upstream, xdg: u32) {
        let origin = self.geometry_origin(xdg);
        let Some(state) = self.shell.surfaces.get_mut(&xdg) else {
            return;
        };
        let Some((_, subsurface)) = &state.subsurface else {
            return;
        };
        let offset = state
            .geometry
            .map_or((0, 0), |geometry| (geometry.x, geometry.y));
        let position = (origin.0 - offset.0, origin.1 - offset.1);
        if state.position == Some(position) {
            return;
        }
        let _ = up.backend.send_request(
            outgoing(
                subsurface.clone(),
                wl_subsurface::REQ_SET_POSITION_OPCODE,
                vec![Argument::Int(position.0), Argument::Int(position.1)],
            ),
            None,
            None,
        );
        state.position = Some(position);
    }

    fn place_all(&mut self, up: &Upstream) {
        let placed: Vec<u32> = self.shell.surfaces.keys().copied().collect();
        for xdg in placed {
            self.place(up, xdg);
        }
    }

    pub(super) fn surface_position(&self, surface: u32) -> (i32, i32) {
        self.surfaces
            .get(&surface)
            .and_then(|state| state.xdg_surface)
            .and_then(|xdg| self.shell.surfaces.get(&xdg)?.position)
            .unwrap_or((0, 0))
    }

    fn next_configure_serial(&mut self) -> u32 {
        self.shell.configure_serial = self.shell.configure_serial.wrapping_add(1);
        self.shell.configure_serial
    }

    fn configure_toplevel(&mut self, toplevel: u32) {
        let Some(&xdg) = self.shell.toplevels.get(&toplevel) else {
            return;
        };
        let Some(version) = self.objects.get(&toplevel).map(|object| object.version) else {
            return;
        };
        let (width, height) = self.size.unwrap_or((0, 0));
        if version >= xdg_toplevel::EVT_CONFIGURE_BOUNDS_SINCE {
            self.send(
                toplevel,
                xdg_toplevel::EVT_CONFIGURE_BOUNDS_OPCODE,
                vec![Argument::Int(width), Argument::Int(height)],
            );
        }
        let send_capabilities = version >= xdg_toplevel::EVT_WM_CAPABILITIES_SINCE
            && self
                .shell
                .surfaces
                .get(&xdg)
                .is_some_and(|state| !state.capabilities_sent);
        if send_capabilities {
            self.send(
                toplevel,
                xdg_toplevel::EVT_WM_CAPABILITIES_OPCODE,
                vec![Argument::Array(Box::default())],
            );
        }
        let mut states = vec![xdg_toplevel::State::Maximized];
        if self.input.game_focused() {
            states.push(xdg_toplevel::State::Activated);
        }
        if version >= TILED_STATES_SINCE {
            states.extend([
                xdg_toplevel::State::TiledLeft,
                xdg_toplevel::State::TiledRight,
                xdg_toplevel::State::TiledTop,
                xdg_toplevel::State::TiledBottom,
            ]);
        }
        let states: Vec<u8> = states
            .into_iter()
            .flat_map(|state| (state as u32).to_ne_bytes())
            .collect();
        self.send(
            toplevel,
            xdg_toplevel::EVT_CONFIGURE_OPCODE,
            vec![
                Argument::Int(width),
                Argument::Int(height),
                Argument::Array(Box::new(states)),
            ],
        );
        let serial = self.next_configure_serial();
        self.send(
            xdg,
            xdg_surface::EVT_CONFIGURE_OPCODE,
            vec![Argument::Uint(serial)],
        );
        if let Some(state) = self.shell.surfaces.get_mut(&xdg) {
            state.configured = true;
            state.capabilities_sent |= send_capabilities;
        }
    }

    pub(super) fn configure_toplevels(&mut self) {
        let configured: Vec<u32> = self
            .shell
            .toplevels
            .iter()
            .filter(|(_, xdg)| {
                self.shell
                    .surfaces
                    .get(xdg)
                    .is_some_and(|state| state.configured)
            })
            .map(|(toplevel, _)| *toplevel)
            .collect();
        for toplevel in configured {
            self.configure_toplevel(toplevel);
        }
    }

    fn configure_popup(&mut self, popup: u32, token: Option<u32>) {
        let Some(state) = self.shell.popups.get(&popup) else {
            return;
        };
        let (xdg, parent, positioner) = (state.xdg_surface, state.parent, state.positioner);
        let origin = self.geometry_origin(parent);
        let bounds = self.size.map_or(UNBOUNDED, |(width, height)| Rect {
            x: -origin.0,
            y: -origin.1,
            width,
            height,
        });
        let placed = positioner.place(bounds);
        if let Some(state) = self.shell.popups.get_mut(&popup) {
            state.placed = placed;
        }
        if let Some(token) = token {
            self.send(
                popup,
                xdg_popup::EVT_REPOSITIONED_OPCODE,
                vec![Argument::Uint(token)],
            );
        }
        self.send(
            popup,
            xdg_popup::EVT_CONFIGURE_OPCODE,
            vec![
                Argument::Int(placed.x),
                Argument::Int(placed.y),
                Argument::Int(placed.width),
                Argument::Int(placed.height),
            ],
        );
        let serial = self.next_configure_serial();
        self.send(
            xdg,
            xdg_surface::EVT_CONFIGURE_OPCODE,
            vec![Argument::Uint(serial)],
        );
        if let Some(state) = self.shell.surfaces.get_mut(&xdg) {
            state.configured = true;
        }
    }

    pub(super) fn keyboard_target(&self) -> Option<u32> {
        let mapped = |xdg: u32| {
            let state = self.shell.surfaces.get(&xdg)?;
            Some((state.mapped?, state.surface.surface))
        };
        let popup = self
            .shell
            .popups
            .values()
            .filter(|popup| popup.grab)
            .filter_map(|popup| mapped(popup.xdg_surface))
            .max_by_key(|(order, _)| *order);
        popup
            .or_else(|| {
                self.shell
                    .toplevels
                    .values()
                    .filter_map(|xdg| mapped(*xdg))
                    .max_by_key(|(order, _)| *order)
            })
            .map(|(_, surface)| surface)
    }

    pub(super) fn grabbing_popups(&self) -> bool {
        self.shell.popups.values().any(|popup| popup.grab)
    }

    pub(super) fn dismiss_popups(&mut self, pressed: Option<u32>) {
        let mut open: Vec<(u64, u32, u32)> = self
            .shell
            .popups
            .iter()
            .filter(|(_, popup)| popup.grab)
            .filter_map(|(id, popup)| {
                let state = self.shell.surfaces.get(&popup.xdg_surface)?;
                Some((state.mapped.unwrap_or(u64::MAX), *id, state.surface.surface))
            })
            .collect();
        open.sort_unstable_by(|a, b| b.cmp(a));
        let mut dismissed = false;
        for (_, popup, surface) in open {
            if Some(surface) == pressed {
                break;
            }
            self.send(popup, xdg_popup::EVT_POPUP_DONE_OPCODE, Vec::new());
            if let Some(state) = self.shell.popups.get_mut(&popup) {
                state.grab = false;
            }
            dismissed = true;
        }
        if dismissed {
            self.sync_focus();
        }
    }
}

fn set_positioner(
    positioner: &mut Positioner,
    opcode: u16,
    args: &[Argument<u32, std::os::fd::OwnedFd>],
) -> Result<(), &'static str> {
    match (opcode, args) {
        (xdg_positioner::REQ_SET_SIZE_OPCODE, [Argument::Int(width), Argument::Int(height)]) => {
            if *width <= 0 || *height <= 0 {
                return Err("an empty popup size");
            }
            positioner.size = (*width, *height);
        }
        (
            xdg_positioner::REQ_SET_ANCHOR_RECT_OPCODE,
            [Argument::Int(x), Argument::Int(y), Argument::Int(width), Argument::Int(height)],
        ) => {
            if *width < 0 || *height < 0 {
                return Err("a negative anchor rectangle");
            }
            positioner.anchor_rect = Rect {
                x: *x,
                y: *y,
                width: *width,
                height: *height,
            };
        }
        (xdg_positioner::REQ_SET_ANCHOR_OPCODE, [Argument::Uint(anchor)]) => {
            positioner.anchor = Edge::from_wire(*anchor).ok_or("an unknown anchor")?;
        }
        (xdg_positioner::REQ_SET_GRAVITY_OPCODE, [Argument::Uint(gravity)]) => {
            positioner.gravity = Edge::from_wire(*gravity).ok_or("an unknown gravity")?;
        }
        (xdg_positioner::REQ_SET_CONSTRAINT_ADJUSTMENT_OPCODE, [Argument::Uint(bits)]) => {
            positioner.adjustment = Adjustment::from_wire(*bits);
        }
        (xdg_positioner::REQ_SET_OFFSET_OPCODE, [Argument::Int(x), Argument::Int(y)]) => {
            positioner.offset = (*x, *y);
        }
        (
            xdg_positioner::REQ_SET_REACTIVE_OPCODE
            | xdg_positioner::REQ_SET_PARENT_SIZE_OPCODE
            | xdg_positioner::REQ_SET_PARENT_CONFIGURE_OPCODE,
            _,
        ) => {}
        _ => return Err("unexpected arguments"),
    }
    Ok(())
}
