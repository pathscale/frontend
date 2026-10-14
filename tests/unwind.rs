//! Serial panic-record and resume checks for the consumer-supplied catcher.
//!
//! Per-thread panic storage remains necessary for embedding programs and nagoya
//! workers. The former race checks created their own OS threads and barriers;
//! they are removed under the owner's concurrency ruling. These existing serial
//! checks remain, and examples/frontend_threads_unwind_witness.rs drives the
//! public API without a test harness, with named defects beside its runner.
//!
//! A std panic hook stands in for the no_std panic handler's record_panic call.
//! Each check installs the same hook through Once; no hook is muted or replaced.

use std::sync::Once;

use frontend::unwind_janky::{catch, install_catcher, record_panic, resume, take_last_panic};

/// Every panic a test raises on purpose starts with this, so the hook can stay quiet about them
/// and still print anything else, such as a failed assertion.
const DELIBERATE: &str = "deliberate:";

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
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
            record_panic(message);
        }));
    });
}

/// Panic with the message under catch, returning the catch outcome and recorded message.
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


