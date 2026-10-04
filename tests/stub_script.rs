#[path = "support/stub_script.rs"]
mod stub_script;

use std::path::PathBuf;
use std::process::Command;

const WRITERS: usize = 8;

const STUBS_PER_WRITER: usize = 100;

#[test]
fn stubs_written_while_other_threads_start_processes_always_run() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("stub-script-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("create the stub directory");

    let failures: Vec<String> = std::thread::scope(|scope| {
        let writers: Vec<_> = (0..WRITERS)
            .map(|writer| {
                let dir = &dir;
                scope.spawn(move || {
                    (0..STUBS_PER_WRITER)
                        .filter_map(|index| {
                            let stub = dir.join(format!("{writer}-{index}"));
                            stub_script::write(&stub, "#!/bin/sh\nexit 0\n");
                            match Command::new(&stub).status() {
                                Ok(status) if status.success() => None,
                                ran => Some(format!("{}: {ran:?}", stub.display())),
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        writers
            .into_iter()
            .flat_map(|writer| writer.join().expect("a writer thread finishes"))
            .collect()
    });
    std::fs::remove_dir_all(&dir).ok();

    assert!(failures.is_empty(), "{failures:#?}");
}
