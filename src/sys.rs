//! What std does not offer: a few calls declared directly rather than pulling
//! in libc for them (every value here is the same on Linux and macOS), and the
//! time as the service reads it.

use std::ffi::{c_char, c_int, CStr};
use std::time::{Duration, SystemTime};

const SIGHUP: c_int = 1;
pub const SIGKILL: c_int = 9;
pub const SIGTERM: c_int = 15;
const SIG_IGN: usize = 1;

extern "C" {
    fn signal(signum: c_int, handler: usize) -> usize;
    fn gethostname(name: *mut c_char, len: usize) -> c_int;
    fn kill(pid: c_int, sig: c_int) -> c_int;
    #[cfg(target_os = "linux")]
    fn prctl(option: c_int, ...) -> c_int;
}

/// Sends `sig` to every process in the group `group` leads. A group already
/// gone is nothing to signal.
pub fn signal_group(group: u32, sig: c_int) {
    if let Ok(group) = c_int::try_from(group) {
        unsafe {
            kill(-group, sig);
        }
    }
}

/// For a child, between fork and exec: told to stop when the host ends, so a
/// session never runs on unheard and then meets the one the next host resumes.
/// macOS has no such call, and there a session ends when it next writes to
/// the host that is gone.
pub fn die_with_parent() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        const PR_SET_PDEATHSIG: c_int = 1;
        if unsafe { prctl(PR_SET_PDEATHSIG, SIGTERM as std::ffi::c_ulong) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
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

/// Now, as the service's `date-time` reads it.
pub fn now() -> String {
    rfc3339(SystemTime::now())
}

/// `secs` after the epoch, as the service's `date-time` reads it; None when
/// it is no such instant.
pub fn epoch(secs: f64) -> Option<String> {
    let d = Duration::try_from_secs_f64(secs).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(d).map(rfc3339)
}

/// UTC to the millisecond, as the service's `date-time` reads it.
fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        d.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc_to_the_millisecond() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_793_102_400_123);
        assert_eq!(rfc3339(t), "2026-10-27T12:00:00.123Z");
        assert_eq!(
            rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            epoch(1_793_965_200.0).as_deref(),
            Some("2026-11-06T11:40:00.000Z")
        );
        assert_eq!(epoch(-1.0), None);
        assert_eq!(epoch(f64::NAN), None);
    }
}
