//! Snapshots of per-process CPU and GPU time, read from `/proc`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Identifies a process across PID reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcKey {
    pub pid: u32,
    /// Start time in clock ticks since boot.
    pub start: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GpuCounters {
    /// Sum of `drm-engine-<name>: <N> ns` (i915, amdgpu, msm, panfrost...).
    pub busy_ns: u64,
    /// Sum of `drm-cycles-<name>` (xe driver).
    pub cycles: u64,
    /// `drm-total-cycles-<name>` (xe driver). It is the same GPU timestamp counter
    /// for every engine, so we keep the largest instead of summing; busy cycles are
    /// summed, like busy nanoseconds on other drivers.
    pub total_cycles: u64,
}

impl GpuCounters {
    fn add(&mut self, o: &GpuCounters) {
        self.busy_ns += o.busy_ns;
        self.cycles += o.cycles;
        self.total_cycles = self.total_cycles.max(o.total_cycles);
    }
}

#[derive(Debug, Clone)]
pub struct ProcSample {
    pub key: ProcKey,
    pub comm: String,
    pub cgroup: String,
    /// utime + stime, in clock ticks.
    pub cpu_ticks: u64,
    pub gpu: GpuCounters,
}

pub struct Snapshot {
    /// Seconds since boot when the snapshot was taken.
    pub uptime_s: f64,
    pub procs: Vec<ProcSample>,
}

pub fn clock_ticks_per_second() -> f64 {
    // SAFETY: sysconf has no preconditions.
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 {
        t as f64
    } else {
        100.0
    }
}

pub fn snapshot() -> Snapshot {
    snapshot_in(Path::new("/proc"))
}

pub fn snapshot_in(proc_root: &Path) -> Snapshot {
    let uptime_s = fs::read_to_string(proc_root.join("uptime"))
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);

    let mut procs = Vec::new();
    // DRM clients can be shared between processes (fd passing); count each once.
    let mut seen_drm_clients = HashSet::new();

    if let Ok(entries) = fs::read_dir(proc_root) {
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let dir = entry.path();
            let Some(stat) = fs::read_to_string(dir.join("stat")).ok().and_then(|s| parse_stat(&s)) else {
                continue;
            };
            let cgroup = fs::read_to_string(dir.join("cgroup"))
                .ok()
                .and_then(|s| crate::apps::cgroup_path(&s).map(str::to_string))
                .unwrap_or_default();
            let gpu = read_gpu(&dir, &mut seen_drm_clients);
            procs.push(ProcSample {
                key: ProcKey { pid, start: stat.start },
                comm: stat.comm,
                cgroup,
                cpu_ticks: stat.utime + stat.stime,
                gpu,
            });
        }
    }
    Snapshot { uptime_s, procs }
}

struct Stat {
    comm: String,
    utime: u64,
    stime: u64,
    start: u64,
}

/// Parses `/proc/<pid>/stat`. The command name is in parentheses and may itself
/// contain spaces or parentheses, so split on the *last* `)`.
fn parse_stat(s: &str) -> Option<Stat> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let comm = s.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = s.get(close + 2..)?.split_whitespace().collect();
    // `rest[0]` is field 3 (state); utime/stime/starttime are fields 14, 15 and 22.
    Some(Stat {
        comm,
        utime: rest.get(11)?.parse().ok()?,
        stime: rest.get(12)?.parse().ok()?,
        start: rest.get(19)?.parse().ok()?,
    })
}

/// Sums the GPU engine counters of every DRM file descriptor the process holds.
/// Only works for processes we are allowed to inspect (our own user's), which is
/// where nearly all GPU use happens anyway.
fn read_gpu(proc_dir: &Path, seen: &mut HashSet<(String, String)>) -> GpuCounters {
    let mut total = GpuCounters::default();
    let Ok(fds) = fs::read_dir(proc_dir.join("fd")) else {
        return total;
    };
    for fd in fds.flatten() {
        // Cheap filter before reading fdinfo: only DRM device nodes.
        let is_drm = fs::read_link(fd.path()).map(|t| t.starts_with("/dev/dri/")).unwrap_or(false);
        if !is_drm {
            continue;
        }
        let Ok(info) = fs::read_to_string(proc_dir.join("fdinfo").join(fd.file_name())) else {
            continue;
        };
        if let Some((client, counters)) = parse_drm_fdinfo(&info) {
            if seen.insert(client) {
                total.add(&counters);
            }
        }
    }
    total
}

/// Returns the `(pdev, client-id)` pair and the engine counters of a DRM fdinfo.
pub fn parse_drm_fdinfo(info: &str) -> Option<((String, String), GpuCounters)> {
    let mut pdev = String::new();
    let mut client = None;
    let mut c = GpuCounters::default();
    for line in info.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let first_num = || v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        if k == "drm-client-id" {
            client = Some(v.to_string());
        } else if k == "drm-pdev" {
            pdev = v.to_string();
        } else if let Some(engine) = k.strip_prefix("drm-engine-") {
            if !engine.starts_with("capacity-") && v.ends_with("ns") {
                c.busy_ns += first_num();
            }
        } else if k.starts_with("drm-total-cycles-") {
            c.total_cycles = c.total_cycles.max(first_num());
        } else if k.starts_with("drm-cycles-") {
            c.cycles += first_num();
        }
    }
    Some(((pdev, client?), c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_with_weird_comm() {
        let s = "1234 (Web Content (x)) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 30 0 98765 0 0";
        let st = parse_stat(s).unwrap();
        assert_eq!(st.comm, "Web Content (x)");
        assert_eq!(st.utime, 250);
        assert_eq!(st.stime, 50);
        assert_eq!(st.start, 98765);
    }

    #[test]
    fn i915_fdinfo() {
        let info = "drm-driver:\ti915\ndrm-client-id:\t14\ndrm-pdev:\t0000:00:02.0\n\
            drm-total-system0:\t1985040 KiB\ndrm-engine-render:\t1662126324248 ns\n\
            drm-engine-copy:\t0 ns\ndrm-engine-video:\t10 ns\ndrm-engine-capacity-video:\t2\n";
        let ((pdev, id), c) = parse_drm_fdinfo(info).unwrap();
        assert_eq!((pdev.as_str(), id.as_str()), ("0000:00:02.0", "14"));
        assert_eq!(c.busy_ns, 1662126324258);
    }

    #[test]
    fn xe_fdinfo() {
        let info = "drm-client-id: 3\ndrm-cycles-rcs: 500\ndrm-total-cycles-rcs: 1000\ndrm-cycles-bcs: 0\ndrm-total-cycles-bcs: 1000\n";
        let (_, c) = parse_drm_fdinfo(info).unwrap();
        assert_eq!((c.cycles, c.total_cycles), (500, 1000));
    }

    #[test]
    fn fdinfo_without_client_is_ignored() {
        assert!(parse_drm_fdinfo("pos: 0\nflags: 02\n").is_none());
    }

    #[test]
    fn snapshot_of_self() {
        let snap = snapshot();
        assert!(snap.uptime_s > 0.0);
        let me = std::process::id();
        assert!(snap.procs.iter().any(|p| p.key.pid == me));
    }
}
