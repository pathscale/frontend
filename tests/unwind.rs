//! `unwind_janky`'s panic record, driven from two threads at once.
//!
//! The record used to be one slot for the whole process, so a thread that caught a panic could
//! read the message another thread's panic left there, and one thread's `resume` could make a
//! different thread's next panic go unrecorded. These tests pin down that each thread now reads
//! exactly its own panic.
//!
//! A `std` program, like `tests/syntax.rs`, so it installs `std::panic::catch_unwind` as the
//! catcher. It also stands in for the `no_std` panic handler, which is what calls `record_panic`
//! in a real build: a `std` panic hook does the same job here, on the same thread, before the
//! unwind starts.
//!
//! The hook is global to this test binary, and the harness runs these tests on several threads
//! at once. Every test here installs the same hook through one `Once`, and no other test file
//! shares this binary, so there is never a second hook to fight with. Every count is fixed; no
//! test waits on a clock.

use std::cell::Cell;
use std::sync::{Arc, Barrier, Once};

use frontend::unwind_janky::{catch, install_catcher, record_panic, resume, take_last_panic};

/// Iterations per thread in the race test. Enough for the two threads to interleave many times,
/// few enough that the test is quick even on a loaded machine.
const ITERATIONS: usize = 1000;

/// Every panic a test raises on purpose starts with this, so the hook can stay quiet about them
/// and still print anything else, such as a failed assertion.
const DELIBERATE: &str = "deliberate:";

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

std::thread_local! {
    /// Set while a test wants its thread's panics to reach no `record_panic` at all, which is the
    /// state between `resume` setting its flag and the panic handler running. Per thread, so one
    /// test muting itself mutes nobody else.
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

/// Install the catcher and the recording hook. Every test calls it; both are idempotent.
fn ready() {
    static HOOK: Once = Once::new();
    install_catcher(catcher);
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let payload = info.payload();
            let message = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                String::from("<non-string payload>")
            };
            if !message.starts_with(DELIBERATE) && message != "resuming a caught panic" {
                eprintln!("{info}");
            }
            if !MUTED.with(Cell::get) {
                record_panic(message);
            }
        }));
    });
}

/// Panic with `message` under `catch`, and return whether it was caught and what the record says
/// afterwards. It asserts nothing itself, so a thread that must still reach a barrier can call it.
fn raise_and_take(message: &str) -> (bool, Option<String>) {
    // The closure never returns, so `R` is named rather than left to never-type fallback.
    let caught = catch::<_, ()>(|| panic!("{message}")).is_err();
    (caught, take_last_panic())
}

/// [`raise_and_take`], asserting the catch.
fn panic_and_take(message: String) -> Option<String> {
    let (caught, taken) = raise_and_take(&message);
    assert!(caught, "catch did not see the panic");
    taken
}

#[test]
fn each_thread_reads_its_own_panic() {
    ready();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|id| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..ITERATIONS {
                    let expected = format!("{DELIBERATE} thread {id} iter {i}");
                    let got = panic_and_take(expected.clone());
                    assert_eq!(got.as_deref(), Some(expected.as_str()), "thread {id} iter {i}");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("a worker failed; its assertion is printed above");
    }
}

#[test]
fn a_read_record_is_gone() {
    ready();
    let message = format!("{DELIBERATE} read once");
    assert_eq!(panic_and_take(message.clone()), Some(message));
    assert_eq!(take_last_panic(), None);
}

#[test]
fn resume_keeps_the_original_message() {
    ready();
    let original = format!("{DELIBERATE} the original");
    let outer = catch::<_, ()>(|| {
        let payload =
            catch::<_, ()>(|| panic!("{original}")).expect_err("the inner catch saw no panic");
        resume(payload)
    });
    assert!(outer.is_err(), "resume did not unwind");
    assert_eq!(take_last_panic(), Some(original));
    assert_eq!(take_last_panic(), None);
}

/// One thread's `resume` must not swallow another thread's next panic.
///
/// Thread A resumes with its hook muted, so its `resume` flag is set and nothing has consumed it
/// yet: the window in which a process-wide flag used to make the *next* panic anywhere go
/// unrecorded. Thread B then panics inside that window and must find its own message. Last, A
/// shows the flag is still its own: its next recorded panic is declined as the re-raise, and the
/// one after that is recorded.
#[test]
fn resume_on_one_thread_does_not_suppress_another() {
    ready();
    let a_resumed = Arc::new(Barrier::new(2));
    let b_recorded = Arc::new(Barrier::new(2));

    let a = {
        let a_resumed = Arc::clone(&a_resumed);
        let b_recorded = Arc::clone(&b_recorded);
        std::thread::spawn(move || {
            MUTED.with(|m| m.set(true));
            let caught = catch::<_, ()>(|| resume(Box::new(()))).is_err();
            MUTED.with(|m| m.set(false));
            let leaked = take_last_panic();
            a_resumed.wait();
            b_recorded.wait();
            assert!(caught, "resume did not unwind");
            assert_eq!(leaked, None, "a muted panic was recorded");
            // The flag `resume` set is still here, so this panic is taken for the re-raise.
            assert_eq!(panic_and_take(format!("{DELIBERATE} A declined")), None);
            let next = format!("{DELIBERATE} A recorded");
            assert_eq!(panic_and_take(next.clone()), Some(next));
        })
    };
    let b = std::thread::spawn(move || {
        a_resumed.wait();
        let mine = format!("{DELIBERATE} B during A's resume");
        let (caught, got) = raise_and_take(&mine);
        b_recorded.wait();
        assert!(caught, "catch did not see B's panic");
        assert_eq!(got, Some(mine));
    });
    // Every assertion runs after both barriers, so a failure on either thread cannot leave the
    // other waiting on one.
    a.join().expect("thread A failed; its assertion is printed above");
    b.join().expect("thread B failed; its assertion is printed above");
}
