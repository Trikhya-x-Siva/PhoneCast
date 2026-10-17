//! CPU and RAM use of this process, shown in the app so the streamer can
//! see what the helper costs.

use std::time::Instant;

pub struct Usage {
    last_cpu: f64,
    last_at: Instant,
    cores: f64,
}

impl Default for Usage {
    fn default() -> Self {
        Self::new()
    }
}

impl Usage {
    pub fn new() -> Self {
        Self {
            last_cpu: cpu_seconds(),
            last_at: Instant::now(),
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as f64)
                .unwrap_or(1.0),
        }
    }

    /// (CPU % of the whole machine since the last call, resident MB).
    pub fn sample(&mut self) -> (f64, f64) {
        let now = Instant::now();
        let cpu = cpu_seconds();
        let wall = now.duration_since(self.last_at).as_secs_f64().max(1e-3);
        let pct = ((cpu - self.last_cpu) / wall / self.cores * 100.0).clamp(0.0, 100.0);
        self.last_cpu = cpu;
        self.last_at = now;
        (
            (pct * 10.0).round() / 10.0,
            (rss_bytes() as f64 / 1_048_576.0).round(),
        )
    }
}

#[cfg(windows)]
fn cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    if ok == 0 {
        return 0.0;
    }
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    ft(k) + ft(u)
}

#[cfg(windows)]
fn rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok == 0 {
        0
    } else {
        pmc.WorkingSetSize as u64
    }
}

#[cfg(not(windows))]
fn cpu_seconds() -> f64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|s| {
            let f: Vec<&str> = s.rsplit(')').next()?.split_whitespace().collect();
            let u: f64 = f.get(11)?.parse().ok()?;
            let k: f64 = f.get(12)?.parse().ok()?;
            Some((u + k) / 100.0)
        })
        .unwrap_or(0.0)
}

#[cfg(not(windows))]
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages * 4096)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sample_is_sane() {
        let mut u = super::Usage::new();
        let (cpu, mb) = u.sample();
        assert!((0.0..=100.0).contains(&cpu));
        assert!(mb >= 0.0);
    }
}
