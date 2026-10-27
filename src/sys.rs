//! The two things std does not offer, declared directly rather than pulling
//! in libc for them. Both values are the same on Linux and macOS.

use std::ffi::{c_char, c_int, CStr};

const SIGHUP: c_int = 1;
const SIG_IGN: usize = 1;

extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
    fn gethostname(name: *mut c_char, len: usize) -> c_int;
}

/// A host started from a terminal must outlive it. Closing the terminal sends
/// SIGHUP, whose default is to exit.
pub fn ignore_hangup() {
    unsafe {
        signal(SIGHUP, SIG_IGN);
    }
}

pub fn hostname() -> Option<String> {
    let mut buf = [0 as c_char; 256];
    if unsafe { gethostname(buf.as_mut_ptr(), buf.len() - 1) } != 0 {
        return None;
    }
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    Some(name.split('.').next()?.to_string()).filter(|n| !n.is_empty())
}

/// Once the terminal is gone stderr may be closed, and `eprintln!` panics on a
/// failed write. Losing a log line is fine; losing the host over one is not.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}
