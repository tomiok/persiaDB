//! Callers never drive the engine (ROADMAP 10.2.6; SPEC §8.2, §8.5; ADR-0064): a program that only opens, puts and
//! closes names neither `tick` nor `merge_step`, its writes are committed, and `close` returns after the background
//! threads stopped. One test in its own binary, so the process's threads are this test's alone.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use persia::{Db, Document, OpenOptions};

/// The names of this process's threads that Persia started and that are still alive: its maintenance thread and the
/// engine's merge workers.
///
/// A joined thread can still be listed for a moment: glibc's `pthread_join` returns when the kernel clears the
/// thread's tid, early in its exit path, before the task leaves `/proc`. So a task that released its memory map (no
/// `VmSize` line) or is a zombie counts as gone, and a census that still finds one is taken again after a yield, a
/// bounded number of times: a thread that was left behind is parked or working, and stays listed (as in
/// `db_process.rs`).
#[cfg(target_os = "linux")]
fn persia_threads() -> Vec<String> {
    let census = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| {
                let task = task.ok()?.path();
                let name = std::fs::read_to_string(task.join("comm")).ok()?;
                let status = std::fs::read_to_string(task.join("status")).ok()?;
                let state = status
                    .lines()
                    .find_map(|l| l.strip_prefix("State:"))?
                    .trim()
                    .chars()
                    .next()?;
                let exiting = !status.contains("VmSize:") || matches!(state, 'Z' | 'X');
                (name.starts_with("persia-") && !exiting).then(|| name.trim_end().to_owned())
            })
            .collect();
        names.sort();
        names
    };
    let mut running = census();
    for _ in 0..10_000 {
        if running.is_empty() {
            break;
        }
        std::thread::yield_now();
        running = census();
    }
    running
}

#[test]
fn a_program_that_only_puts_and_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    #[cfg(target_os = "linux")]
    assert_eq!(persia_threads(), Vec::<String>::new());

    let db = Db::open(&path).unwrap();
    // One maintenance thread (the kernel keeps 15 bytes of a name), beside the engine's merge workers.
    #[cfg(target_os = "linux")]
    {
        let running = persia_threads();
        let maintenance = running
            .iter()
            .filter(|n| n.as_str() == "persia-maintena")
            .count();
        assert_eq!(maintenance, 1, "{running:?}");
    }
    let c = db
        .create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap();
    let created = db.committed_seq().unwrap();
    for i in 0..20 {
        c.put(&Document::new(format!("d{i}"))).unwrap();
    }
    db.close().unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(persia_threads(), Vec::<String>::new(), "close joined them");

    // The close committed the writes: a reader sees them in a segment, not only in the log.
    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    assert!(reader.committed_seq().unwrap() > created);
    let c = reader.collection("c").unwrap();
    for i in 0..20 {
        assert!(c.get(&format!("d{i}")).unwrap().is_some());
    }
    // A read-only database starts no thread.
    #[cfg(target_os = "linux")]
    assert_eq!(persia_threads(), Vec::<String>::new());
    reader.close().unwrap();

    // Neither is a drop's thread left behind.
    drop(Db::open(&path).unwrap());
    #[cfg(target_os = "linux")]
    assert_eq!(persia_threads(), Vec::<String>::new(), "drop joined them");
}
