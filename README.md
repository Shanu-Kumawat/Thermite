# Thermite

Thermal management for the **Acer Nitro V 15 (ANV15-51)** on Linux: a privileged
daemon with custom fan curves, thermal failsafes, performance profiles, and
live monitoring — driven from a CLI and a GPUI desktop GUI.

```
crates/
├── thermite-core     # pure policy: curves, hysteresis, failsafes, config
├── thermite-proto    # versioned IPC protocol over a Unix socket
├── thermite-backend  # acer-wmi sysfs + hwmon + NVML hardware access
├── thermite-daemon   # control loop, failsafes, IPC server (runs as root)
├── thermite-cli      # `thermite` command-line client
└── thermite-gui      # GPUI desktop client
```

## Why it looks like this

The investigation that shaped this architecture is in
[`docs/INVESTIGATION.md`](docs/INVESTIGATION.md). The short version:

- On the ANV15-51, fan control physically lives behind the firmware's WMI
  gaming interface, not behind EC registers — writing EC RAM is a proven dead
  end on this model, so Thermite **never writes to the EC**.
- Mainline `acer-wmi` already speaks that WMI interface (hwmon fans/temps,
  `pwmX_enable`, `platform_profile`), and the ANV15-51 quirk was accepted
  upstream in October 2026 (kernel 7.3+). Until then,
  `modprobe acer_wmi predator_v4=1` gives monitoring + profiles, and the
  temporary DKMS module in [`driver/`](driver/) enables full control.
- The reference project (Div-Acer-Manager-Max) showed what to avoid: no
  failsafes, no suspend handling (resume-storm bugs), fan settings reverted by
  firmware, a forked 143 KB kernel module. Thermite counters each — see
  [`docs/SAFETY.md`](docs/SAFETY.md).

## Quick start

```sh
# build
cargo build --release

# diagnose what this machine can do right now (no root needed)
thermite doctor

# with root: enable the full control path (kernel 7.2)
sudo modprobe -r acer_wmi && sudo modprobe acer_wmi predator_v4=1   # monitoring + profiles
# or build+install the stopgap module for manual fan control (see driver/)

# install + run the daemon
sudo install -Dm755 target/release/thermite-daemon /usr/bin/thermite-daemon
sudo install -Dm755 target/release/thermite /usr/bin/thermite
sudo install -Dm644 packaging/thermite.service /usr/lib/systemd/system/thermite.service
sudo install -Dm644 packaging/sysusers.conf /usr/lib/sysusers.d/thermite.conf
sudo install -Dm644 packaging/tmpfiles.conf /usr/lib/tmpfiles.d/thermite.conf
sudo systemd-sysusers /usr/lib/sysusers.d/thermite.conf
sudo install -Dm600 examples/thermite.toml /etc/thermite/thermite.toml   # optional; defaults work
sudo usermod -aG thermite $USER   # let your user talk to the daemon
sudo systemctl enable --now thermite

thermite status
thermite fan cpu curve balanced
thermite profile set quiet
thermite monitor
```

The GUI (`target/release/thermite-gui`) connects to the same daemon; add your
user to the `thermite` group first.

## Safety posture

- Fans return to **firmware auto** on daemon stop, crash-lapse (re-assert
  stops → firmware resumes control), suspend, and thermal crit.
- Duty floors make "0% fan on a hot machine" impossible even with a
  miswritten curve.
- Config is strictly validated; a bad config is rejected, never applied.
- `thermite-daemon --dry-run` runs the whole control loop while logging
  writes instead of performing them.

Details: [`docs/SAFETY.md`](docs/SAFETY.md).

## License

MIT for the application crates. The stopgap kernel module in `driver/` is
GPL-2.0 (it contains upstream kernel code).
