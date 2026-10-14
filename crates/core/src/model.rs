//! Turns successive process snapshots and battery readings into per-app energy.
//!
//! Linux laptops do not expose per-process power, so we estimate it:
//!
//! 1. The battery tells us the whole machine's draw `P` while unplugged.
//! 2. A rolling low percentile of `P` is the *baseline* `P0`: what the machine draws
//!    when nothing much is running (screen backlight, idle SoC, Wi-Fi...).
//! 3. Part of `P0` is the screen: backlight level × the panel's full-brightness power.
//!    That power starts from a typical laptop value and is calibrated automatically by
//!    comparing the baselines with the screen on and off. The rest of `P0` goes to the
//!    "Idle system" bucket.
//! 4. The rest, `P - P0`, is split between apps in proportion to the CPU and GPU time
//!    they used during the interval.
//!
//! This is the same kind of model macOS and Android use for their "energy impact"
//! views. It is an estimate, not a measurement.

use std::collections::{HashMap, VecDeque};

use crate::apps::{classify, AppId};
use crate::procs::{GpuCounters, ProcKey, Snapshot};
use crate::screen::ScreenState;

/// How much one second of GPU engine time weighs compared to one CPU second.
pub const GPU_WEIGHT: f64 = 1.0;
/// Window used to estimate the idle baseline.
const BASELINE_WINDOW_S: f64 = 30.0 * 60.0;
const BASELINE_PERCENTILE: f64 = 0.10;
/// Typical backlight power of a 13–15" laptop panel at full brightness, used until
/// the machine's own value has been calibrated.
pub const DEFAULT_SCREEN_FULL_W: f64 = 4.0;
const SCREEN_FULL_W_RANGE: (f64, f64) = (0.3, 20.0);
/// Weight of each new calibration sample (exponential moving average).
const CALIBRATION_RATE: f64 = 0.05;

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
    /// How long the built-in screen was on during the interval.
    pub screen_on_s: f64,
    pub apps: HashMap<AppId, Usage>,
}

pub struct Attributor {
    ticks_per_s: f64,
    prev: HashMap<ProcKey, (u64, GpuCounters)>,
    prev_uptime: Option<f64>,
    /// Idle baselines with the screen on (or unknown) and off.
    baseline_on: Baseline,
    baseline_off: Baseline,
    screen_full_w: f64,
}

impl Attributor {
    pub fn new(ticks_per_s: f64) -> Self {
        Self {
            ticks_per_s,
            prev: HashMap::new(),
            prev_uptime: None,
            baseline_on: Baseline::default(),
            baseline_off: Baseline::default(),
            screen_full_w: DEFAULT_SCREEN_FULL_W,
        }
    }

    /// Estimated backlight power at full brightness, in watts.
    pub fn screen_full_w(&self) -> f64 {
        self.screen_full_w
    }

    /// Restores a previously calibrated value.
    pub fn set_screen_full_w(&mut self, w: f64) {
        if w.is_finite() {
            self.screen_full_w = w.clamp(SCREEN_FULL_W_RANGE.0, SCREEN_FULL_W_RANGE.1);
        }
    }

    /// Feeds a new snapshot. `power_w` is the measured system draw while running on
    /// battery, or `None` when it is unknown (plugged in, no battery). `screen` is the
    /// built-in screen state, `None` on machines without one.
    pub fn step(&mut self, snap: &Snapshot, power_w: Option<f64>, screen: Option<ScreenState>) -> Interval {
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
        let screen_on = screen.is_some_and(|s| s.on);
        if screen_on {
            out.screen_on_s = dt;
        }

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
            let t = snap.uptime_s;
            let screen_off = screen.is_some_and(|s| !s.on);
            let p0 = if screen_off {
                self.baseline_off.push(t, p, 0.0)
            } else {
                self.baseline_on.push(t, p, screen.map_or(0.0, |s| s.brightness))
            }
            .min(p);
            self.calibrate(t);
            let screen_w = match screen {
                Some(s) if s.on => (self.screen_full_w * s.brightness).min(p0 * 0.9),
                _ => 0.0,
            };
            distribute(&mut out.apps, p * dt, p0 * dt, screen_w * dt);
        }

