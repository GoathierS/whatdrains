//! History database, shared between the daemon (writer) and the app (reader).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::apps::{AppId, AppKind};
use crate::battery::PowerState;
use crate::model::Usage;

pub use rusqlite::Error;
pub type Result<T> = rusqlite::Result<T>;

/// How long history is kept.
pub const RETENTION_DAYS: i64 = 30;

/// `$XDG_STATE_HOME/whatdrains/history.sqlite` (usually `~/.local/state/...`).
pub fn default_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("whatdrains").join("history.sqlite")
}

pub struct Store {
    conn: Connection,
    app_ids: HashMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryPoint {
    pub ts: i64,
    pub percent: f64,
    pub power_w: Option<f64>,
    pub state: PowerState,
    /// Seconds the built-in screen was on during this minute.
    pub screen_on_s: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppTotal {
    pub key: String,
    pub kind: AppKind,
    pub label: String,
    pub icon: Option<String>,
    pub usage: Usage,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Opens an existing database without write access (used by the app).
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Self { conn, app_ids: HashMap::new() })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS battery (
                 ts      INTEGER PRIMARY KEY,
                 percent REAL NOT NULL,
                 power_w REAL,
                 state   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS apps (
                 id    INTEGER PRIMARY KEY,
                 key   TEXT NOT NULL UNIQUE,
                 kind  INTEGER NOT NULL,
                 label TEXT NOT NULL,
                 icon  TEXT
             );
             CREATE TABLE IF NOT EXISTS usage (
                 ts       INTEGER NOT NULL,
                 app      INTEGER NOT NULL REFERENCES apps(id),
                 cpu_s    REAL NOT NULL,
                 gpu_s    REAL NOT NULL,
                 energy_j REAL NOT NULL,
                 PRIMARY KEY (ts, app)
             ) WITHOUT ROWID;
             CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value REAL NOT NULL
             );",
        )?;
        // Added in 0.2.
        let has_screen: bool =
            conn.query_row("SELECT COUNT(*) FROM pragma_table_info('battery') WHERE name = 'screen_on_s'", [], |r| {
                r.get::<_, i64>(0).map(|n| n > 0)
            })?;
        if !has_screen {
            conn.execute("ALTER TABLE battery ADD COLUMN screen_on_s REAL NOT NULL DEFAULT 0", [])?;
        }
        Ok(Self { conn, app_ids: HashMap::new() })
    }

    /// Writes one aggregated time bucket. `ts` is a unix timestamp in seconds.
    pub fn record(
        &mut self,
        ts: i64,
        battery: Option<&BatteryPoint>,
        apps: &[(AppId, String, Option<String>, Usage)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        if let Some(b) = battery {
            tx.execute(
                "INSERT OR REPLACE INTO battery (ts, percent, power_w, state, screen_on_s) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![ts, b.percent, b.power_w, b.state as i64, b.screen_on_s],
            )?;
        }
        for (id, label, icon, u) in apps {
            let app = match self.app_ids.get(&id.key) {
                Some(app) => *app,
                None => {
                    tx.execute(
                        "INSERT INTO apps (key, kind, label, icon) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(key) DO UPDATE SET kind = excluded.kind, label = excluded.label, icon = excluded.icon",
                        params![id.key, id.kind as i64, label, icon],
                    )?;
                    let app: i64 = tx.query_row("SELECT id FROM apps WHERE key = ?1", [&id.key], |r| r.get(0))?;
                    self.app_ids.insert(id.key.clone(), app);
                    app
                }
            };
            tx.execute(
                "INSERT INTO usage (ts, app, cpu_s, gpu_s, energy_j) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(ts, app) DO UPDATE SET cpu_s = cpu_s + excluded.cpu_s,
                     gpu_s = gpu_s + excluded.gpu_s, energy_j = energy_j + excluded.energy_j",
                params![ts, app, u.cpu_s, u.gpu_s, u.energy_j],
            )?;
        }
        tx.commit()
    }

    /// Deletes history older than [`RETENTION_DAYS`].
    pub fn prune(&mut self, now: i64) -> Result<()> {
        let cutoff = now - RETENTION_DAYS * 86_400;
        self.conn.execute("DELETE FROM battery WHERE ts < ?1", [cutoff])?;
        self.conn.execute("DELETE FROM usage WHERE ts < ?1", [cutoff])?;
        self.conn.execute("DELETE FROM apps WHERE id NOT IN (SELECT DISTINCT app FROM usage)", [])?;
        self.app_ids.clear();
        Ok(())
    }

    pub fn battery_since(&self, since: i64) -> Result<Vec<BatteryPoint>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ts, percent, power_w, state, screen_on_s FROM battery WHERE ts >= ?1 ORDER BY ts")?;
        let rows = stmt.query_map([since], |r| {
            Ok(BatteryPoint {
                ts: r.get(0)?,
                percent: r.get(1)?,
                power_w: r.get(2)?,
                state: PowerState::from_i64(r.get(3)?),
                screen_on_s: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Per-app totals since `since`, most energy-hungry first, then by activity.
    pub fn apps_since(&self, since: i64) -> Result<Vec<AppTotal>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.key, a.kind, a.label, a.icon, SUM(u.cpu_s), SUM(u.gpu_s), SUM(u.energy_j) AS e
             FROM usage u JOIN apps a ON a.id = u.app
             WHERE u.ts >= ?1
             GROUP BY a.id
             ORDER BY e DESC, SUM(u.cpu_s) + SUM(u.gpu_s) DESC",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok(AppTotal {
                key: r.get(0)?,
                kind: AppKind::from_i64(r.get(1)?),
                label: r.get(2)?,
                icon: r.get(3)?,
                usage: Usage { cpu_s: r.get(4)?, gpu_s: r.get(5)?, energy_j: r.get(6)? },
            })
        })?;
        rows.collect()
    }

    pub fn setting(&self, key: &str) -> Result<Option<f64>> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_setting(&self, key: &str, value: f64) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    /// Timestamp of the most recent record, to tell whether the daemon is running.
    pub fn last_record(&self) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MAX(ts) FROM (SELECT MAX(ts) AS ts FROM battery UNION ALL SELECT MAX(ts) FROM usage)",
                [],
                |r| r.get(0),
            )
            .optional()
            .map(Option::flatten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(key: &str) -> AppId {
        crate::apps::classify(
            &format!("/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-{key}-1.scope"),
            "x",
        )
    }

    #[test]
    fn record_and_query() {
        let mut s = Store::open_in_memory().unwrap();
        let firefox = app("firefox");
        let files = app("org.gnome.Nautilus");
        let u = |e| Usage { cpu_s: 1.0, gpu_s: 0.5, energy_j: e };
        let bp = BatteryPoint {
            ts: 0,
            percent: 80.0,
            power_w: Some(9.0),
            state: PowerState::Discharging,
            screen_on_s: 60.0,
        };

        s.record(
            60,
            Some(&bp),
            &[
                (firefox.clone(), "Firefox".into(), Some("firefox".into()), u(100.0)),
                (files.clone(), "Files".into(), None, u(10.0)),
            ],
        )
        .unwrap();
        s.record(120, None, &[(firefox.clone(), "Firefox".into(), Some("firefox".into()), u(50.0))]).unwrap();
        // Same bucket twice accumulates.
        s.record(120, None, &[(firefox, "Firefox".into(), Some("firefox".into()), u(50.0))]).unwrap();

        let apps = s.apps_since(0).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].label, "Firefox");
        assert_eq!(apps[0].usage.energy_j, 200.0);
        assert_eq!(apps[0].usage.cpu_s, 3.0);
        assert_eq!(apps[1].label, "Files");

        assert_eq!(s.apps_since(100).unwrap().len(), 1);
        assert_eq!(s.battery_since(0).unwrap()[0].screen_on_s, 60.0);
        assert_eq!(s.last_record().unwrap(), Some(120));
    }

    #[test]
    fn settings_and_migration_from_0_1() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE battery (ts INTEGER PRIMARY KEY, percent REAL NOT NULL, power_w REAL, state INTEGER NOT NULL);
             INSERT INTO battery VALUES (0, 50, NULL, 3);",
        )
        .unwrap();
        let s = Store::init(conn).unwrap();
        assert_eq!(s.battery_since(0).unwrap()[0].screen_on_s, 0.0);
        assert_eq!(s.setting("screen_full_w").unwrap(), None);
        s.set_setting("screen_full_w", 5.5).unwrap();
        assert_eq!(s.setting("screen_full_w").unwrap(), Some(5.5));
    }

    #[test]
    fn prune_drops_old_rows() {
        let mut s = Store::open_in_memory().unwrap();
        let bp = BatteryPoint { ts: 0, percent: 50.0, power_w: None, state: PowerState::OnAc, screen_on_s: 0.0 };
        s.record(0, Some(&bp), &[(app("old"), "Old".into(), None, Usage::default())]).unwrap();
        s.prune(RETENTION_DAYS * 86_400 + 1).unwrap();
        assert!(s.apps_since(0).unwrap().is_empty());
        assert!(s.battery_since(0).unwrap().is_empty());
        assert_eq!(s.last_record().unwrap(), None);
    }
}
