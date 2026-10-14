First release of **What Drains**: see which apps drain your laptop battery on Linux.

### Features
- Battery level over the last 24 hours or 7 days, with plugged-in periods
- Estimated share of the battery used by each app while unplugged (Wh and %), with CPU and GPU time
- Screen energy (calibrated automatically on your laptop) and screen-on time
- Apps grouped the way your desktop launches them: Snap, Flatpak and regular apps with their real names and icons; terminal commands shown by name
- `whatdrainsd report` for a quick summary in the terminal
- Everything stays local, no root needed

### Install (x86_64)
Needs GTK ≥ 4.14 and libadwaita ≥ 1.5 (Ubuntu 24.04+, Fedora 40+, Debian 13+, Arch...).

```sh
tar xf whatdrains-*-x86_64-linux.tar.gz
cd whatdrains-*-x86_64-linux
./install.sh
```

Or build from source: see the [README](https://github.com/GoathierS/whatdrains#install).

### Help wanted
The numbers are estimates. If you try it, please send a [hardware report](https://github.com/GoathierS/whatdrains/issues/new?template=hardware-report.yml), especially from AMD, NVIDIA and ARM laptops.
