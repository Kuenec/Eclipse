mod app;
mod bridge;
mod cookies;
mod intent;
mod logging;
mod view;
mod wire;

use app::{App, Engine, Storage};
use eclipse_webview::proto::{self, ConsumerMsg, HelperMsg, PROTO_VERSION};
use gtk4 as gtk;
use gtk4::glib;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const APP_ID: &str = "io.github.kuenec.Eclipse";

const APPLICATION_NAME: &str = "Eclipse";

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

const EXIT_SETUP_FAILED: u8 = 1;

const DATA_DIR_ENV: &str = "ECLIPSE_WEBVIEW_DATA_DIR";

const CACHE_DIR_ENV: &str = "ECLIPSE_WEBVIEW_CACHE_DIR";

const SANDBOX_OPT_OUT_ENV: &str = "WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS";

fn parse_ipc_fd(args: impl Iterator<Item = OsString>) -> Result<RawFd, String> {
    let mut found = None;
    for arg in args {
        let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--ipc-fd=")) else {
            return Err(format!("unknown argument {arg:?}"));
        };
        let fd = value
            .parse::<RawFd>()
            .ok()
            .filter(|fd| *fd > 2)
            .ok_or_else(|| format!("invalid --ipc-fd value {value:?}"))?;
        if found.replace(fd).is_some() {
            return Err("--ipc-fd given twice".to_string());
        }
    }
    found.ok_or_else(|| "missing --ipc-fd=<fd>".to_string())
}

fn storage_dir(name: &str) -> Result<PathBuf, String> {
    let dir = std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is not set"))?;
    if !dir.is_absolute() || !dir.is_dir() {
        return Err(format!(
            "{name} must name an existing absolute directory, not {}",
            dir.display()
        ));
    }
    Ok(dir)
}

fn adopt_close_on_exec(fd: RawFd) -> Result<OwnedFd, String> {
    std::fs::symlink_metadata(format!("/proc/self/fd/{fd}"))
        .map_err(|error| format!("the control socket fd {fd} is not open: {error}"))?;
    let inherited = unsafe { OwnedFd::from_raw_fd(fd) };
    inherited
        .try_clone()
        .map_err(|error| format!("cannot duplicate the control socket fd {fd}: {error}"))
}

fn keep_web_process_sandbox() {
    if std::env::var_os(SANDBOX_OPT_OUT_ENV).is_some() {
        logging::warn(format_args!(
            "ignoring {SANDBOX_OPT_OUT_ENV}: web pages always run in WebKit's sandbox"
        ));
        std::env::remove_var(SANDBOX_OPT_OUT_ENV);
    }
}

fn engine_version() -> String {
    format!(
        "webkitgtk/{}.{}.{}",
        webkit6::functions::major_version(),
        webkit6::functions::minor_version(),
        webkit6::functions::micro_version()
    )
}

fn write_frame(stream: &UnixStream, msg: &HelperMsg) -> Result<(), String> {
    let frame = msg
        .encode()
        .map_err(|error| format!("cannot encode {}: {error}", msg.name()))?;
    (&mut &*stream)
        .write_all(&frame)
        .map_err(|error| format!("cannot write {}: {error}", msg.name()))
}

fn handshake(stream: &UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(HELLO_TIMEOUT))
        .map_err(|error| format!("cannot arm the handshake timeout: {error}"))?;
    let version = match proto::read_consumer_msg(&mut &*stream) {
        Ok(ConsumerMsg::Hello { version }) => version,
        Ok(_) => return Err("the first message was not Hello".to_string()),
        Err(error) => return Err(format!("no Hello within {HELLO_TIMEOUT:?}: {error}")),
    };
    write_frame(
        stream,
        &HelperMsg::HelloAck {
            version: PROTO_VERSION,
            engine: engine_version(),
        },
    )?;
    if version != PROTO_VERSION {
        return Err(format!(
            "protocol version mismatch: host v{version}, helper v{PROTO_VERSION}"
        ));
    }
    stream
        .set_read_timeout(None)
        .map_err(|error| format!("cannot clear the handshake timeout: {error}"))
}

fn fail(stream: &UnixStream, reason: String) -> ExitCode {
    logging::error(format_args!("{reason}"));
    if let Err(error) = write_frame(stream, &HelperMsg::Fatal { reason }) {
        logging::error(format_args!("{error}"));
    }
    ExitCode::from(EXIT_SETUP_FAILED)
}

