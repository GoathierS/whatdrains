//! Built-in screen state: on/off and backlight level.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenState {
    pub on: bool,
    /// Backlight level, 0–1, as a fraction of the raw maximum. Backlight power is
    /// roughly proportional to the raw (PWM duty cycle) value.
    pub brightness: f64,
}

/// Locations of the built-in panel's backlight and DRM connector.
#[derive(Debug, Clone)]
pub struct ScreenSource {
    backlight: PathBuf,
    connector: Option<PathBuf>,
    lid: Option<PathBuf>,
}

impl ScreenSource {
    /// Returns `None` on machines without a controllable backlight (desktops).
    pub fn find() -> Option<Self> {
        Self::find_in(Path::new("/sys"), Path::new("/proc/acpi/button/lid"))
    }

    pub fn find_in(sys: &Path, lid_dir: &Path) -> Option<Self> {
        let backlight = pick_backlight(&sys.join("class/backlight"))?;
        let connector = sorted_dir(&sys.join("class/drm")).into_iter().find(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            ["-eDP-", "-LVDS-", "-DSI-"].iter().any(|k| name.contains(k))
                && read_trimmed(&p.join("status")).as_deref() == Some("connected")
        });
        let lid = sorted_dir(lid_dir).into_iter().map(|p| p.join("state")).find(|p| p.exists());
        Some(Self { backlight, connector, lid })
    }

    pub fn read(&self) -> Option<ScreenState> {
        let num = |f: &str| read_trimmed(&self.backlight.join(f))?.parse::<f64>().ok();
        let max = num("max_brightness").filter(|m| *m > 0.0)?;
        let level = num("actual_brightness").or_else(|| num("brightness"))?;
        let brightness = (level / max).clamp(0.0, 1.0);

        let bl_off = num("bl_power").is_some_and(|p| p != 0.0);
        let dpms_off = self.connector.as_ref().and_then(|c| read_trimmed(&c.join("dpms"))).is_some_and(|d| d != "On");
        let disabled =
            self.connector.as_ref().and_then(|c| read_trimmed(&c.join("enabled"))).is_some_and(|e| e != "enabled");
        let lid_closed = self.lid.as_ref().and_then(|l| read_trimmed(l)).is_some_and(|s| s.ends_with("closed"));

        let on = !(bl_off || dpms_off || disabled || lid_closed) && brightness > 0.0;
        Some(ScreenState { on, brightness: if on { brightness } else { 0.0 } })
    }
}

/// When several backlight interfaces exist, use the one desktops use: firmware,
/// then platform, then raw.
fn pick_backlight(dir: &Path) -> Option<PathBuf> {
    let all = sorted_dir(dir);
    for kind in ["firmware", "platform", "raw"] {
        if let Some(p) = all.iter().find(|p| read_trimmed(&p.join("type")).as_deref() == Some(kind)) {
            return Some(p.clone());
        }
    }
    all.into_iter().next()
}

fn sorted_dir(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir).map(|it| it.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn read_trimmed(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn fake_sys(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("whatdrains-screen-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        write(&root, "sys/class/backlight/intel_backlight/max_brightness", "96000\n");
        write(&root, "sys/class/backlight/intel_backlight/actual_brightness", "24000\n");
        write(&root, "sys/class/backlight/intel_backlight/bl_power", "0\n");
        write(&root, "sys/class/backlight/intel_backlight/type", "raw\n");
        write(&root, "sys/class/drm/card1-eDP-1/status", "connected\n");
        write(&root, "sys/class/drm/card1-eDP-1/enabled", "enabled\n");
        write(&root, "sys/class/drm/card1-eDP-1/dpms", "On\n");
        write(&root, "lid/LID0/state", "state:      open\n");
        root
    }

    #[test]
    fn screen_on() {
        let root = fake_sys("on");
        let s = ScreenSource::find_in(&root.join("sys"), &root.join("lid")).unwrap().read().unwrap();
        assert_eq!(s, ScreenState { on: true, brightness: 0.25 });
    }

    #[test]
    fn screen_blanked_or_lid_closed() {
        let root = fake_sys("off");
        let src = ScreenSource::find_in(&root.join("sys"), &root.join("lid")).unwrap();
        write(&root, "sys/class/drm/card1-eDP-1/dpms", "Off\n");
        assert!(!src.read().unwrap().on);
        write(&root, "sys/class/drm/card1-eDP-1/dpms", "On\n");
        write(&root, "lid/LID0/state", "state:      closed\n");
        assert_eq!(src.read().unwrap(), ScreenState { on: false, brightness: 0.0 });
    }

    #[test]
    fn no_backlight() {
        let root = std::env::temp_dir().join(format!("whatdrains-screen-none-{}", std::process::id()));
        let _ = fs::create_dir_all(&root);
        assert!(ScreenSource::find_in(&root, &root).is_none());
    }
}
