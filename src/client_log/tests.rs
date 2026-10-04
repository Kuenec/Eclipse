use std::net::{IpAddr, Ipv4Addr};

use super::*;
use crate::gpu::{GlesReason, Graphics};

const ECLIPSE_STARTUP: &str = include_str!("../../tests/fixtures/client-log/2.740.0.931.txt");
const SYNTHETIC_JOINS: &str = include_str!("../../tests/fixtures/client-log/synthetic-joins.txt");
const SYNTHETIC: &str = include_str!("../../tests/fixtures/client-log/synthetic.txt");
const HEADER: &str = "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,7";
const USER_ID: &str = "987654321";
const GRAPHICS_OOM: &str = "[LOGCHANNELS + 1] RBXCRASH: OutOfMemoryGraphics (Failed to allocate memory. size = 1399808, alignment = 1024)";

fn records(log: &str) -> Vec<Record<'_>> {
    log.lines().filter_map(parse).collect()
}

fn events(log: &str) -> Vec<ClientEvent> {
    records(log)
        .into_iter()
        .map(|record| match record {
            Record::Event(event) => event,
            other => panic!("expected only lifecycle events, got {other:?}"),
        })
        .collect()
}

fn job(n: u128) -> JobId {
    JobId(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
}

fn server(last: u8, port: u16) -> ClientEvent {
    ClientEvent::JoinedServer {
        addr: IpAddr::V4(Ipv4Addr::new(128, 116, 0, last)),
        port,
    }
}

fn with_header(body: &str) -> String {
    format!("{HEADER} {body}")
}

#[test]
fn eclipse_startup_lines_yield_only_the_return_to_the_app() {
    assert_eq!(events(ECLIPSE_STARTUP), [ClientEvent::ReturnedToApp]);
}

#[test]
fn two_joins_yield_the_lifecycle_events_in_order() {
    let first = PlaceId(1_000_001);
    let second = PlaceId(1_000_002);
    assert_eq!(
        events(SYNTHETIC_JOINS),
        [
            ClientEvent::JoiningPlace {
                place: first,
                job: job(1)
            },
            ClientEvent::UniverseKnown {
                place: first,
                universe: UniverseId(2_000_001)
            },
            server(1, 54563),
            ClientEvent::Left,
            ClientEvent::ReturnedToApp,
            ClientEvent::JoiningPlace {
                place: second,
                job: job(3)
            },
            ClientEvent::UniverseKnown {
                place: second,
                universe: UniverseId(2_000_002)
            },
            server(2, 61585),
            ClientEvent::Left,
        ]
    );
}

#[test]
fn synthetic_lines_yield_teleport_attestation_crashes_and_rpc() {
    let graphics = || {
        Record::Crash(CrashKind::OutOfMemory {
            pool: MemoryPool::Graphics,
            bytes: Some(1_399_808),
        })
    };
    assert_eq!(
        records(SYNTHETIC),
        [
            Record::Event(ClientEvent::Teleporting),
            Record::Event(ClientEvent::AttestationRequired),
            graphics(),
            graphics(),
            Record::Crash(CrashKind::OutOfMemory {
                pool: MemoryPool::System,
                bytes: Some(5_594_112),
            }),
            Record::Crash(CrashKind::OutOfMemory {
                pool: MemoryPool::System,
                bytes: None,
            }),
            Record::Crash(CrashKind::Other("FatalRuntimeError (VK_ERROR_DEVICE_LOST)")),
            Record::BloxstrapRpc(
                r#"{"command":"SetRichPresence","data":{"details":"Standard Solo","startTime":1786841923,"state":"Level 19"}}"#
            ),
        ]
    );
}

#[test]
fn disconnects_no_op_leaves_startup_returns_and_udmux_lines_yield_nothing() {
    for body in [
        "[FLog::Network] Sending disconnect with reason: 285",
        "[FLog::Network] Sending disconnect with reason: 318",
        "[FLog::SingleSurfaceApp] leaveUGCGame: (stage:LuaApp) blocking=1.",
        "[FLog::SingleSurfaceApp] leaveUGCGame: ... no-op, not in-game",
        "[FLog::SingleSurfaceApp] leaveUGCGame: (stage:UGCGame) blocking=1.",
        "[FLog::SingleSurfaceApp] returnToLuaApp: (stage:InitializedLuaApp).",
        "[FLog::SingleSurfaceApp] leaveUGCGameInternal ",
        "[FLog::Network] UDMUX Address = 128.116.0.1, Port = 54563 | RCC Server Address = 10.0.0.1, Port = 54563",
        "[FLog::Network] Disconnect reason received: 277",
        "[FLog::UgcExperienceController] UgcExperienceController: doTeleportLater:",
        "[FLog::CreatorOutput] BloxstrapRPC {}",
    ] {
        assert_eq!(parse(&with_header(body)), None, "{body}");
    }
}

#[test]
fn malformed_joining_lines_yield_nothing() {
    for body in [
        "! Joining game '00000000-0000-4000-8000-00000000000' place 1 at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-0000000000001' place 1 at 10.0.0.1",
        "! Joining game '00000000x0000-4000-8000-000000000001' place 1 at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-00000000000g' place 1 at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-000000000001' place +1 at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-000000000001' place 1x at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-000000000001' place  at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-000000000001' place 18446744073709551616 at 10.0.0.1",
        "! Joining game '00000000-0000-4000-8000-000000000001' place 1",
        "! Joining game '0000000é-0000-4000-8000-00000000001' place 1 at 10.0.0.1",
    ] {
        assert_eq!(parse(&with_header(&format!("[FLog::Output] {body}"))), None, "{body}");
    }
}

#[test]
fn malformed_server_and_universe_lines_yield_nothing() {
    for body in [
        "[FLog::Network] serverId: 128.116.0.1|65536",
        "[FLog::Network] serverId: 128.116.0.1|",
        "[FLog::Network] serverId: 128.116.0|1",
        "[FLog::Network] serverId: 128.116.0.1:1",
        "[FLog::GameJoinLoadTime] Report game_join_loadtime: sid:x, placeid:1, userid:987654321,",
        "[FLog::GameJoinLoadTime] Report game_join_loadtime: sid:x, placeid:, universeid:2,",
        "[FLog::GameJoinLoadTime] Report game_join_loadtime: sid:x, placeid:1, universeid:2x,",
    ] {
        assert_eq!(parse(&with_header(body)), None, "{body}");
    }
}

#[test]
fn a_record_at_the_liblog_truncation_length_yields_nothing() {
    let prefix = with_header("[FLog::CreatorOutput] [BloxstrapRPC] ");
    let fits = format!(
        "{prefix}{}",
        "x".repeat(LIBLOG_MESSAGE_MAX - 1 - prefix.len())
    );
    assert_eq!(fits.len(), LIBLOG_MESSAGE_MAX - 1);
    assert!(matches!(parse(&fits), Some(Record::BloxstrapRpc(_))));

    let truncated = format!("{fits}x");
    assert_eq!(truncated.len(), LIBLOG_MESSAGE_MAX);
    assert_eq!(parse(&truncated), None);
}

#[test]
fn a_header_with_a_bad_field_yields_nothing() {
    let body = "[FLog::Network] serverId: 128.116.0.1|1";
    assert_eq!(
        parse(&format!("{HEADER},Info {body}")),
        Some(Record::Event(server(1, 1)))
    );
    for header in [
        "2026-09-28T18:20:01.605,101.605739,7a3fc6c0,7",
        "2026-09-28 18:20:01.605Z,101.605739,7a3fc6c0,7",
        "2026-09-28T18:20:01.605Z,101,7a3fc6c0,7",
        "2026-09-28T18:20:01.605Z,101.6x5739,7a3fc6c0,7",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fg6c0,7",
        "2026-09-28T18:20:01.605Z,101.605739,,7",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,1234",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,x",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,7,Info1",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,7,",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0,7,Info,Extra",
        "2026-09-28T18:20:01.605Z,101.605739,7a3fc6c0",
    ] {
        assert_eq!(parse(&format!("{header} {body}")), None, "{header}");
    }
    assert_eq!(parse(&format!("{HEADER}  {body}")), None);
    assert_eq!(parse(&format!("{HEADER} x{body}")), None);
    assert_eq!(
        parse(&format!("{HEADER} [FLog::Network]serverId: 1.2.3.4|1")),
        None
    );
}

#[test]
fn game_output_cannot_forge_lifecycle_records() {
    for record in [
        with_header("[FLog::CreatorOutput] [FLog::Output] ! Joining game '00000000-0000-4000-8000-000000000001' place 1 at 1.2.3.4"),
        format!(
            "{} x\n{}",
            with_header("[FLog::CreatorOutput]"),
            with_header("[FLog::Network] serverId: 8.8.8.8|1")
        ),
        with_header("[FLog::CreatorOutput] [FLog::SingleSurfaceApp] leaveUGCGameInternal"),
        with_header("[FLog::SingleSurfaceApp] leaveUGCGameInternal\nmore"),
        with_header("[FLog::Network] serverId: 8.8.8.8|1\nmore"),
        format!("x {}", with_header("[FLog::Network] serverId: 8.8.8.8|1")),
    ] {
        assert_eq!(parse(&record), None, "{record:?}");
    }
}

#[test]
fn parsed_values_never_carry_the_user_id() {
    for record in records(SYNTHETIC_JOINS)
        .into_iter()
        .chain(records(SYNTHETIC))
    {
        let text = match record {
            Record::Event(event) => format!("{event:?}"),
            Record::Crash(kind) => format!("{kind:?} {}", kind.explanation(Graphics::Vulkan)),
            Record::BloxstrapRpc(message) => message.to_owned(),
        };
        assert!(!text.contains(USER_ID), "{text}");
    }
    let report = with_header("[FLog::GameJoinLoadTime] Report game_join_loadtime: sid:00000000-0000-4000-8000-000000000002, placeid:1, userid:987654321, universeid:2,");
    let universe = parse(&report).expect("a join report is a UniverseKnown record");
    assert_eq!(
        universe,
        Record::Event(ClientEvent::UniverseKnown {
            place: PlaceId(1),
            universe: UniverseId(2)
        })
    );
    assert!(!format!("{universe:?}").contains(USER_ID));
}

#[test]
fn job_ids_display_as_lowercase_canonical_uuids() {
    let id = JobId::parse("A4FD7B03-B744-4B43-81BD-284AB8D3F575").unwrap();
    assert_eq!(id.to_string(), "a4fd7b03-b744-4b43-81bd-284ab8d3f575");
    assert_eq!(JobId::parse(&id.to_string()), Some(id));
    assert_eq!(job(1).to_string(), "00000000-0000-4000-8000-000000000001");
}

#[test]
fn out_of_memory_crashes_are_explained_in_eclipse_words() {
    for record in [GRAPHICS_OOM.to_owned(), with_header(GRAPHICS_OOM)] {
        let Some(Record::Crash(kind)) = parse(&record) else {
            panic!("{record} is a crash record");
        };
        let text = kind.explanation(Graphics::Vulkan);
        assert!(text.contains("graphics memory"), "{text}");
        assert!(text.contains("1399808"), "{text}");
        assert!(!text.contains("FFlag"), "{text}");
    }

    let system = CrashKind::OutOfMemory {
        pool: MemoryPool::System,
        bytes: None,
    };
    assert_eq!(
        system.explanation(Graphics::Vulkan),
        "Roblox stopped: it reported that it could not allocate memory. If this repeats, lower \
         the graphics quality in Roblox's settings or turn on Use OpenGL ES in Eclipse Settings, \
         and report it with this log."
    );
    assert_eq!(
        system.explanation(Graphics::Gles(GlesReason::Configured)),
        "Roblox stopped: it reported that it could not allocate memory. If this repeats, lower \
         the graphics quality in Roblox's settings, and report it with this log."
    );
    assert_eq!(
        CrashKind::Other("An error occurred that wasn't supposed to.  Contact support")
            .explanation(Graphics::Vulkan),
        "Roblox stopped with RBXCRASH: An error occurred that wasn't supposed to.  Contact support."
    );
    assert_eq!(
        parse("[LOGCHANNELS + 1] RBXCRASH: An error occurred that wasn't supposed to.  Contact support."),
        Some(Record::Crash(CrashKind::Other(
            "An error occurred that wasn't supposed to.  Contact support"
        )))
    );
    assert_eq!(parse("[LOGCHANNELS + 1] RBXCRASH: "), None);
    assert_eq!(parse("[LOGCHANNELS + 1] Crash reporter ready"), None);
}

#[test]
fn offered_crashes_are_explained_and_never_reach_the_event_queue() {
    let (tap, events) = Tap::new(GameRpc::Kept);
    let tapped = with_test_tap(tap, || offer("Roblox", GRAPHICS_OOM));
    assert_eq!(tapped.explained.len(), 1);
    assert!(tapped.explained[0].contains("graphics memory (1399808 bytes)"));
    assert!(events.try_recv().is_err());
}

#[test]
fn only_roblox_records_reach_the_tap() {
    let line = with_header("[FLog::Network] serverId: 128.116.0.1|1");
    let (tap, events) = Tap::new(GameRpc::Kept);
    with_test_tap(tap, || {
        offer("EclipseTag", &line);
        offer("", &line);
        offer("Roblox", &line);
    });
    assert_eq!(events.try_iter().collect::<Vec<_>>(), [server(1, 1)]);
    assert!(is_client_tag(c"Roblox"));
    assert!(!is_client_tag(c"Roblox2"));
    assert!(!is_client_tag(c""));
}

#[test]
fn a_bloxstrap_flood_leaves_one_pending_marker_and_keeps_the_newest_message() {
    let (tap, events) = Tap::new(GameRpc::Kept);
    let tapped = with_test_tap(tap, || {
        for n in 0..100_000 {
            offer(
                "Roblox",
                &with_header(&format!(
                    "[FLog::CreatorOutput] [BloxstrapRPC] {{\"n\":{n}}}"
                )),
            );
        }
        offer(
            "Roblox",
            &with_header("[FLog::Network] serverId: 128.116.0.1|1"),
        );
    });
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        [ClientEvent::RpcPending, server(1, 1)]
    );
    let mut message = String::new();
    assert!(tapped.tap.take_rpc(&mut message));
    assert_eq!(message, r#"{"n":99999}"#);
    assert!(
        !tapped.tap.take_rpc(&mut message),
        "a taken message is gone"
    );
    assert_eq!(tapped.tap.dropped_events(), 0);
}

#[test]
fn the_rpc_slot_reuses_its_buffer() {
    let (tap, _events) = Tap::new(GameRpc::Kept);
    let slot = tap.game_rpc.as_ref().unwrap();
    let buffer = slot.latest.lock().unwrap().as_ptr();
    tap.keep_rpc(&"x".repeat(LIBLOG_MESSAGE_MAX - 1));
    tap.keep_rpc("{}");
    assert_eq!(slot.latest.lock().unwrap().as_ptr(), buffer);
}

#[test]
fn a_taken_rpc_marks_the_next_message_pending_again() {
    let (tap, events) = Tap::new(GameRpc::Kept);
    tap.keep_rpc("first");
    let mut message = String::new();
    assert!(tap.take_rpc(&mut message));
    tap.keep_rpc("second");
    assert_eq!(
        events.try_iter().collect::<Vec<_>>(),
        [ClientEvent::RpcPending, ClientEvent::RpcPending]
    );
    assert!(tap.take_rpc(&mut message));
    assert_eq!(message, "second");
}

#[test]
fn a_full_queue_counts_dropped_events_and_rpc_retries_its_marker() {
    let (tap, events) = Tap::new(GameRpc::Kept);
    for _ in 0..EVENT_QUEUE + 3 {
        tap.send(ClientEvent::Left);
    }
    tap.keep_rpc("while full");
    assert_eq!(tap.dropped_events(), 4);

    assert_eq!(events.try_iter().count(), EVENT_QUEUE);
    tap.keep_rpc("after draining");
    assert_eq!(events.try_recv(), Ok(ClientEvent::RpcPending));
    let mut message = String::new();
    assert!(tap.take_rpc(&mut message));
    assert_eq!(message, "after draining");
}

#[test]
fn ignored_game_rpc_is_never_kept_or_marked() {
    let (tap, events) = Tap::new(GameRpc::Ignored);
    let tapped = with_test_tap(tap, || {
        offer(
            "Roblox",
            &with_header("[FLog::CreatorOutput] [BloxstrapRPC] {}"),
        );
    });
    assert!(events.try_recv().is_err());
    assert!(!tapped.tap.take_rpc(&mut String::new()));
}

const GLOBAL_TAP_CHILD: &str = "ECLIPSE_TEST_CLIENT_LOG_GLOBAL_TAP_CHILD";

const GLOBAL_TAP_CHILD_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

#[test]
fn the_installed_tap_receives_events_and_explains_crashes_in_the_run_log() {
    if std::env::var_os(GLOBAL_TAP_CHILD).is_none() {
        let child = crate::bounded_child::output(
            std::process::Command::new(
                std::env::current_exe().expect("the test harness executable must have a path"),
            )
            .args([
                "--exact",
                "client_log::tests::\
                 the_installed_tap_receives_events_and_explains_crashes_in_the_run_log",
                "--test-threads=1",
            ])
            .env(GLOBAL_TAP_CHILD, "1"),
            GLOBAL_TAP_CHILD_LIMIT,
        );
        let stdout = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success() && stdout.contains("1 passed"),
            "status={:?}, stdout={stdout}, stderr={}",
            child.status,
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("eclipse-client-log-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create the record directory");
    let path = dir.join("records");
    let records = std::fs::File::create(&path).expect("create the record file");
    crate::diagnostics::init(crate::diagnostics::LogSink::Supervisor(records));
    offer("Roblox", GRAPHICS_OOM);

    let (tap, events) = Tap::new(GameRpc::Ignored);
    let Ok(tap) = install(tap) else {
        panic!("the first install succeeds");
    };
    assert!(install(Tap::new(GameRpc::Ignored).0).is_err());
    offer(
        "Roblox",
        &with_header("[FLog::Network] serverId: 128.116.0.1|54563"),
    );
    offer("Roblox", &with_header(GRAPHICS_OOM));

    assert_eq!(events.try_iter().collect::<Vec<_>>(), [server(1, 54563)]);
    assert_eq!(tap.dropped_events(), 0);
    let log = std::fs::read_to_string(&path).expect("read the run log");
    assert_eq!(
        log.matches(" ERROR eclipse::status: Roblox stopped: ")
            .count(),
        1,
        "only the crash offered after the install is explained: {log}"
    );
    assert!(
        log.contains(
            " ERROR eclipse::status: Roblox stopped: it reported that it could not allocate \
             graphics memory (1399808 bytes). If this repeats, lower the graphics quality in \
             Roblox's settings or turn on Use OpenGL ES in Eclipse Settings, and report it with \
             this log.\n"
        ),
        "{log}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
