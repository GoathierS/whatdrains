//! Turns successive process snapshots and battery readings into per-app energy.
//!
//! Linux laptops do not expose per-process power, so we estimate it:
//!
//! 1. The battery tells us the whole machine's draw `P` while unplugged.
//! 2. A rolling low percentile of `P` is the *baseline* `P0`: what the machine draws
//!    when nothing much is running (screen backlight, idle SoC, Wi-Fi...). That share
//!    goes to the "Display & idle system" bucket.
//! 3. The rest, `P - P0`, is split between apps in proportion to the CPU and GPU time
//!    they used during the interval.
//!
//! This is the same kind of model macOS and Android use for their "energy impact"
//! views. It is an estimate, not a measurement.

use std::collections::{HashMap, VecDeque};

use crate::apps::{classify, AppId};
use crate::procs::{GpuCounters, ProcKey, Snapshot};

/// How much one second of GPU engine time weighs compared to one CPU second.
pub const GPU_WEIGHT: f64 = 1.0;
/// Window used to estimate the idle baseline.
const BASELINE_WINDOW_S: f64 = 30.0 * 60.0;
const BASELINE_PERCENTILE: f64 = 0.10;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cpu_s: f64,
    pub gpu_s: f64,
    /// Estimated energy in joules; 0 when the battery could not be measured
    /// (plugged in, or no battery).
    pub energy_j: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.cpu_s += o.cpu_s;
        self.gpu_s += o.gpu_s;
        self.energy_j += o.energy_j;
    }

    fn weight(&self) -> f64 {
        self.cpu_s + GPU_WEIGHT * self.gpu_s
    }
}

#[derive(Debug, Default)]
pub struct Interval {
    pub dt_s: f64,
    pub apps: HashMap<AppId, Usage>,
}

pub struct Attributor {
    ticks_per_s: f64,
    prev: HashMap<ProcKey, (u64, GpuCounters)>,
    prev_uptime: Option<f64>,
    baseline: Baseline,
}

impl Attributor {
    pub fn new(ticks_per_s: f64) -> Self {
        Self { ticks_per_s, prev: HashMap::new(), prev_uptime: None, baseline: Baseline::default() }
    }

    /// Feeds a new snapshot. `power_w` is the measured system draw while running on
    /// battery, or `None` when it is unknown (plugged in, no battery).
    pub fn step(&mut self, snap: &Snapshot, power_w: Option<f64>) -> Interval {
        let mut out = Interval::default();
        let Some(prev_uptime) = self.prev_uptime else {
            self.remember(snap);
            return out;
        };
        let dt = snap.uptime_s - prev_uptime;
        if dt <= 0.0 {
            self.remember(snap);
            return out;
        }
        out.dt_s = dt;

        for p in &snap.procs {
            let (cpu_ticks, gpu) = match self.prev.get(&p.key) {
                Some((c, g)) => (p.cpu_ticks.saturating_sub(*c), gpu_delta(&p.gpu, g)),
                // Started during this interval: everything it used belongs here.
                None if p.key.start as f64 / self.ticks_per_s >= prev_uptime => {
                    (p.cpu_ticks, GpuCounters { busy_ns: p.gpu.busy_ns, ..Default::default() })
                }
                None => continue,
            };
            let mut gpu_s = gpu.busy_ns as f64 / 1e9;
            if gpu.total_cycles > 0 {
                gpu_s += gpu.cycles as f64 / gpu.total_cycles as f64 * dt;
            }
            let usage = Usage { cpu_s: cpu_ticks as f64 / self.ticks_per_s, gpu_s, energy_j: 0.0 };
            if usage.weight() > 0.0 {
                out.apps.entry(classify(&p.cgroup, &p.comm)).or_default().add(&usage);
            }
        }

        if let Some(p) = power_w.filter(|p| p.is_finite() && *p > 0.0) {
            let p0 = self.baseline.push(snap.uptime_s, p);
            distribute(&mut out.apps, p * dt, p0.min(p) * dt);
        }

        self.remember(snap);
        out
    }

    fn remember(&mut self, snap: &Snapshot) {
        self.prev = snap.procs.iter().map(|p| (p.key, (p.cpu_ticks, p.gpu))).collect();
        self.prev_uptime = Some(snap.uptime_s);
    }
}

fn gpu_delta(cur: &GpuCounters, prev: &GpuCounters) -> GpuCounters {
    GpuCounters {
        busy_ns: cur.busy_ns.saturating_sub(prev.busy_ns),
        cycles: cur.cycles.saturating_sub(prev.cycles),
        total_cycles: cur.total_cycles.saturating_sub(prev.total_cycles),
    }
}

