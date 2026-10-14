//! `whatdrainsd`: samples the battery and per-process activity every few seconds and
//! stores per-app energy estimates, one bucket per minute.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use whatdrains_core::apps::{AppId, DesktopDb};
use whatdrains_core::battery::{self, PowerState};
use whatdrains_core::model::{Attributor, Usage};
use whatdrains_core::store::{self, BatteryPoint, Store};
use whatdrains_core::{display_info, procs};

const SAMPLE_EVERY: Duration = Duration::from_secs(10);
const BUCKET_S: i64 = 60;
const DESKTOP_DB_REFRESH: Duration = Duration::from_secs(3600);

const USAGE: &str = "\
Usage: whatdrainsd [COMMAND] [OPTIONS]

Commands:
  run              Record battery usage (default; meant to run as a systemd user service)
  report [HOURS]   Print the apps that used the most battery in the last HOURS (default 24)

Options:
  --db PATH        History database (default: $XDG_STATE_HOME/whatdrains/history.sqlite)
  -h, --help       Show this help
  -V, --version    Show the version";

fn main() {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut positional = Vec::new();
    let mut db = store::default_path();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return println!("{USAGE}"),
            "-V" | "--version" => return println!("whatdrainsd {}", env!("CARGO_PKG_VERSION")),
            "--db" => match args.next() {
                Some(p) => db = PathBuf::from(p),
                None => fail("--db needs a path"),
            },
            _ if command.is_none() => command = Some(arg),
            _ => positional.push(arg),
        }
    }

    let result = match command.as_deref().unwrap_or("run") {
        "run" => run(&db),
        "report" => {
            let hours = positional.first().map(|h| h.parse::<f64>().unwrap_or_else(|_| fail("HOURS must be a number")));
            report(&db, hours.unwrap_or(24.0))
        }
        other => fail(&format!("unknown command '{other}'\n\n{USAGE}")),
    };
    if let Err(e) = result {
        fail(&format!("database error: {e}"));
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("whatdrainsd: {msg}");
    std::process::exit(2)
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Accumulates samples until the current minute is over.
struct Bucket {
    start: i64,
    apps: HashMap<AppId, Usage>,
    percent: Option<f64>,
    state: PowerState,
    power_sum_w: f64,
    power_n: u32,
}

impl Bucket {
    fn new(now: i64) -> Self {
        Self {
            start: now - now.rem_euclid(BUCKET_S),
            apps: HashMap::new(),
            percent: None,
            state: PowerState::Unknown,
            power_sum_w: 0.0,
            power_n: 0,
        }
    }
}

fn run(db_path: &Path) -> store::Result<()> {
    let mut db = Store::open(db_path)?;
    db.prune(now_unix())?;
    eprintln!("whatdrainsd: recording to {}", db_path.display());

    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&stop));
    }

    let battery_path = battery::find_battery();
    if battery_path.is_none() {
        eprintln!("whatdrainsd: no battery found, recording activity only");
    }

    let mut attributor = Attributor::new(procs::clock_ticks_per_second());
    let mut desktop = DesktopDb::new();
    let mut desktop_loaded = Instant::now();
    let mut bucket = Bucket::new(now_unix());
    let mut last_prune = now_unix();

    attributor.step(&procs::snapshot(), None);
    let mut next = Instant::now() + SAMPLE_EVERY;

    while !stop.load(Ordering::Relaxed) {
        // Sleep in short steps so SIGTERM is handled promptly.
        if Instant::now() < next {
            std::thread::sleep(Duration::from_millis(500).min(next - Instant::now()));
            continue;
        }
        next += SAMPLE_EVERY;
        // After a suspend, don't try to catch up on missed samples.
        if next < Instant::now() {
            next = Instant::now() + SAMPLE_EVERY;
        }

        let reading = battery_path.as_deref().and_then(battery::read_battery);
        let power = reading.filter(|r| r.state == PowerState::Discharging).and_then(|r| r.power_w);
        let interval = attributor.step(&procs::snapshot(), power);

        let now = now_unix();
        if now - bucket.start >= BUCKET_S {
            if desktop_loaded.elapsed() > DESKTOP_DB_REFRESH {
                desktop = DesktopDb::new();
                desktop_loaded = Instant::now();
            }
            flush(&mut db, &bucket, &mut desktop)?;
            bucket = Bucket::new(now);
            if now - last_prune > 86_400 {
                db.prune(now)?;
                last_prune = now;
            }
        }

        // Ignore intervals stretched by a suspend: the battery reading is meaningless.
        if interval.dt_s > SAMPLE_EVERY.as_secs_f64() * 3.0 {
            continue;
        }
        for (id, u) in interval.apps {
            bucket.apps.entry(id).or_default().add(&u);
        }
        if let Some(r) = reading {
            bucket.percent = Some(r.percent);
            bucket.state = r.state;
            if let Some(p) = power {
                bucket.power_sum_w += p;
                bucket.power_n += 1;
            }
        }
    }

    flush(&mut db, &bucket, &mut desktop)?;
    eprintln!("whatdrainsd: stopped");
    Ok(())
}

