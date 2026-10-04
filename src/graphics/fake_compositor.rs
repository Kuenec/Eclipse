use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::mpsc;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_callback;
use wayland_client::protocol::wl_compositor::{self, WlCompositor};
use wayland_client::protocol::wl_display;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::content_type::v1::client::wp_content_type_manager_v1;

const DISPLAY: u32 = 1;

const HEADER_BYTES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) object: u32,
    pub(crate) interface: String,
    pub(crate) opcode: u16,
    pub(crate) words: Vec<u32>,
}

impl Request {
    pub(crate) fn call(&self) -> (&str, u16) {
        (&self.interface, self.opcode)
    }
}

pub(crate) struct FakeCompositor {
    connection: Connection,
    queue: EventQueue<Game>,
    surface: WlSurface,
    requests: mpsc::Receiver<Request>,
}

struct Game;

impl Dispatch<WlRegistry, GlobalListContents> for Game {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Game: WlCompositor);
delegate_noop!(Game: ignore WlSurface);

impl FakeCompositor {
    pub(crate) fn offering(extra_globals: &[(&'static str, u32)]) -> Self {
        let (client, server) = UnixStream::pair().expect("a socket pair for the fake compositor");
        let globals: Vec<(u32, &'static str, u32)> = [("wl_compositor", 4)]
            .iter()
            .chain(extra_globals)
            .zip(1..)
            .map(|(&(interface, version), name)| (name, interface, version))
            .collect();
        let (log, requests) = mpsc::channel();
        std::thread::spawn(move || serve(server, &globals, &log));
        let connection = Connection::from_socket(client).expect("libwayland-client");
        let (globals, queue) =
            registry_queue_init::<Game>(&connection).expect("the fake compositor's globals");
        let compositor: WlCompositor = globals
            .bind(&queue.handle(), 4..=4, ())
            .expect("the fake compositor's wl_compositor");
        let surface = compositor.create_surface(&queue.handle(), ());
        let mut compositor = Self {
            connection,
            queue,
            surface,
            requests,
        };
        compositor.requests_since_last_call();
        compositor
    }

    pub(crate) fn handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        let display = NonNull::new(self.connection.backend().display_ptr().cast::<c_void>())
            .expect("a connected wl_display");
        let surface =
            NonNull::new(self.surface.id().as_ptr().cast::<c_void>()).expect("a live wl_surface");
        (
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
        )
    }

    pub(crate) fn surface_id(&self) -> u32 {
        self.surface.id().protocol_id()
    }

    pub(crate) fn commit_surface(&self) {
        self.surface.commit();
    }

    pub(crate) fn requests_since_last_call(&mut self) -> Vec<Request> {
        self.queue
            .roundtrip(&mut Game)
            .expect("a roundtrip with the fake compositor");
        let mut requests: Vec<Request> = self.requests.try_iter().collect();
        let roundtrip = requests.pop().expect("the roundtrip's own sync");
        assert_eq!(
            roundtrip.call(),
            ("wl_display", wl_display::REQ_SYNC_OPCODE)
        );
        requests
    }
}

pub(crate) fn wire_string(text: &str) -> Vec<u32> {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let length = u32::try_from(bytes.len()).expect("a short wire string");
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    std::iter::once(length).chain(words(&bytes)).collect()
}

fn words(bytes: &[u8]) -> Vec<u32> {
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().copied().map(u32::from_ne_bytes).collect()
}

fn read_wire_string(words: &[u32]) -> Option<String> {
    let (&length, rest) = words.split_first()?;
    let bytes: Vec<u8> = rest.iter().flat_map(|word| word.to_ne_bytes()).collect();
    let text = bytes.get(..usize::try_from(length).ok()?.checked_sub(1)?)?;
    String::from_utf8(text.to_vec()).ok()
}

fn serve(
    mut stream: UnixStream,
    globals: &[(u32, &'static str, u32)],
    log: &mpsc::Sender<Request>,
) {
    let mut interfaces = HashMap::from([(DISPLAY, "wl_display".to_owned())]);
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut serial = 0;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => pending.extend_from_slice(&chunk[..read]),
        }
        while let Some((object, opcode, words)) = take_message(&mut pending) {
            let interface = interfaces.get(&object).cloned().unwrap_or_default();
            if let Some((id, created)) = created_object(&interface, opcode, &words) {
                interfaces.insert(id, created);
            }
            let request = Request {
                object,
                interface,
                opcode,
                words,
            };
            let events = answer(&request, globals, &mut serial);
            if log.send(request).is_err() {
                return;
            }
            for (object, opcode, args) in events {
                if stream.write_all(&encode(object, opcode, &args)).is_err() {
                    return;
                }
            }
        }
    }
}

fn take_message(pending: &mut Vec<u8>) -> Option<(u32, u16, Vec<u32>)> {
    let [object, size_and_opcode] = words(pending.get(..HEADER_BYTES)?)[..] else {
        return None;
    };
    let size = usize::try_from(size_and_opcode >> 16).ok()?;
    if size < HEADER_BYTES || pending.len() < size {
        return None;
    }
    let opcode = u16::try_from(size_and_opcode & 0xffff).ok()?;
    let arguments = words(&pending[HEADER_BYTES..size]);
    pending.drain(..size);
    Some((object, opcode, arguments))
}

fn created_object(interface: &str, opcode: u16, words: &[u32]) -> Option<(u32, String)> {
    let created = match (interface, opcode) {
        ("wl_display", wl_display::REQ_SYNC_OPCODE) => "wl_callback".to_owned(),
        ("wl_display", wl_display::REQ_GET_REGISTRY_OPCODE) => "wl_registry".to_owned(),
        ("wl_registry", wl_registry::REQ_BIND_OPCODE) => {
            return Some((*words.last()?, read_wire_string(words.get(1..)?)?));
        }
        ("wl_compositor", wl_compositor::REQ_CREATE_SURFACE_OPCODE) => "wl_surface".to_owned(),
        (
            "wp_content_type_manager_v1",
            wp_content_type_manager_v1::REQ_GET_SURFACE_CONTENT_TYPE_OPCODE,
        ) => "wp_content_type_v1".to_owned(),
        _ => return None,
    };
    Some((*words.first()?, created))
}

fn answer(
    request: &Request,
    globals: &[(u32, &'static str, u32)],
    serial: &mut u32,
) -> Vec<(u32, u16, Vec<u32>)> {
    match (request.call(), request.words.as_slice()) {
        (("wl_display", wl_display::REQ_SYNC_OPCODE), &[callback]) => {
            *serial += 1;
            vec![
                (callback, wl_callback::EVT_DONE_OPCODE, vec![*serial]),
                (DISPLAY, wl_display::EVT_DELETE_ID_OPCODE, vec![callback]),
            ]
        }
        (("wl_display", wl_display::REQ_GET_REGISTRY_OPCODE), &[registry]) => globals
            .iter()
            .map(|&(name, interface, version)| {
                let args = std::iter::once(name)
                    .chain(wire_string(interface))
                    .chain([version])
                    .collect();
                (registry, wl_registry::EVT_GLOBAL_OPCODE, args)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn encode(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
    let size = u32::try_from(HEADER_BYTES + 4 * args.len()).expect("a short event");
    [object, (size << 16) | u32::from(opcode)]
        .iter()
        .chain(args)
        .flat_map(|word| word.to_ne_bytes())
        .collect()
}
