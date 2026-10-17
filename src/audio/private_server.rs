use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Permissions};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait as _, HostTrait as _};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use rustix::process::Pid;

use super::null_devices::NullDevices;
use super::{end_with_this_process, PULSE_PCM};

const PROGRAMS: [&str; 4] = ["pipewire", "wireplumber", "pipewire-pulse", "pactl"];

const SESSION_MANAGER: [&str; 3] = ["wireplumber", "--profile", "policy"];

const DAEMONS: [&[&str]; 3] = [&["pipewire"], &SESSION_MANAGER, &["pipewire-pulse"]];

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

const CLIENT_VARIABLES: [&str; 4] = [
    "ALSA_CONFIG_PATH",
    "ALSA_CONFIG_DIR",
    "PULSE_SINK",
    "PULSE_SOURCE",
];

const ROOT_PREFIX: &str = "eclipse-sound-";

const SOCKET: &str = "runtime/pulse/native";

const CONFIG: &str = "config";

const ALSA_OVERRIDE: &str = "config/alsa/asoundrc";

const PRIVATE_DEFAULT: &str = "Eclipse test sound server";

const DEFAULT_PCM: &str = "default";

const ON_SERVER: &str = "ECLIPSE_TEST_ON_PRIVATE_SOUND_SERVER";

const TEST_LIMIT: Duration = Duration::from_secs(300);

const STARTUP: Duration = Duration::from_secs(10);

const POLL: Duration = Duration::from_millis(50);

static STARTED: AtomicUsize = AtomicUsize::new(0);

struct PrivateServer {
    root: PathBuf,
    daemons: Vec<Child>,
}

impl PrivateServer {
    fn start() -> Result<Self, String> {
        let missing: Vec<&str> = PROGRAMS
            .into_iter()
            .filter(|program| !on_path(program))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "no private PipeWire sound server: {missing:?} not found"
            ));
        }
        let runtime = runtime_dir();
        remove_abandoned_roots(&runtime);
        let root = runtime.join(format!(
            "{ROOT_PREFIX}{}-{}",
            std::process::id(),
            STARTED.fetch_add(1, Ordering::Relaxed)
        ));
        fs::remove_dir_all(&root).ok();
        for dir in ["runtime", "config/alsa", "state", "home"] {
            fs::create_dir_all(root.join(dir)).expect("create the sound server's directories");
        }
        fs::set_permissions(root.join("runtime"), Permissions::from_mode(0o700))
            .expect("make the sound server's runtime directory private");
        fs::write(
            root.join(ALSA_OVERRIDE),
            format!(
                "pcm.!{DEFAULT_PCM} {{ type {PULSE_PCM} hint {{ show on description \
                 \"{PRIVATE_DEFAULT}\" }} }}\n"
            ),
        )
        .expect("make the sound server ALSA's default device");
        let mut server = Self {
            root,
            daemons: Vec::new(),
        };
        server.check_session_manager()?;
        let pulse = cpal::DeviceId::new(cpal::HostId::Alsa, PULSE_PCM);
        if cpal::default_host().device_by_id(&pulse).is_none() {
            return Err(format!(
                "ALSA has no \"{PULSE_PCM}\" device to reach a private sound server through"
            ));
        }
        for daemon in DAEMONS {
            let log = File::create(server.root.join(format!("{}.log", daemon[0])))
                .expect("create the daemon log");
            let mut command = server.command(daemon);
            command
                .stdin(Stdio::null())
                .stdout(log.try_clone().expect("share the daemon log"))
                .stderr(log);
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
        Ok(server)
    }

    fn command(&self, program: &[&str]) -> Command {
        let mut command = Command::new(program[0]);
        command
            .args(&program[1..])
            .env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join(CONFIG))
            .env("XDG_STATE_HOME", self.root.join("state"));
        for variable in SESSION_VARIABLES {
            command.env_remove(variable);
        }
        end_with_this_process(&mut command);
        command
    }

    fn check_session_manager(&self) -> Result<(), String> {
        let mut probe = self.command(&SESSION_MANAGER);
        probe.arg("--version");
        let output = crate::bounded_child::output(&mut probe, STARTUP);
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "WirePlumber 0.5 or later is needed for its policy profile: \
             `{} --version` failed ({}): {}",
            SESSION_MANAGER.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    fn address(&self) -> OsString {
        let mut address = OsString::from("unix:");
        address.push(self.root.join(SOCKET));
        address
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

pub(crate) fn on_private_server(test: &str, body: impl FnOnce(&NullDevices)) {
    if std::env::var_os(ON_SERVER).is_some() {
        refuse_other_clients(&refuse_unless_private());
        let defaults = NullDevices::create("default");
        defaults.make_system_default();
        body(&defaults);
        return;
    }
    let server = match PrivateServer::start() {
        Ok(server) => server,
        Err(reason) => {
            eprintln!("{test} skipped: {reason}");
            return;
        }
    };
    let mut command = test_command(test);
    command
        .env(ON_SERVER, "1")
        .env("PULSE_SERVER", server.address())
        .env("XDG_CONFIG_HOME", server.root.join(CONFIG));
    for variable in CLIENT_VARIABLES {
        command.env_remove(variable);
    }
    let output = crate::bounded_child::output(&mut command, TEST_LIMIT);
    assert_passed(&output);
}

pub(crate) fn test_command(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("the test binary has a path"));
    command.args(["--exact", test, "--test-threads=1", "--nocapture"]);
    end_with_this_process(&mut command);
    command
}

pub(crate) fn assert_passed(output: &Output) {
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}",
        outcome(output)
    );
}