fn flush(db: &mut Store, bucket: &Bucket, desktop: &mut DesktopDb) -> store::Result<()> {
    let battery = bucket.percent.map(|percent| BatteryPoint {
        ts: bucket.start,
        percent,
        power_w: (bucket.power_n > 0).then(|| bucket.power_sum_w / bucket.power_n as f64),
        state: bucket.state,
    });
    let apps: Vec<_> = bucket
        .apps
        .iter()
        .map(|(id, u)| {
            let (label, icon) = display_info(desktop, id);
            (id.clone(), label, icon, *u)
        })
        .collect();
    if battery.is_none() && apps.is_empty() {
        return Ok(());
    }
    db.record(bucket.start, battery.as_ref(), &apps)
}

fn report(db_path: &Path, hours: f64) -> store::Result<()> {
    if !db_path.exists() {
        fail(&format!("no history yet at {} (is whatdrainsd running?)", db_path.display()));
    }
    let db = Store::open_read_only(db_path)?;
    let since = now_unix() - (hours * 3600.0) as i64;
    let apps = db.apps_since(since)?;
    let battery = db.battery_since(since)?;

    let total_j: f64 = apps.iter().map(|a| a.usage.energy_j).sum();
    let on_battery_min = battery.iter().filter(|b| b.state == PowerState::Discharging).count();
    println!(
        "Last {hours} h: {on_battery_min} min on battery, {:.1} Wh used{}\n",
        total_j / 3600.0,
        if total_j > 0.0 { "" } else { " (apps ranked by CPU/GPU activity)" }
    );
    println!("{:>6}  {:>8}  {:>9}  {:>9}  APP", "SHARE", "ENERGY", "CPU", "GPU");
    // Hide the long tail of apps that barely did anything.
    let visible = apps.iter().filter(|a| {
        if total_j > 0.0 {
            a.usage.energy_j / total_j >= 0.001
        } else {
            a.usage.cpu_s + a.usage.gpu_s >= 1.0
        }
    });
    for a in visible.take(25) {
        let share = if total_j > 0.0 { format!("{:.1}%", a.usage.energy_j / total_j * 100.0) } else { "-".into() };
        println!(
            "{:>6}  {:>6.2}Wh  {:>9}  {:>9}  {}",
            share,
            a.usage.energy_j / 3600.0,
            duration(a.usage.cpu_s),
            duration(a.usage.gpu_s),
            a.label
        );
    }
    Ok(())
}

fn duration(s: f64) -> String {
    match s {
        s if s < 60.0 => format!("{s:.0}s"),
        s if s < 3600.0 => format!("{:.0}m", s / 60.0),
        s => format!("{:.0}h{:02.0}m", (s / 3600.0).floor(), (s % 3600.0) / 60.0),
    }
}