        self.remember(snap);
        out
    }

    /// When both baselines are known, the difference between them is the screen's
    /// power at the recent average brightness.
    fn calibrate(&mut self, now: f64) {
        // The screen-off baseline only gets samples while the screen is blanked, which
        // is rare; accept one from the last few hours.
        let on = self.baseline_on.settled(now, 60.0);
        let off = self.baseline_off.settled(now, 6.0 * 3600.0);
        let (Some(on), Some(off)) = (on, off) else {
            return;
        };
        let brightness = self.baseline_on.mean_brightness();
        if brightness < 0.05 {
            return;
        }
        let candidate = (on - off) / brightness;
        if (SCREEN_FULL_W_RANGE.0..=SCREEN_FULL_W_RANGE.1).contains(&candidate) {
            self.screen_full_w += (candidate - self.screen_full_w) * CALIBRATION_RATE;
        }
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

/// Splits `total_j` between the baseline (`baseline_j`, of which `screen_j` is the
/// screen) and the apps, by weight.
fn distribute(apps: &mut HashMap<AppId, Usage>, total_j: f64, baseline_j: f64, screen_j: f64) {
    let active_j = (total_j - baseline_j).max(0.0);
    let total_weight: f64 = apps.values().map(Usage::weight).sum();
    let screen_j = screen_j.min(baseline_j);
    let mut idle = baseline_j - screen_j;
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
    if screen_j > 0.0 {
        apps.entry(AppId::screen()).or_default().energy_j += screen_j;
    }
}

/// Rolling low-percentile estimate of the machine's idle power draw.
#[derive(Default)]
struct Baseline {
    /// (time, power in W, screen brightness 0–1)
    samples: VecDeque<(f64, f64, f64)>,
}

impl Baseline {
    fn push(&mut self, t: f64, power_w: f64, brightness: f64) -> f64 {
        self.samples.push_back((t, power_w, brightness));
        while self.samples.front().is_some_and(|(t0, _, _)| t - t0 > BASELINE_WINDOW_S) {
            self.samples.pop_front();
        }
        // Not enough history yet: assume half of the draw is the baseline.
        self.percentile().unwrap_or(power_w * 0.5)
    }

    fn percentile(&self) -> Option<f64> {
        if self.samples.len() < 6 {
            return None;
        }
        let mut v: Vec<f64> = self.samples.iter().map(|(_, p, _)| *p).collect();
        v.sort_by(f64::total_cmp);
        Some(v[((v.len() - 1) as f64 * BASELINE_PERCENTILE).round() as usize])
    }

    /// The baseline, if it has enough samples and the latest is recent enough.
    fn settled(&self, now: f64, max_age_s: f64) -> Option<f64> {
        let (last, _, _) = self.samples.back()?;
        if now - last > max_age_s {
            return None;
        }
        self.percentile()
    }

    fn mean_brightness(&self) -> f64 {
        let n = self.samples.len().max(1) as f64;
        self.samples.iter().map(|(_, _, b)| b).sum::<f64>() / n
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
        a.step(&snap(100.0, vec![proc(1, 10, APP, 0, 0), proc(2, 10, OTHER, 0, 0)]), Some(10.0), None);
        // 10 s later: App used 3 s CPU, Other used 1 s CPU + 2 s GPU.
        let out = a.step(
            &snap(110.0, vec![proc(1, 10, APP, 300, 0), proc(2, 10, OTHER, 100, 2_000_000_000)]),
            Some(10.0),
            None,
        );
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
        a.step(&snap(0.0, vec![proc(1, 0, APP, 0, 0)]), None, None);
        let out = a.step(&snap(10.0, vec![proc(1, 0, APP, 500, 0)]), None, None);
        let app = key(&out.apps, "app:org.example.App");
        assert_eq!(app.energy_j, 0.0);
        assert_eq!(app.cpu_s, 5.0);
        assert!(!out.apps.keys().any(|k| k.key == "idle"));
    }

    #[test]
    fn new_process_counts_from_start_and_pid_reuse_is_detected() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(100.0, vec![proc(1, 10, APP, 1000, 0)]), None, None);
        // PID 1 was reused by a new process started at t = 105 s.
        let out = a.step(&snap(110.0, vec![proc(1, 10_500, APP, 200, 0)]), None, None);
        assert_eq!(key(&out.apps, "app:org.example.App").cpu_s, 2.0);
    }

    #[test]
    fn processes_seen_at_startup_are_not_counted() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(100.0, vec![]), None, None);
        // Process started long before but appears only now (e.g. daemon restart).
        let out = a.step(&snap(110.0, vec![proc(1, 10, APP, 99_999, 0)]), None, None);
        assert!(out.apps.is_empty());
    }

    #[test]
    fn idle_machine_puts_everything_in_idle() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(0.0, vec![]), Some(5.0), None);
        let out = a.step(&snap(10.0, vec![]), Some(5.0), None);
        assert!((key(&out.apps, "idle").energy_j - 50.0).abs() < 1e-9);
    }

    #[test]
    fn baseline_is_low_percentile() {
        let mut b = Baseline::default();
        for (i, p) in [9.0, 4.0, 12.0, 5.0, 20.0, 4.5, 15.0, 30.0, 8.0, 6.0, 7.0].iter().enumerate() {
            b.push(i as f64, *p, 0.5);
        }
        assert_eq!(b.push(11.0, 25.0, 0.5), 4.5);
        assert_eq!(b.settled(11.0, 60.0), Some(4.5));
        assert_eq!(b.settled(1000.0, 60.0), None);
    }

    fn on(brightness: f64) -> Option<ScreenState> {
        Some(ScreenState { on: true, brightness })
    }

    #[test]
    fn screen_takes_its_share_of_the_baseline() {
        let mut a = Attributor::new(100.0);
        let mut out = Interval::default();
        // A steady 8 W with nothing running, screen at 50 %.
        for i in 0..=10 {
            out = a.step(&snap(i as f64 * 10.0, vec![]), Some(8.0), on(0.5));
        }
        let screen = key(&out.apps, "screen").energy_j;
        let idle = key(&out.apps, "idle").energy_j;
        // Default 4 W panel at 50 % = 2 W of the 8 W, over 10 s.
        assert!((screen - 20.0).abs() < 1e-9, "{screen}");
        assert!((idle - 60.0).abs() < 1e-9, "{idle}");
        assert_eq!(out.screen_on_s, 10.0);
    }

    #[test]
    fn screen_off_has_no_screen_energy() {
        let mut a = Attributor::new(100.0);
        a.step(&snap(0.0, vec![]), Some(5.0), None);
        let out = a.step(&snap(10.0, vec![]), Some(5.0), Some(ScreenState { on: false, brightness: 0.0 }));
        assert_eq!(key(&out.apps, "screen").energy_j, 0.0);
        assert!((key(&out.apps, "idle").energy_j - 50.0).abs() < 1e-9);
        assert_eq!(out.screen_on_s, 0.0);
    }

    #[test]
    fn screen_power_never_exceeds_the_baseline() {
        let mut a = Attributor::new(100.0);
        a.set_screen_full_w(20.0);
        a.step(&snap(0.0, vec![]), Some(3.0), on(1.0));
        let out = a.step(&snap(10.0, vec![]), Some(3.0), on(1.0));
        // Baseline is 1.5 W while history is short; the screen gets at most 90 % of it.
        assert!((key(&out.apps, "screen").energy_j - 13.5).abs() < 1e-9);
    }

    #[test]
    fn calibrates_screen_power_from_screen_off_periods() {
        let mut a = Attributor::new(100.0);
        let mut t = 0.0;
        // 10 min idle with the screen off at 3 W...
        for _ in 0..60 {
            a.step(&snap(t, vec![]), Some(3.0), Some(ScreenState { on: false, brightness: 0.0 }));
            t += 10.0;
        }
        // ...then a long time with the screen at 40 % and 6 W: the panel uses
        // (6 - 3) / 0.4 = 7.5 W at full brightness.
        for _ in 0..600 {
            a.step(&snap(t, vec![]), Some(6.0), on(0.4));
            t += 10.0;
        }
        assert!((a.screen_full_w() - 7.5).abs() < 0.1, "{}", a.screen_full_w());
    }
}
