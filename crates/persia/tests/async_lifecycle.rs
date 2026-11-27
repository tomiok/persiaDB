//! The async pool's threads (ROADMAP 10.1.11; ADR-0064 item 12): none before the first job, the configured number
//! after it, separate from the maintenance thread, none left by a start the OS refused a thread of (ROADMAP
//! 10.1.27), and none once `AsyncDb::close` resolved. One test in its own binary, so the process's threads are this
//! test's alone.
#![cfg(feature = "async")]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)] // Tests: inputs are fixed.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use persia::{AsyncDb, Document, OpenOptions};

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// The names of this process's live threads that start with `prefix`, sorted.
///
/// A joined thread can still be listed for a moment: glibc's `pthread_join` returns when the kernel clears the
/// thread's tid, early in its exit path, before the task leaves `/proc`. So a task that released its memory map (no
/// `VmSize` line) or is a zombie counts as gone, and a census that differs from `expected` is taken again after a
/// yield, a bounded number of times: a thread that was left behind is parked or working, and stays listed (as in
/// `lifecycle.rs`).
#[cfg(target_os = "linux")]
fn threads(prefix: &str, expected: &[&str]) -> Vec<String> {
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
                (name.starts_with(prefix) && !exiting).then(|| name.trim_end().to_owned())
            })
            .collect();
        names.sort();
        names
    };
    let mut running = census();
    for _ in 0..10_000 {
        if running == expected {
            break;
        }
        std::thread::yield_now();
        running = census();
    }
    running
}

#[test]
fn the_pool_starts_on_first_use_all_or_nothing_and_close_joins_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    let setup = persia::Db::open(&path).unwrap();
    setup
        .create_collection_json(r#"{"name": "c", "fields": {}}"#)
        .unwrap();
    setup.close().unwrap();

    let options = OpenOptions::new().async_threads(2);
    let db = block_on(AsyncDb::open_with(options, &path)).unwrap();
    // The open's own thread is joined, the pool has not started, the maintenance thread runs (the kernel keeps 15
    // bytes of a name).
    #[cfg(target_os = "linux")]
    {
        assert_eq!(threads("persia-async", &[]), Vec::<String>::new());
        assert_eq!(
            threads("persia-maint", &["persia-maintena"]),
            ["persia-maintena"]
        );
    }
    let c = db.collection("c").unwrap();
    block_on(c.put(Document::new("a"))).unwrap();
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let pool = ["persia-async-0", "persia-async-1"];
    #[cfg(target_os = "linux")]
    assert_eq!(threads("persia-async", &pool), pool);
    block_on(c.put(Document::new("b"))).unwrap();
    // Still the same two: the pool starts once.
    #[cfg(target_os = "linux")]
    assert_eq!(threads("persia-async", &pool), pool);

    block_on(db.close()).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(
        threads("persia-", &[]),
        Vec::<String>::new(),
        "close joined them all"
    );

    let reader = OpenOptions::new().read_only(true).open(&path).unwrap();
    let c = reader.collection("c").unwrap();
    assert!(c.get("a").unwrap().is_some() && c.get("b").unwrap().is_some());
    drop(reader);

    // A start the OS refuses a thread of (ROADMAP 10.1.27): the job fails, and the threads that start made are joined
    // before it returns, so none is left. The next job starts the whole pool.
    let options = OpenOptions::new()
        .async_threads(3)
        .async_refuse_thread(2)
        .read_only(true);
    let db = block_on(AsyncDb::open_with(options, &path)).unwrap();
    let c = db.collection("c").unwrap();
    assert_eq!(
        block_on(c.get("a")).unwrap_err().code(),
        persia::ErrorCode::Io
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        threads("persia-async", &[]),
        Vec::<String>::new(),
        "the two threads the refused start made were joined"
    );
    assert!(block_on(c.get("a")).unwrap().is_some());
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let pool = ["persia-async-0", "persia-async-1", "persia-async-2"];
    #[cfg(target_os = "linux")]
    assert_eq!(threads("persia-async", &pool), pool);
    block_on(db.close()).unwrap();
    #[cfg(target_os = "linux")]
    assert_eq!(threads("persia-", &[]), Vec::<String>::new());
}
