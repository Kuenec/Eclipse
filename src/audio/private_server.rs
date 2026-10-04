use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROGRAMS: [&str; 4] = ["pipewire", "wireplumber", "pipewire-pulse", "pactl"];

const DAEMONS: [&[&str]; 3] = [
    &["pipewire"],
    &["wireplumber", "--profile", "policy"],
    &["pipewire-pulse"],
];

const SESSION_VARIABLES: [&str; 10] = [
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "PIPEWIRE_CONFIG_DIR",
    "PIPEWIRE_REMOTE",
    "PIPEWIRE_RUNTIME_DIR",
    "PIPEWIRE_NODE",
    "PULSE_SERVER",
    "PULSE_SINK",
    "PULSE_SOURCE",
];

const ROOT_PREFIX: &str = "eclipse-sound-";

const STARTUP: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(50);

pub(crate) struct PrivateServer {
    root: PathBuf,
    daemons: Vec<Child>,
}

impl PrivateServer {
    pub(crate) fn start(tag: &str) -> Option<Self> {
        let missing: Vec<&str> = PROGRAMS
            .into_iter()
            .filter(|program| !on_path(program))
            .collect();
        if !missing.is_empty() {
            eprintln!("no private PipeWire sound server: {missing:?} not found");
            return None;
        }
        let root = runtime_dir().join(format!("{ROOT_PREFIX}{tag}-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        for dir in ["runtime", "config", "state", "home"] {
            fs::create_dir_all(root.join(dir)).expect("create the sound server's directories");
        }
        fs::set_permissions(root.join("runtime"), Permissions::from_mode(0o700))
            .expect("make the sound server's runtime directory private");
        let mut server = Self {
            root,
            daemons: Vec::new(),
        };
        for daemon in DAEMONS {
            let log = File::create(server.root.join(format!("{}.log", daemon[0])))
                .expect("create the daemon log");
            let mut command = Command::new(daemon[0]);
            command
                .args(&daemon[1..])
                .env("HOME", server.root.join("home"))
                .env("XDG_RUNTIME_DIR", server.root.join("runtime"))
                .env("XDG_CONFIG_HOME", server.root.join("config"))
                .env("XDG_STATE_HOME", server.root.join("state"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().expect("share the daemon log"))
                .stderr(log);
            for variable in SESSION_VARIABLES {
                command.env_remove(variable);
            }
            let daemon = command
                .spawn()
                .unwrap_or_else(|error| panic!("cannot start {}: {error}", daemon[0]));
            server.daemons.push(daemon);
        }
        let deadline = Instant::now() + STARTUP;
        while !server.answers() {
            assert!(
                Instant::now() < deadline,
                "the private sound server did not answer within {STARTUP:?}; logs in {}",
                server.root.display()
            );
            std::thread::sleep(POLL);
        }
        Some(server)
    }

    pub(crate) fn address(&self) -> OsString {
        let mut address = OsString::from("unix:");
        address.push(self.root.join("runtime").join("pulse").join("native"));
        address
    }

    pub(crate) fn is_private(address: &OsStr) -> bool {
        let private = format!("unix:{}/{ROOT_PREFIX}", runtime_dir().display());
        address.to_string_lossy().starts_with(&private)
    }

    fn answers(&self) -> bool {
        Command::new("pactl")
            .arg("info")
            .env("PULSE_SERVER", self.address())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        for daemon in self.daemons.iter_mut().rev() {
            daemon.kill().ok();
            daemon.wait().ok();
        }
        fs::remove_dir_all(&self.root).ok();
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .expect("XDG_RUNTIME_DIR must name a directory for the sound server's sockets")
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}
