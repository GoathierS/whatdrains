<div align="center">

<img src="data/io.github.GoathierS.WhatDrains.svg" width="96" alt="">

# What Drains

**See which apps drain your laptop battery on Linux.**

The battery page your phone has had for years, finally on the Linux desktop.

[![CI](https://github.com/GoathierS/whatdrains/actions/workflows/ci.yml/badge.svg)](https://github.com/GoathierS/whatdrains/actions/workflows/ci.yml)
[![License: GPL v3+](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

</div>

---

Windows, macOS, Android and iOS all answer a simple question: *"what ate my
battery today?"* On Linux you get `powertop`, a terminal UI that shows the last few
seconds, and that's about it. GNOME Usage tried to add per-app power reporting in
2018 and it never shipped.

**What Drains** records your battery in the background and shows:

- your **battery level over the last 24 hours or 7 days**, with plugged-in periods;
- the **share of the battery each app used** while unplugged, in Wh and %;
- how much **CPU and GPU time** each app used;
- what is just the **screen and idle hardware**, so you know what you can and cannot fix.

It understands how modern desktops launch apps: Firefox and all its content
processes count as *Firefox*, `cargo build` in a terminal counts as *cargo* rather
than *Terminal*, and Snap, Flatpak and regular apps show with their real names and
icons.

## How it works

Linux doesn't expose per-process power, so What Drains estimates it the same way
phones do:

1. **Whole-machine draw.** While unplugged, the battery reports the system's power
   draw (`/sys/class/power_supply`).
2. **Idle baseline.** A rolling low percentile of that draw over 30 minutes is what
   the machine uses doing nothing: screen backlight, idle SoC, Wi-Fi... It is shown as
   *Display & idle system*.
3. **Per-app split.** The rest is split between apps in proportion to the CPU time
   (`/proc/<pid>/stat`) and GPU engine time (DRM `fdinfo`: i915, xe, amdgpu,
   nouveau, msm, panfrost...) they used.
4. **Grouping.** Processes are grouped into apps using the systemd cgroup the desktop
   puts every app in (`app-gnome-org.gnome.Nautilus-1234.scope`,
   `snap.firefox.firefox-….scope`, `app-flatpak-….scope`, `vte-spawn-….scope`), and
   named using their `.desktop` file.

Sampling happens every 10 seconds, at idle CPU priority, and is stored per minute in
a small SQLite database in `~/.local/state/whatdrains/`. History is kept for 30 days.
**Nothing leaves your computer**, and no root access is needed.

> [!NOTE]
> The numbers are **estimates**: they're good for finding the app that's draining your
> battery, not for lab measurements. The model ignores the cost of waking the CPU from
> deep sleep, network and disk activity, so very "chatty" apps that use little CPU are
> under-counted. See the [roadmap](#roadmap).

## Install

### From source

You need Rust (1.85 or newer), GTK 4.14+ and libadwaita 1.5+.

```sh
# Debian / Ubuntu 24.04+
sudo apt install libgtk-4-dev libadwaita-1-dev build-essential
# Fedora
sudo dnf install gtk4-devel libadwaita-devel gcc
# Arch
sudo pacman -S gtk4 libadwaita base-devel

git clone https://github.com/GoathierS/whatdrains
cd whatdrains
make install            # installs to ~/.local, no root needed
systemctl --user daemon-reload
systemctl --user enable --now whatdrainsd.service
```

Then open **What Drains** from your app grid. The first numbers appear after a few
minutes on battery.

To remove it: `make uninstall`.

### Packages

Flatpak and distribution packages are on the [roadmap](#roadmap). Help is welcome!

## Command line

The background service also has a small report command:

```console
$ whatdrainsd report 24     # apps that used the most battery in the last 24 hours
```

It prints each app's share of the energy used on battery, in Wh, with its CPU and GPU
time.

## Comparison

|                              | What Drains | PowerTOP | GNOME Usage | powerstat |
| ---------------------------- | :---------: | :------: | :---------: | :-------: |
| Per-app energy               |      ✅      | estimate, live only | ❌ | ❌ |
| History (days)               |      ✅      |    ❌     |      ❌      |     ❌     |
| GPU time per app             |      ✅      |    ❌     |      ❌      |     ❌     |
| Groups processes into apps   |      ✅      |    ❌     |   partly    |     ❌     |
| Graphical app                |      ✅      |    ❌     |      ✅      |     ❌     |
| Needs root                   |      ❌      |    ✅     |      ❌      |  for RAPL  |

PowerTOP remains the right tool for *tuning* power settings; What Drains is about
*understanding* where the battery went.

## Roadmap

- [ ] Flatpak on Flathub, plus `.deb` and AUR packages
- [ ] Per-app chart over time and "since last full charge" view
- [ ] Notification when an app keeps draining the battery in the background
- [ ] Better model: CPU wake-ups, network and disk I/O, per-core frequency
- [ ] Optional RAPL readings through a tiny polkit helper (more precise CPU package power)
- [ ] Estimates while plugged in (learned power model)
- [ ] Translations
- [ ] Battery health page (capacity vs. design, cycle count)

## Contributing

Issues and pull requests are very welcome, especially:

- **reports from other hardware**: run `whatdrainsd report` and tell us if the
  numbers look wrong on your laptop (AMD, ARM, NVIDIA, desktops without a battery);
- apps that show up with a wrong name or icon (please include `cat /proc/<pid>/cgroup`);
- packaging for your distribution.

The code is split in three crates:

| Crate | What it does |
| --- | --- |
| [`crates/core`](crates/core) | Reading the battery and `/proc`, grouping processes into apps, the energy model, storage |
| [`crates/daemon`](crates/daemon) | `whatdrainsd`, the background recorder and `report` command |
| [`crates/gui`](crates/gui) | `whatdrains`, the GTK 4 / libadwaita app |

Before sending a PR, run `make check` (rustfmt, clippy and tests).

## License

[GPL-3.0-or-later](LICENSE)
