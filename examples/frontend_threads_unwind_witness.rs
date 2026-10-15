//! A real input through the installed panic catcher and panic-record API.
//! Reads one message from stdin and emits its successfully preserved value.

use std::io::{self, Read};

use frontend::unwind_janky::{catch, install_catcher, record_panic, resume, take_last_panic};

fn catcher(f: &mut dyn FnMut()) -> Result<(), frontend::unwind_janky::Payload> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
}

fn main() {
    install_catcher(catcher);
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("frontend_threads_unwind_stdin");
    let input = input.trim_end();
    assert!(!input.is_empty(), "frontend_threads_unwind_nonempty_input");
    let expected = format!("deliberate: {input}");
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let message = if let Some(message) = payload.downcast_ref::<String>() {
            message.clone()
        } else if let Some(message) = payload.downcast_ref::<&str>() {
            (*message).to_string()
        } else {
            String::from("<non-string payload>")
        };
        if !message.starts_with("deliberate:") && message != "resuming a caught panic" {
            eprintln!("{info}");
        }
        record_panic(message);
    }));

    let caught = catch::<_, ()>(|| panic!("{expected}"));
    assert!(caught.is_err(), "frontend_threads_initial_catch");
    assert_eq!(
        take_last_panic().as_deref(),
        Some(expected.as_str()),
        "frontend_threads_recorded_input"
    );
    assert_eq!(take_last_panic(), None, "frontend_threads_record_consumed");

    let resumed = catch::<_, ()>(|| {
        let payload =
            catch::<_, ()>(|| panic!("{expected}")).expect_err("frontend_threads_inner_catch");
        resume(payload)
    });
    assert!(resumed.is_err(), "frontend_threads_resume_unwinds");
    assert_eq!(
        take_last_panic().as_deref(),
        Some(expected.as_str()),
        "frontend_threads_resume_original"
    );
    assert_eq!(take_last_panic(), None, "frontend_threads_resume_consumed");

    let next = format!("deliberate: next {input}");
    assert!(catch::<_, ()>(|| panic!("{next}")).is_err(), "frontend_threads_next_catch");
    assert_eq!(take_last_panic().as_deref(), Some(next.as_str()), "frontend_threads_fresh_panic");
    assert_eq!(take_last_panic(), None, "frontend_threads_fresh_consumed");
    println!("frontend_threads_unwind: {input}");
}
