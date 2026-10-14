//! Groups processes into "apps" using the systemd cgroup each process lives in.
//!
//! Desktop environments launch every application in its own transient systemd unit
//! (`app-gnome-org.gnome.Nautilus-1234.scope`, `snap.firefox.firefox-<uuid>.scope`,
//! `app-flatpak-org.gimp.GIMP-5678.scope`, ...), so the cgroup path tells us which
//! application a process belongs to, including all of its helper processes.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppKind {
    /// A graphical application with a `.desktop` file.
    App = 0,
    /// A command run from a terminal (`vte-spawn-*.scope`, ...).
    Command = 1,
    /// A user background service (`app.slice/*.service`).
    Service = 2,
    /// Part of the desktop session itself (shell, settings daemons, audio server...).
    Desktop = 3,
    /// A system service (`system.slice`).
    System = 4,
    /// Kernel threads.
    Kernel = 5,
    /// Baseline consumption that no process explains (screen, idle SoC, Wi-Fi...).
    Idle = 6,
}

impl AppKind {
    pub fn from_i64(v: i64) -> Self {
        match v {
            0 => Self::App,
            1 => Self::Command,
            2 => Self::Service,
            3 => Self::Desktop,
            4 => Self::System,
            5 => Self::Kernel,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppId {
    /// Stable key stored in the database, e.g. `app:org.gnome.Nautilus`.
    pub key: String,
    pub kind: AppKind,
    /// Fallback label, used until a `.desktop` file provides a nicer one.
    pub label: String,
    /// Desktop file id to look up (without `.desktop`), for `AppKind::App`.
    pub desktop_id: Option<String>,
}

impl AppId {
    fn new(kind: AppKind, key: String, label: impl Into<String>, desktop_id: Option<String>) -> Self {
        Self { key, kind, label: label.into(), desktop_id }
    }

    pub fn idle() -> Self {
        Self::new(AppKind::Idle, "idle".into(), "Display & idle system", None)
    }

    pub fn kernel() -> Self {
        Self::new(AppKind::Kernel, "kernel".into(), "Linux kernel", None)
    }
}

/// Extracts the systemd unit path from the contents of `/proc/<pid>/cgroup`.
/// Works with the unified (v2) hierarchy and with the hybrid layout (`name=systemd`).
pub fn cgroup_path(proc_cgroup: &str) -> Option<&str> {
    let mut v2 = None;
    for line in proc_cgroup.lines() {
        if let Some(p) = line.strip_prefix("0::") {
            v2 = Some(p);
        } else if let Some((_, p)) = line.split_once(":name=systemd:") {
            return Some(p);
        }
    }
    v2
}

/// Classifies a process from its cgroup path and command name.
pub fn classify(cgroup: &str, comm: &str) -> AppId {
    let parts: Vec<&str> = cgroup.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return AppId::kernel();
    }

    // Look at the deepest systemd unit (scope or service) in the path.
    let Some(idx) = parts.iter().rposition(|p| p.ends_with(".scope") || p.ends_with(".service")) else {
        return AppId::new(AppKind::System, "sys:other".into(), "Other system processes", None);
    };
    let unit = parts[idx];
    let in_slice = |slice: &str| parts[..idx].contains(&slice);
    let (name, is_scope) = match unit.strip_suffix(".scope") {
        Some(n) => (n, true),
        None => (unit.strip_suffix(".service").unwrap_or(unit), false),
    };

    // Commands started from a terminal emulator get their own scope; report the
    // command itself ("cargo", "ffmpeg"...) rather than the terminal.
    if name.starts_with("vte-spawn-") || name.starts_with("tab-") || name.starts_with("kitty-") {
        return command(comm);
    }

    if let Some(rest) = name.strip_prefix("snap.") {
        return snap_app(rest);
    }

    if let Some(rest) = name.strip_prefix("app-") {
        let id = parse_app_unit(rest);
        return AppId::new(AppKind::App, format!("app:{id}"), id.clone(), Some(id));
    }

    // Units grouped under an application slice (`app-org.gnome.Terminal.slice/
    // gnome-terminal-server.service`) belong to that application.
    if let Some(slice) = parts[..idx].iter().rev().find_map(|p| p.strip_prefix("app-")?.strip_suffix(".slice")) {
        let id = unescape(slice);
        return AppId::new(AppKind::App, format!("app:{id}"), id.clone(), Some(id));
    }

    if name == "init" {
        return AppId::new(AppKind::System, "sys:systemd".into(), "systemd", None);
    }
    if name.starts_with("session-") && is_scope {
        return AppId::new(AppKind::Desktop, "desk:session".into(), "Login session", None);
    }
    if name.starts_with("user@") {
        return AppId::new(AppKind::Desktop, "desk:systemd-user".into(), "systemd (user)", None);
    }

    let unit_name = unescape(name.split('@').next().unwrap_or(name));
    if in_slice("system.slice") || !parts.iter().any(|p| p.starts_with("user@")) {
        return AppId::new(AppKind::System, format!("sys:{unit_name}"), unit_name, None);
    }
    if in_slice("session.slice") {
        let (key, label) = desktop_component(&unit_name);
        return AppId::new(AppKind::Desktop, format!("desk:{key}"), label, None);
    }
    AppId::new(AppKind::Service, format!("svc:{unit_name}"), unit_name, None)
}

fn command(comm: &str) -> AppId {
    let comm = if comm.is_empty() { "unknown" } else { comm };
    AppId::new(AppKind::Command, format!("cmd:{comm}"), comm, None)
}

/// `firefox.firefox-<uuid>` / `snapd-desktop-integration.snapd-desktop-integration`
fn snap_app(rest: &str) -> AppId {
    let mut it = rest.splitn(2, '.');
    let snap = it.next().unwrap_or(rest);
    let app = it.next().map(strip_random_suffix).unwrap_or(snap);
    let desktop_id = format!("{snap}_{app}");
    AppId::new(AppKind::App, format!("app:{desktop_id}"), snap, Some(desktop_id))
}

/// Parses `[<launcher>-]<ApplicationID>[-<RANDOM>]` (the part after `app-`), following
/// the systemd "Desktop Environment Integration" naming scheme. Dashes inside the
/// application id are escaped as `\x2d`, so splitting on `-` is safe.
fn parse_app_unit(rest: &str) -> String {
    let rest = rest.split('@').next().unwrap_or(rest);
    let mut parts: Vec<&str> = rest.split('-').collect();
    if parts.len() > 1 && looks_random(parts[parts.len() - 1]) {
        parts.pop();
    }
    let id = if parts.len() > 1 { parts[1..].join("-") } else { parts[0].to_string() };
    unescape(&id)
}

fn strip_random_suffix(s: &str) -> &str {
    match s.rsplit_once('-') {
        Some((head, tail)) if looks_random(tail) => {
            // UUIDs contain dashes themselves: strip every random-looking chunk.
            let mut head = head;
            while let Some((h, t)) = head.rsplit_once('-') {
                if !looks_random(t) {
                    break;
                }
                head = h;
            }
            head
        }
        _ => s,
    }
}

fn looks_random(s: &str) -> bool {
    !s.is_empty()
        && (s.bytes().all(|b| b.is_ascii_digit())
            || (s.len() >= 4 && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b.is_ascii_digit())))
}

/// Undoes systemd unit name escaping (`\x2d` → `-`).
pub fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x' {
            if let Ok(b) = u8::from_str_radix(&s[i + 2..i + 4], 16) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Groups the many small session services into recognisable components.
fn desktop_component(unit: &str) -> (String, String) {
    let known: &[(&str, &str, &str)] = &[
        ("org.gnome.Shell", "gnome-shell", "GNOME Shell"),
        ("org.gnome.SettingsDaemon.", "gnome-settings-daemon", "GNOME Settings Daemon"),
        ("plasma-kwin", "kwin", "KWin"),
        ("plasma-plasmashell", "plasmashell", "Plasma Shell"),
        ("plasma-", "plasma", "Plasma services"),
        ("pipewire", "audio", "Audio (PipeWire)"),
        ("wireplumber", "audio", "Audio (PipeWire)"),
        ("gvfs-", "gvfs", "GVfs"),
        ("xdg-", "portals", "Desktop portals"),
        ("org.freedesktop.IBus", "ibus", "Input methods (IBus)"),
        ("tracker-", "search", "File indexer"),
        ("localsearch", "search", "File indexer"),
        ("dbus", "dbus", "D-Bus"),
    ];
    for (prefix, key, label) in known {
        if unit.starts_with(prefix) {
            return (key.to_string(), label.to_string());
        }
    }
    (unit.to_string(), unit.to_string())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopInfo {
    pub name: String,
    pub icon: Option<String>,
}

/// Resolves desktop file ids to a localized name and icon, with caching.
pub struct DesktopDb {
    dirs: Vec<PathBuf>,
    langs: Vec<String>,
    cache: HashMap<String, Option<DesktopInfo>>,
}

impl Default for DesktopDb {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopDb {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let data_home =
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
        let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());

        let mut dirs = vec![data_home.join("applications")];
        dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("applications")));
        for extra in [
            home.join(".local/share/flatpak/exports/share/applications"),
            "/var/lib/flatpak/exports/share/applications".into(),
            "/var/lib/snapd/desktop/applications".into(),
        ] {
            if !dirs.contains(&extra) {
                dirs.push(extra);
            }
        }