/// Splits `total_j` between the idle bucket (`idle_j`) and the apps, by weight.
fn distribute(apps: &mut HashMap<AppId, Usage>, total_j: f64, idle_j: f64) {
    let active_j = (total_j - idle_j).max(0.0);
    let total_weight: f64 = apps.values().map(Usage::weight).sum();
    let mut idle = idle_j;
    if total_weight > 0.0 {
        for u in apps.values_mut() {
            u.energy_j = active_j * u.weight() / total_weight;
        }
    } else {
        idle += active_j;
    }
    if idle > 0.0 {
        apps.entry(AppId::idle()).or_default().energy_j += idle;
    }
}

/// Rolling low-percentile estimate of the machine's idle power draw.
#[derive(Default)]
struct Baseline {
    samples: VecDeque<(f64, f64)>,
}

impl Baseline {
    fn push(&mut self, t: f64, power_w: f64) -> f64 {
        self.samples.push_back((t, power_w));
        while self.samples.front().is_some_and(|(t0, _)| t - t0 > BASELINE_WINDOW_S) {
            self.samples.pop_front();
        }
        if self.samples.len() < 6 {
            // Not enough history yet: assume half of the draw is the baseline.
            return power_w * 0.5;
        }
        let mut v: Vec<f64> = self.samples.iter().map(|(_, p)| *p).collect();
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * BASELINE_PERCENTILE).round() as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::ProcSample;

    const APP: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.example.App-1.scope";
    const OTHER: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-org.example.Other-2.scope";

    fn proc(pid: u32, start: u64, cgroup: &str, cpu: u64, gpu_ns: u64) -> ProcSample {
        ProcSample {
            key: ProcKey { pid, start },
            comm: "x".into(),
            cgroup: cgroup.into(),
            cpu_ticks: cpu,
            gpu: GpuCounters { busy_ns: gpu_ns, ..Default::default() },
        }
    }

    fn snap(uptime_s: f64, procs: Vec<ProcSample>) -> Snapshot {
        Snapshot { uptime_s, procs }
    }

    fn key(apps: &HashMap<AppId, Usage>, k: &str) -> Usage {
        apps.iter().find(|(id, _)| id.key == k).map(|(_, u)| *u).unwrap_or_default()
    }

    #[test]
    fn splits_active_energy_by_cpu_and_gpu() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(100.0, vec![proc(1, 10, APP, 0, 0), proc(2, 10, OTHER, 0, 0)]), Some(10.0));
        // 10 s later: App used 3 s CPU, Other used 1 s CPU + 2 s GPU.
        let out =
            a.step(&snap(110.0, vec![proc(1, 10, APP, 300, 0), proc(2, 10, OTHER, 100, 2_000_000_000)]), Some(10.0));
        assert_eq!(out.dt_s, 10.0);
        // 100 J total, baseline = 50 % while history is short.
        let app = key(&out.apps, "app:org.example.App");
        let other = key(&out.apps, "app:org.example.Other");
        let idle = key(&out.apps, "idle");
        assert!((idle.energy_j - 50.0).abs() < 1e-9);
        assert!((app.energy_j - 25.0).abs() < 1e-9, "{app:?}");
        assert!((other.energy_j - 25.0).abs() < 1e-9, "{other:?}");
        assert!((other.gpu_s - 2.0).abs() < 1e-9);
    }

    #[test]
    fn no_energy_when_plugged_in() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(0.0, vec![proc(1, 0, APP, 0, 0)]), None);
        let out = a.step(&snap(10.0, vec![proc(1, 0, APP, 500, 0)]), None);
        let app = key(&out.apps, "app:org.example.App");
        assert_eq!(app.energy_j, 0.0);
        assert_eq!(app.cpu_s, 5.0);
        assert!(!out.apps.keys().any(|k| k.key == "idle"));
    }

    #[test]
    fn new_process_counts_from_start_and_pid_reuse_is_detected() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(100.0, vec![proc(1, 10, APP, 1000, 0)]), None);
        // PID 1 was reused by a new process started at t = 105 s.
        let out = a.step(&snap(110.0, vec![proc(1, 10_500, APP, 200, 0)]), None);
        assert_eq!(key(&out.apps, "app:org.example.App").cpu_s, 2.0);
    }

    #[test]
    fn processes_seen_at_startup_are_not_counted() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(100.0, vec![]), None);
        // Process started long before but appears only now (e.g. daemon restart).
        let out = a.step(&snap(110.0, vec![proc(1, 10, APP, 99_999, 0)]), None);
        assert!(out.apps.is_empty());
    }

    #[test]
    fn idle_machine_puts_everything_in_idle() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(0.0, vec![]), Some(5.0));
        let out = a.step(&snap(10.0, vec![]), Some(5.0));
        assert!((key(&out.apps, "idle").energy_j - 50.0).abs() < 1e-9);
    }

    #[test]
    fn baseline_is_low_percentile() {
        let mut b = Baseline::default();
        for (i, p) in [9.0, 4.0, 12.0, 5.0, 20.0, 4.5, 15.0, 30.0, 8.0, 6.0, 7.0].iter().enumerate() {
            b.push(i as f64, *p);
        }
        assert_eq!(b.push(11.0, 25.0), 4.5);
    }
}
