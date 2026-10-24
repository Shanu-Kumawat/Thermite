# Architecture

## Process model

```
┌─────────────┐   ┌────────────┐   ┌──────────────┐
│ thermite    │   │ thermite   │   │ thermite-gui │
│ (CLI, user) │   │ doctor     │   │ (GPUI, user) │
└──────┬──────┘   └─────┬──────┘   └──────┬───────┘
       │  UDS /run/thermite/thermite.sock │        (probe sysfs directly,
       └───────────────┬──────────────────┘         no daemon needed)
                 ┌─────┴──────────────────────────┐
                 │ thermite-daemon (root, systemd)│
                 │  ┌──────────┐  ┌────────────┐  │
                 │  │ control  │─▶│ thermite-  │  │
                 │  │ loop     │  │ backend    │──┼──▶ /sys (acer-wmi
                 │  │ (1s tick)│  │            │  │    hwmon, profiles),
                 │  └──────────┘  └────────────┘  │    NVML
                 │  ┌──────────┐  ┌────────────┐  │
                 │  │ IPC      │  │ suspend    │  │──▶ logind (D-Bus,
                 │  │ server   │  │ watcher    │  │    PrepareForSleep)
                 │  └──────────┘  └────────────┘  │
                 │  signals: SIGTERM stop, SIGHUP │
                 │  reload; sd_notify watchdog    │
                 └────────────────────────────────┘
```

- The **daemon owns all hardware access**; clients never touch sysfs for
  control. `thermite doctor` is the exception (read-only probing).
- The control loop is a single deterministic function of
  `(telemetry, config, persisted state) → writes`, driven once per
  `poll_interval_ms`. Everything hard to test lives behind traits
  (`thermite_core::Backend`) so tests run the exact production loop against
  a scripted fake.
- Threads: main (control loop), IPC accept, one reader + one writer thread
  per connection (single-writer ordering), logind watcher, signal handler.
  No async runtime — the problem is a 1 Hz loop, and std threads keep the
  daemon's failure surface small.

## IPC protocol (thermite-proto)

Length-prefixed (`u32` LE) JSON frames over the socket:

- `Hello` handshake checks `PROTOCOL_VERSION` on both ends.
- Requests carry a caller id; responses match it; subscribed connections
  additionally receive `Event::Tick` (1 Hz telemetry + control snapshot),
  `Event::Alert`, and `Event::ConfigChanged`.
- Auth: peer must be root, the daemon's own uid, or a member of the
  `thermite` group (SO_PEERCRED). Socket is `root:thermite 0660`.

## State model

- `/etc/thermite/thermite.toml` — policy: curves, thresholds, intervals,
  *default* fan modes, desired platform profile. Strict TOML
  (`deny_unknown_fields`), validated on load; GUI curve edits are persisted
  here by the daemon (atomic tmp+rename).
- `/var/lib/thermite/state.json` — mutable selections: current per-fan modes
  and profile. Overlays config defaults on startup so a restart restores
  user intent.
- In-memory `SharedState` — everything both the IPC server and control loop
  need, behind one mutex, updated once per tick.

## Backend layer (thermite-backend)

`ThermiteBackend` composes:

1. `AcerWmiBackend` — fans/pwm/profiles via the mainline driver's hwmon
   (`fan1/2`, `pwm1/2[_enable]`, `temp1-3`) and `platform_profile`.
   Capability-probed: `Full | MonitorProfiles | None`, degrading gracefully.
2. generic hwmon scan — coretemp/nvme/acpitz/iwlwifi/... temperatures with
   classification (the `cpu_temp`/`gpu_temp` used for control are the max
   over CPU/GPU-class sensors).
3. NVML (feature `nvml`) — GPU temp/power.

All sysfs paths are built from a `root` prefix, so the entire backend is
tested against tempdir fixtures that mirror real sysfs layouts.

## Capability-first design

Because this machine's control surface depends on kernel version and loaded
modules, nothing assumes `Full` capability: the daemon reports
`CapabilityReport` (level, available profiles, remediation notes) in `Hello`
and `GetStatus`; CLI and GUI render what's actually possible and `doctor`
explains how to get more (kernel 7.3+ / stopgap module / `predator_v4=1`).

## Testing strategy

- `thermite-core`: pure unit tests for curves, hysteresis (upshift-gated
  downshifts, temporal holds), failsafe transitions, floors, config
  validation.
- `thermite-backend`: tempdir sysfs fixtures for probing, telemetry parsing,
  write encoding (pct↔pwm), permission/capability error mapping.
- `thermite-daemon`: the production control loop against a `ScriptedBackend`
  (fake + settable temps), asserting on the recorded write log —
  re-assert timing, failsafe escalation/latch/recovery, suspend auto +
  20 s grace, override detection with cooldown, graceful-shutdown restore;
  plus a real-socket end-to-end test of the IPC server (auth path exercised
  in code review; root-only paths validated on-target by `doctor`).
- `thermite-proto`: framing, oversize/truncation rejection, handshake
  version checks over real sockets.
