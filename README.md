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
cargo build --release -p thermite-daemon -p thermite-cli -p thermite-gui

# diagnose what this machine can do right now (no root needed)
thermite doctor

# one-shot install: stopgap driver (DKMS) + binaries + systemd service
# + thermite group + verification. Asks for sudo itself.
sudo bash scripts/thermite-setup.sh

# undo everything (or --keep-app to remove just the driver at kernel 7.3+)
sudo bash scripts/thermite-remove.sh
```

The manual equivalent of the setup script is in
[`docs/VERIFICATION.md`](docs/VERIFICATION.md). After it completes:

```sh
thermite status
thermite fan cpu curve balanced
thermite monitor
thermite-gui   # after logging out and back in once (group membership)
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