pub(crate) fn outcome(output: &Output) -> String {
    format!(
        "status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub(crate) fn exited_by(process: &OwnedFd, deadline: Instant) -> bool {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = Timespec::try_from(remaining).expect("a poll timeout");
        match rustix::event::poll(&mut [PollFd::new(process, PollFlags::IN)], Some(&timeout)) {
            Ok(ready) => return ready > 0,
            Err(Errno::INTR) => {}
            Err(error) => panic!("cannot wait for a process to exit: {error}"),
        }
    }
}

pub(crate) fn refuse_unless_private() -> PathBuf {
    let server = std::env::var_os("PULSE_SERVER").unwrap_or_default();
    root_of(&server, &runtime_dir()).unwrap_or_else(|| {
        panic!("refusing to use the sound server at PULSE_SERVER={server:?}: it is not private")
    })
}

fn refuse_other_clients(root: &Path) {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    assert_eq!(
        config,
        Some(root.join(CONFIG)),
        "refusing to run with ALSA and PulseAudio clients configured outside {}",
        root.display()
    );
    let default = cpal::DeviceId::new(cpal::HostId::Alsa, DEFAULT_PCM);
    let described = cpal::default_host()
        .device_by_id(&default)
        .and_then(|device| device.description().ok());
    assert_eq!(
        described.as_ref().map(cpal::DeviceDescription::name),
        Some(PRIVATE_DEFAULT),
        "refusing to run: ALSA's default device is not the private sound server"
    );
}

fn root_of(address: &OsStr, runtime: &Path) -> Option<PathBuf> {
    let address = address.to_str()?;
    if address.contains(char::is_whitespace) {
        return None;
    }
    let socket = Path::new(address.strip_prefix("unix:")?);
    let root = socket.ancestors().nth(3)?;
    let named = root.file_name()?.to_str()?.starts_with(ROOT_PREFIX);
    (named && root.parent() == Some(runtime) && socket == root.join(SOCKET))
        .then(|| root.to_owned())
}

