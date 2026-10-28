//! How loaded the machine is: cpu, memory and the disk agents work on. A
//! reading that cannot be taken is None, never zero, since a zero is a claim;
//! a sample needs all of them, so one missing reading leaves the whole sample
//! out of the heartbeat and the service keeps the last one it had.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub at: String,
    pub cpu: f64,
    pub memory_used: u64,
    pub memory_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

/// Cpu time is counted in ticks since boot, so how busy the machine is means
/// the ticks between two readings: the first reading yields no sample.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ticks {
    busy: u64,
    idle: u64,
}

pub struct Meter {
    disk: PathBuf,
    last: Option<Ticks>,
}

impl Meter {
    /// `disk` is any path on the filesystem agents work on; it need not exist yet.
    pub fn new(disk: PathBuf) -> Self {
        Meter { disk, last: None }
    }

    pub fn sample(&mut self) -> Option<Sample> {
        let now = ticks();
        let cpu = busy(self.last, now);
        self.last = now;
        let (memory_used, memory_total) = memory()?;
        let (disk_used, disk_total) = disk(&self.disk)?;
        Some(Sample {
            at: rfc3339(SystemTime::now()),
            cpu: cpu?,
            memory_used,
            memory_total,
            disk_used,
            disk_total,
        })
    }
}

fn busy(then: Option<Ticks>, now: Option<Ticks>) -> Option<f64> {
    let (then, now) = (then?, now?);
    let busy = now.busy.wrapping_sub(then.busy) & TICK_MASK;
    let idle = now.idle.wrapping_sub(then.idle) & TICK_MASK;
    let total = busy + idle;
    (total > 0).then(|| (busy as f64 / total as f64).clamp(0.0, 1.0))
}

/// Used is total less what is still available to a user, so space the
/// filesystem reserves, or that another volume in the same container took,
/// counts as used: what matters is the room agents have left.
fn disk(path: &Path) -> Option<(u64, u64)> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let out = Command::new("df").arg("-Pk").arg(existing).output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// POSIX `df -Pk`: a header, then the filesystem, 1024-blocks, used, available
/// and capacity. A filesystem or mount point may hold spaces, so the numbers
/// are found from the capacity column, the one ending in `%`.
fn parse_df(text: &str) -> Option<(u64, u64)> {
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    let pct = fields.iter().rposition(|f| f.ends_with('%'))?;
    let kb = |i: usize| -> Option<u64> { fields.get(i)?.parse::<u64>().ok().map(|k| k * 1024) };
    let total = kb(pct.checked_sub(3)?)?;
    let available = kb(pct - 1)?;
    (total > 0).then(|| (total.saturating_sub(available), total))
}

/// Ticks on linux are 64-bit; mach counts them in 32 bits that wrap.
#[cfg(target_os = "linux")]
const TICK_MASK: u64 = u64::MAX;
#[cfg(target_os = "macos")]
const TICK_MASK: u64 = u32::MAX as u64;

#[cfg(target_os = "linux")]
fn ticks() -> Option<Ticks> {
    parse_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// The `cpu` line of /proc/stat: user nice system idle iowait irq softirq
/// steal, then guest time already counted in user. Waiting on io is idle.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(text: &str) -> Option<Ticks> {
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let n: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|f| f.parse().ok())
        .collect::<Option<_>>()?;
    if n.len() < 5 {
        return None;
    }
    let idle = n[3] + n[4];
    Some(Ticks {
        busy: n.iter().sum::<u64>() - idle,
        idle,
    })
}

#[cfg(target_os = "linux")]
fn memory() -> Option<(u64, u64)> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Used is what is not available, so the page cache the kernel would give
/// back on demand is not counted as taken.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        Some(kb * 1024)
    };
    let total = field("MemTotal:")?;
    let available = field("MemAvailable:")?;
    (total > 0).then(|| (total.saturating_sub(available), total))
}

#[cfg(target_os = "macos")]
mod mach {
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::OnceLock;

    pub const HOST_CPU_LOAD_INFO: c_int = 3;
    pub const HOST_VM_INFO64: c_int = 4;

    /// `vm_statistics64` from <mach/vm_statistics.h>, whose layout is ABI.
    #[repr(C, align(8))]
    #[derive(Default)]
    pub struct VmStatistics64 {
        pub free_count: u32,
        pub active_count: u32,
        pub inactive_count: u32,
        pub wire_count: u32,
        pub zero_fill_count: u64,
        pub reactivations: u64,
        pub pageins: u64,
        pub pageouts: u64,
        pub faults: u64,
        pub cow_faults: u64,
        pub lookups: u64,
        pub hits: u64,
        pub purges: u64,
        pub purgeable_count: u32,
        pub speculative_count: u32,
        pub decompressions: u64,
        pub compressions: u64,
        pub swapins: u64,
        pub swapouts: u64,
        pub compressor_page_count: u32,
        pub throttled_count: u32,
        pub external_page_count: u32,
        pub internal_page_count: u32,
        pub total_uncompressed_pages_in_compressor: u64,
    }

    extern "C" {
        fn mach_host_self() -> u32;
        pub fn host_statistics(
            host: u32,
            flavor: c_int,
            info: *mut c_int,
            count: *mut u32,
        ) -> c_int;
        pub fn host_statistics64(
            host: u32,
            flavor: c_int,
            info: *mut c_int,
            count: *mut u32,
        ) -> c_int;
        pub fn host_page_size(host: u32, size: *mut usize) -> c_int;
        pub fn sysctlbyname(
            name: *const c_char,
            old: *mut c_void,
            len: *mut usize,
            new: *const c_void,
            new_len: usize,
        ) -> c_int;
    }