        let lang = std::env::var("LC_MESSAGES").or_else(|_| std::env::var("LANG")).unwrap_or_default();
        Self { dirs, langs: lang_candidates(&lang), cache: HashMap::new() }
    }

    pub fn lookup(&mut self, desktop_id: &str) -> Option<DesktopInfo> {
        if let Some(hit) = self.cache.get(desktop_id) {
            return hit.clone();
        }
        let file = format!("{desktop_id}.desktop");
        let info = self
            .dirs
            .iter()
            .find_map(|d| fs::read_to_string(d.join(&file)).ok())
            .and_then(|content| parse_desktop_entry(&content, &self.langs));
        self.cache.insert(desktop_id.to_string(), info.clone());
        info
    }
}

/// `fr_FR.UTF-8` → `["fr_FR", "fr"]`
fn lang_candidates(lang: &str) -> Vec<String> {
    let base = lang.split(['.', '@']).next().unwrap_or("");
    let mut v = Vec::new();
    if !base.is_empty() && base != "C" && base != "POSIX" {
        v.push(base.to_string());
        if let Some((short, _)) = base.split_once('_') {
            v.push(short.to_string());
        }
    }
    v
}

pub fn parse_desktop_entry(content: &str, langs: &[String]) -> Option<DesktopInfo> {
    let mut in_entry = false;
    let mut name = None;
    let mut localized: Vec<(usize, String)> = Vec::new();
    let mut icon = None;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Name" => name = Some(v.to_string()),
            "Icon" if !v.is_empty() => icon = Some(v.to_string()),
            _ => {
                if let Some(l) = k.strip_prefix("Name[").and_then(|r| r.strip_suffix(']')) {
                    if let Some(rank) = langs.iter().position(|c| c == l) {
                        localized.push((rank, v.to_string()));
                    }
                }
            }
        }
    }
    localized.sort();
    let name = localized.into_iter().next().map(|(_, n)| n).or(name)?;
    Some(DesktopInfo { name, icon })
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "/user.slice/user-1000.slice/user@1000.service";

    #[test]
    fn cgroup_v2_and_hybrid() {
        assert_eq!(cgroup_path("0::/user.slice/x.scope\n"), Some("/user.slice/x.scope"));
        let hybrid = "12:pids:/user.slice\n1:name=systemd:/user.slice/a.service\n0::/\n";
        assert_eq!(cgroup_path(hybrid), Some("/user.slice/a.service"));
    }

    #[test]
    fn gnome_app_scope() {
        let id = classify(&format!("{U}/app.slice/app-gnome-org.gnome.Nautilus-12345.scope"), "nautilus");
        assert_eq!(id.kind, AppKind::App);
        assert_eq!(id.key, "app:org.gnome.Nautilus");
        assert_eq!(id.desktop_id.as_deref(), Some("org.gnome.Nautilus"));
    }

    #[test]
    fn flatpak_scope() {
        let id = classify(&format!("{U}/app.slice/app-flatpak-org.gimp.GIMP-5678.scope"), "gimp");
        assert_eq!(id.key, "app:org.gimp.GIMP");
    }

    #[test]
    fn escaped_dashes() {
        let id = classify(&format!("{U}/app.slice/app-gnome-at\\x2dspi\\x2ddbus\\x2dbus-3736.scope"), "at-spi");
        assert_eq!(id.key, "app:at-spi-dbus-bus");
    }

    #[test]
    fn systemd_app_service() {
        let id = classify(
            &format!("{U}/app.slice/app-org.gnome.Terminal.slice/app-gnome-org.gnome.Terminal@abc.service"),
            "x",
        );
        assert_eq!(id.key, "app:org.gnome.Terminal");
    }

    #[test]
    fn service_inside_app_slice() {
        let id = classify(
            &format!("{U}/app.slice/app-org.gnome.Terminal.slice/gnome-terminal-server.service"),
            "gnome-terminal-",
        );
        assert_eq!(id.key, "app:org.gnome.Terminal");
    }

    #[test]
    fn snap_scope() {
        let id = classify(
            &format!("{U}/app.slice/snap.firefox.firefox-2666e63b-ad9c-42a1-be5c-c7b6a066267b.scope"),
            "firefox",
        );
        assert_eq!(id.kind, AppKind::App);
        assert_eq!(id.key, "app:firefox_firefox");
        assert_eq!(id.label, "firefox");
        let id = classify(&format!("{U}/app.slice/snap.code.code-6cfb8a23-27ff-454f-bb16-95883df6cc0a.scope"), "code");
        assert_eq!(id.key, "app:code_code");
    }

    #[test]
    fn terminal_command() {
        let id = classify(
            &format!("{U}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-14acf4bc-3900-4af2-8b20-21c225e7522d.scope"),
            "cargo",
        );
        assert_eq!(id.kind, AppKind::Command);
        assert_eq!(id.key, "cmd:cargo");
    }

    #[test]
    fn session_and_services() {
        assert_eq!(
            classify(&format!("{U}/session.slice/org.gnome.Shell@wayland.service"), "gnome-shell").key,
            "desk:gnome-shell"
        );
        assert_eq!(classify(&format!("{U}/session.slice/pipewire-pulse.service"), "pipewire-pulse").key, "desk:audio");
        assert_eq!(classify(&format!("{U}/app.slice/librepods.service"), "librepods").key, "svc:librepods");
        assert_eq!(classify("/system.slice/NetworkManager.service", "NetworkManager").key, "sys:NetworkManager");
        assert_eq!(classify("/system.slice/system-getty.slice/getty@tty1.service", "agetty").key, "sys:getty");
        assert_eq!(classify("/init.scope", "systemd").key, "sys:systemd");
        assert_eq!(classify("/", "kworker/0:1").kind, AppKind::Kernel);
        assert_eq!(classify("/user.slice/user-1000.slice/session-3.scope", "gdm").key, "desk:session");
    }

    #[test]
    fn desktop_entry_localized() {
        let content = "[Desktop Entry]\nName=Files\nName[fr]=Fichiers\nIcon=org.gnome.Nautilus\n[Desktop Action new]\nName=New Window\n";
        let info = parse_desktop_entry(content, &lang_candidates("fr_FR.UTF-8")).unwrap();
        assert_eq!(info.name, "Fichiers");
        assert_eq!(info.icon.as_deref(), Some("org.gnome.Nautilus"));
        let info = parse_desktop_entry(content, &lang_candidates("C")).unwrap();
        assert_eq!(info.name, "Files");
    }
}