fn remove_abandoned_roots(runtime: &Path) {
    for entry in fs::read_dir(runtime).expect("list XDG_RUNTIME_DIR") {
        let root = entry.expect("list XDG_RUNTIME_DIR").path();
        let Some(owner) = owner_of(&root) else {
            continue;
        };
        if rustix::process::test_kill_process(owner) != Err(Errno::SRCH) {
            continue;
        }
        match fs::remove_dir_all(&root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => panic!(
                "cannot remove the abandoned sound server {}: {error}",
                root.display()
            ),
            _ => {}
        }
    }
}

fn owner_of(root: &Path) -> Option<Pid> {
    let name = root.file_name()?.to_str()?.strip_prefix(ROOT_PREFIX)?;
    let (pid, _) = name.split_once('-')?;
    Pid::from_raw(pid.parse().ok()?)
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader};

    use rustix::process::{PidfdFlags, Signal};

    use super::*;

    const SHIMMED: &str = "ECLIPSE_TEST_SHIMMED_SESSION_MANAGER";

    const LATE_TEST: &str =
        "audio::private_server::tests::the_defaults_wait_for_a_session_manager_that_starts_late";

    const LATE_SESSION_MANAGER: &str =
        "#!/bin/sh\nsleep 2\nPATH=${PATH#*:} exec wireplumber \"$@\"\n";

    const OLD_TEST: &str =
        "audio::private_server::tests::sound_tests_skip_when_wireplumber_has_no_policy_profile";

    const OLD_SESSION_MANAGER: &str = "#!/bin/sh\necho \"Unknown option $1\" >&2\nexit 64\n";

    const HOLDER: &str = "ECLIPSE_TEST_SOUND_SERVER_HOLDER";

    const HOLDER_TEST: &str =
        "audio::private_server::tests::a_killed_test_process_takes_its_sound_server_with_it";

    const HELD: &str = "held daemons: ";

    const SKIPPED: &str = "skipped: ";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("eclipse-sound-shims-{tag}-{}", std::process::id()));
            fs::remove_dir_all(&root).ok();
            fs::create_dir_all(&root).expect("create the shim directory");
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn with_session_manager(test: &str, tag: &str, script: &str) -> Option<Output> {
        let missing: Vec<&str> = PROGRAMS
            .into_iter()
            .filter(|program| !on_path(program))
            .collect();
        if !missing.is_empty() {
            eprintln!("{test} skipped: {missing:?} not found");
            return None;
        }
        let shims = Scratch::new(tag);
        let shim = shims.0.join(SESSION_MANAGER[0]);
        fs::write(&shim, script).expect("write the session manager shim");
        fs::set_permissions(&shim, Permissions::from_mode(0o755))
            .expect("make the session manager shim executable");
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(shims.0.clone()).chain(std::env::split_paths(&path)),
        )
        .expect("put the shim first on PATH");
        let mut command = test_command(test);
        command.env(SHIMMED, "1").env("PATH", path);
        Some(crate::bounded_child::output(&mut command, TEST_LIMIT))
    }

    #[test]
    fn the_defaults_wait_for_a_session_manager_that_starts_late() {
        if std::env::var_os(SHIMMED).is_some() {
            on_private_server(LATE_TEST, |_| {});
            return;
        }
        if let Some(output) = with_session_manager(LATE_TEST, "late", LATE_SESSION_MANAGER) {
            assert_passed(&output);
        }
    }

    #[test]
    fn sound_tests_skip_when_wireplumber_has_no_policy_profile() {
        if std::env::var_os(SHIMMED).is_some() {
            on_private_server(OLD_TEST, |_| {
                panic!("the test ran without a session manager")
            });
            return;
        }
        let Some(output) = with_session_manager(OLD_TEST, "old", OLD_SESSION_MANAGER) else {
            return;
        };
        assert_passed(&output);
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(
            said.contains(&format!(
                "{OLD_TEST} skipped: WirePlumber 0.5 or later is needed for its policy profile"
            )) && said.contains("Unknown option --profile"),
            "{said}"
        );
    }

    fn hold_a_server_until_killed() {
        let server = match PrivateServer::start() {
            Ok(server) => server,
            Err(reason) => {
                println!("{SKIPPED}{reason}");
                return;
            }
        };
        let pids: Vec<String> = server
            .daemons
            .iter()
            .map(|daemon| daemon.id().to_string())
            .collect();
        println!("{HELD}{}", pids.join(" "));
        std::thread::sleep(TEST_LIMIT);
        panic!("nothing killed this test process within {TEST_LIMIT:?}");
    }

    #[test]
    fn a_killed_test_process_takes_its_sound_server_with_it() {
        if std::env::var_os(HOLDER).is_some() {
            hold_a_server_until_killed();
            return;
        }
        let mut command = test_command(HOLDER_TEST);
        command
            .env(HOLDER, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut holder = command.spawn().expect("start the test process to kill");
        let mut lines = BufReader::new(holder.stdout.take().expect("the holder's stdout"))
            .lines()
            .map(|line| line.expect("read the holder's output"));
        let said = lines
            .by_ref()
            .find(|line| line.contains(HELD) || line.contains(SKIPPED))
            .unwrap_or_default();
        let Some((_, pids)) = said.split_once(HELD) else {
            lines.for_each(drop);
            let ended = holder.wait_with_output().expect("wait for the holder");
            let skipped = said.split_once(SKIPPED).map(|(_, reason)| reason);
            assert!(
                ended.status.success() && skipped.is_some(),
                "the holder held no sound server: {}",
                String::from_utf8_lossy(&ended.stderr)
            );
            eprintln!("{HOLDER_TEST} skipped: {}", skipped.unwrap_or_default());
            return;
        };
        let exits: Vec<OwnedFd> = pids
            .split(' ')
            .map(|pid| {
                let pid = Pid::from_raw(pid.parse().expect("a daemon pid")).expect("a daemon pid");
                rustix::process::pidfd_open(pid, PidfdFlags::empty()).expect("watch a daemon")
            })
            .collect();
        let root = runtime_dir().join(format!("{ROOT_PREFIX}{}-0", holder.id()));
        holder.kill().expect("kill the holder");
        holder.wait().expect("wait for the holder");
        let deadline = Instant::now() + STARTUP;
        let survivors: Vec<&OwnedFd> = exits
            .iter()
            .filter(|exit| !exited_by(exit, deadline))
            .collect();
        for survivor in &survivors {
            rustix::process::pidfd_send_signal(survivor, Signal::KILL).ok();
        }
        remove_abandoned_roots(&runtime_dir());
        let abandoned = root.exists();
        fs::remove_dir_all(&root).ok();
        assert!(
            survivors.is_empty(),
            "{} of the daemons in {pids} outlived their killed test process",
            survivors.len()
        );
        assert!(!abandoned, "{} was left behind", root.display());
    }

    #[test]
    fn only_a_lone_socket_of_a_private_server_counts_as_private() {
        let runtime = Path::new("/run/user/1000");
        let root = runtime.join("eclipse-sound-41-0");
        let private = format!("unix:{}", root.join(SOCKET).display());
        assert_eq!(root_of(OsStr::new(&private), runtime), Some(root.clone()));
        for refused in [
            "unix:/run/user/1000/pulse/native".to_owned(),
            format!("{private} unix:/run/user/1000/pulse/native"),
            "unix:/run/user/1000/eclipse-sound-41-0/../pulse/native".to_owned(),
            "unix:/run/user/1000/eclipse-sound-41-0/runtime/pulse/native/..".to_owned(),
            "unix:/tmp/eclipse-sound-41-0/runtime/pulse/native".to_owned(),
            "unix:/run/user/1000/other-41-0/runtime/pulse/native".to_owned(),
            root.join(SOCKET).display().to_string(),
            "tcp:localhost:4713".to_owned(),
            String::new(),
        ] {
            assert_eq!(root_of(OsStr::new(&refused), runtime), None, "{refused}");
        }
    }
}