    /// Each call to `mach_host_self` takes another reference to the port, so
    /// it is asked once rather than on every heartbeat.
    pub fn host() -> u32 {
        static HOST: OnceLock<u32> = OnceLock::new();
        *HOST.get_or_init(|| unsafe { mach_host_self() })
    }
}

#[cfg(target_os = "macos")]
fn ticks() -> Option<Ticks> {
    // user, system, idle, nice
    let mut t = [0u32; 4];
    let mut count = t.len() as u32;
    let ok = unsafe {
        mach::host_statistics(
            mach::host(),
            mach::HOST_CPU_LOAD_INFO,
            t.as_mut_ptr().cast(),
            &mut count,
        )
    } == 0;
    ok.then(|| Ticks {
        busy: t[0].wrapping_add(t[1]).wrapping_add(t[3]) as u64,
        idle: t[2] as u64,
    })
}

/// Used is what Activity Monitor calls Memory Used: app memory, wired and
/// compressed. macOS keeps free memory filled with file cache it gives back on
/// demand, so total less free would read as always nearly full.
#[cfg(target_os = "macos")]
fn memory() -> Option<(u64, u64)> {
    let mut total = 0u64;
    let mut len = std::mem::size_of::<u64>();
    let ok = unsafe {
        mach::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut len,
            std::ptr::null(),
            0,
        )
    } == 0;
    if !ok || total == 0 {
        return None;
    }
    let mut page = 0usize;
    if unsafe { mach::host_page_size(mach::host(), &mut page) } != 0 || page == 0 {
        return None;
    }
    let mut vm = mach::VmStatistics64::default();
    let mut count = (std::mem::size_of::<mach::VmStatistics64>() / 4) as u32;
    let ok = unsafe {
        mach::host_statistics64(
            mach::host(),
            mach::HOST_VM_INFO64,
            (&mut vm as *mut mach::VmStatistics64).cast(),
            &mut count,
        )
    } == 0;
    if !ok {
        return None;
    }
    let pages = (vm.internal_page_count as u64).saturating_sub(vm.purgeable_count as u64)
        + vm.wire_count as u64
        + vm.compressor_page_count as u64;
    Some(((pages * page as u64).min(total), total))
}

/// Now, as the service's `date-time` reads it.
pub fn now() -> String {
    rfc3339(SystemTime::now())
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
    use std::time::Duration;

    #[test]
    fn this_machine_is_measured() {
        let mut meter = Meter::new(std::env::temp_dir().join("agent-host-not-made-yet"));
        assert_eq!(
            meter.sample(),
            None,
            "the first reading has nothing to compare cpu with"
        );
        std::thread::sleep(Duration::from_millis(200));
        let sample = meter
            .sample()
            .expect("a second reading samples the machine");
        assert!((0.0..=1.0).contains(&sample.cpu), "{sample:?}");
        assert!(sample.memory_total > 64 << 20, "{sample:?}");
        assert!(sample.memory_used > 0, "{sample:?}");
        assert!(sample.memory_used <= sample.memory_total, "{sample:?}");
        assert!(sample.disk_total > 0, "{sample:?}");
        assert!(sample.disk_used <= sample.disk_total, "{sample:?}");
    }

    #[test]
    fn cpu_is_the_busy_share_of_the_ticks_between_readings() {
        let then = Some(Ticks {
            busy: 100,
            idle: 300,
        });
        let now = Some(Ticks {
            busy: 130,
            idle: 370,
        });
        assert_eq!(busy(then, now), Some(0.3));
        assert_eq!(busy(None, now), None);
        assert_eq!(busy(now, now), None, "no time passed, so no reading");
    }

    #[test]
    fn ticks_that_wrapped_still_count_forward() {
        let then = Some(Ticks {
            busy: TICK_MASK - 9,
            idle: 0,
        });
        let now = Some(Ticks { busy: 10, idle: 20 });
        assert_eq!(busy(then, now), Some(0.5));
    }

    #[test]
    fn proc_stat_counts_io_wait_as_idle() {
        let stat = "cpu  10 0 10 60 20 0 0 0 5 0\ncpu0 1 2 3 4 5 6 7 8\n";
        assert_eq!(parse_stat(stat), Some(Ticks { busy: 20, idle: 80 }));
        assert_eq!(parse_stat("intr 1 2 3\n"), None);
    }

    #[test]
    fn meminfo_used_is_what_is_not_available() {
        let info = "MemTotal:       16000 kB\nMemFree:  1000 kB\nMemAvailable:   6000 kB\n";
        assert_eq!(parse_meminfo(info), Some((10000 * 1024, 16000 * 1024)));
        assert_eq!(
            parse_meminfo("MemTotal: 16000 kB\n"),
            None,
            "no guess without MemAvailable"
        );
    }

    #[test]
    fn df_is_read_from_the_capacity_column() {
        let mac = "Filesystem 1024-blocks Used Available Capacity  Mounted on\n\
                   /dev/disk3s5 971350180 401234567 500000000    45%    /System/Volumes/Data\n";
        assert_eq!(
            parse_df(mac),
            Some(((971350180 - 500000000) * 1024, 971350180 * 1024))
        );
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      map auto home 100 40 50 45% /Volumes/My Disk\n";
        assert_eq!(parse_df(spaced), Some((50 * 1024, 100 * 1024)));
        assert_eq!(parse_df("df: /nope: No such file or directory\n"), None);
    }

    #[test]
    fn times_are_utc_to_the_millisecond() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_793_102_400_123);
        assert_eq!(rfc3339(t), "2026-10-27T12:00:00.123Z");
        assert_eq!(
            rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
    }
}