fn main() -> ExitCode {
    let fd = match parse_ipc_fd(std::env::args_os().skip(1)) {
        Ok(fd) => fd,
        Err(error) => {
            logging::error(format_args!(
                "{error}; usage: eclipse-webview --ipc-fd=<fd>"
            ));
            return ExitCode::from(EXIT_SETUP_FAILED);
        }
    };
    let socket = match adopt_close_on_exec(fd) {
        Ok(socket) => socket,
        Err(error) => {
            logging::error(format_args!("{error}"));
            return ExitCode::from(EXIT_SETUP_FAILED);
        }
    };
    let stream = UnixStream::from(socket);
    if let Err(error) = handshake(&stream) {
        logging::error(format_args!("{error}"));
        return ExitCode::from(app::EXIT_CONSUMER_LOST);
    }
    let storage = match (storage_dir(DATA_DIR_ENV), storage_dir(CACHE_DIR_ENV)) {
        (Ok(data), Ok(cache)) => Storage { data, cache },
        (Err(error), _) | (_, Err(error)) => return fail(&stream, error),
    };
    if let Err(error) = std::env::set_current_dir("/") {
        return fail(
            &stream,
            format!("cannot change the working directory to /: {error}"),
        );
    }
    keep_web_process_sandbox();
    glib::set_prgname(Some(APP_ID));
    glib::set_application_name(APPLICATION_NAME);
    if let Err(error) = gtk::init() {
        return fail(
            &stream,
            format!("GTK could not open the display (WAYLAND_DISPLAY or DISPLAY): {error}"),
        );
    }
    gtk::Window::set_default_icon_name(APP_ID);
    if let Err(error) = app::configure_web_context() {
        return fail(&stream, error);
    }
    let engine = match Engine::open(&storage) {
        Ok(engine) => engine,
        Err(error) => return fail(&stream, error),
    };
    let wire = match wire::Wire::new(OwnedFd::from(stream)) {
        Ok(wire) => wire,
        Err(error) => {
            logging::error(format_args!("{error}"));
            return ExitCode::from(EXIT_SETUP_FAILED);
        }
    };
    let main_loop = glib::MainLoop::new(None, false);
    let app = App::new(wire, engine, main_loop.clone());
    app.listen();
    logging::info(format_args!("{} ready", engine_version()));
    main_loop.run();
    ExitCode::from(app.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_control_socket_fd_is_the_only_argument_and_lies_above_stdio() {
        assert_eq!(parse_ipc_fd(args(&["--ipc-fd=3"])), Ok(3));
        assert!(parse_ipc_fd(args(&[])).is_err());
        assert!(parse_ipc_fd(args(&["--ipc-fd=0"])).is_err());
        assert!(parse_ipc_fd(args(&["--ipc-fd=abc"])).is_err());
        assert!(parse_ipc_fd(args(&["--ipc-fd=3", "--ipc-fd=4"])).is_err());
        assert_eq!(
            parse_ipc_fd(args(&["--ipc-fd=3", "--allow-unsandboxed"])),
            Err("unknown argument \"--allow-unsandboxed\"".to_string())
        );
    }

    #[test]
    fn the_handshake_answers_with_this_protocol_and_rejects_others() {
        let (host, helper) = UnixStream::pair().expect("socketpair");
        (&mut &host)
            .write_all(
                &ConsumerMsg::Hello {
                    version: PROTO_VERSION,
                }
                .encode()
                .expect("encode"),
            )
            .expect("hello");
        handshake(&helper).expect("handshake");
        match proto::read_helper_msg(&mut &host).expect("ack") {
            HelperMsg::HelloAck { version, engine } => {
                assert_eq!(version, PROTO_VERSION);
                assert!(engine.starts_with("webkitgtk/"), "{engine}");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }

        let (host, helper) = UnixStream::pair().expect("socketpair");
        (&mut &host)
            .write_all(
                &ConsumerMsg::Hello {
                    version: PROTO_VERSION + 1,
                }
                .encode()
                .expect("encode"),
            )
            .expect("hello");
        assert!(handshake(&helper).unwrap_err().contains("mismatch"));
    }
}
