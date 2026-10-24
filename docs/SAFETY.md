# Safety design

Thermite writes fan-control values to hardware. This document is the contract
for how it does so without endangering the machine, and what it refuses to do.

## Hard rules

1. **No EC RAM writes. Ever.** On the ANV15-51 the EC's fan-control path was
   moved behind WMI mailboxes; the classic AN515-era EC registers either do
   nothing or control something else entirely. Writing them is the main
   firmware-bricking risk in this ecosystem. Thermite's only control surface
   is the mainline `acer-wmi` hwmon interface (`pwmX`, `pwmX_enable`) and
   `platform_profile`.
2. **The firmware's own controller is the safety net.** `pwmX_enable = 2`
   (auto) returns a fan to EC control instantly. Every abnormal path ends
   there.
3. **Duty floors are applied after every decision**, so no curve, hysteresis
   bug, or IPC request can command 0% duty on a hot chip:
   - at/above `duty_floor_temp_c` (default 60 °C) duty ≥ `duty_floor_pct` (30),
   - above `zero_duty_max_c` (default 45 °C) duty ≥ `min_spin_pct` (15).

## Failsafe state machine (per tick)

```
Normal ──warn──▶ ForceFull ──crit──▶ ReleasedAuto
  ▲                  │                    │
  └──── below warn ──┘   └── below warn ──┘
        Normal ◀───── crit - recovery_c ─────┘   (latched release)
```

- **ForceFull** (warn: CPU 87 °C / GPU 82 °C defaults): command 100% duty on
  all fans under our control.
- **ReleasedAuto** (crit: CPU 92 °C / GPU 87 °C): write `pwm_enable = auto`
  on everything and write no duties. Latched until temperatures recover by
  `recovery_c` (5 °C). Rationale: if our model of the machine is wrong while
  it's that hot, the firmware's own controller is the better authority.

## Failure modes and answers

| Failure | Behavior |
|---|---|
| Daemon killed (SIGKILL) | Re-assert stops; firmware auto resumes (periodic `SetGamingFanBehavior` lapses). Fans never stay pinned by dead software. |
| Daemon stopped (SIGTERM/systemctl stop) | `restore_auto()` writes auto on every fan before exit (also covered by `ExecStopPost` semantics — the daemon handles it in-process). |
| Firmware reverts manual settings (observed ~10 s reverts on sibling models, DAMX #136) | Mode+duty re-asserted at least every `reassert_interval_ms` (2 s default); readback drift triggers an immediate rewrite + `external-override` alert. |
| Another tool (CoolerControl…) fights us | Same drift detection alerts; our writes continue. Single-writer is encouraged but not assumed. |
| Suspend/resume | logind `PrepareForSleep(true)` → fans to auto before sleep. On resume: **20 s grace with zero writes** (the EC needs 12–15 s to reinitialize; eager writes cause the profile-bouncing bugs seen in DAMX #253), then state re-applied from scratch. |
| Config reload (SIGHUP) | New config validated first; on failure the old config stays active and an alert is raised. Failsafe thresholds/hysteresis retune atomically. |
| IPC client requests unsafe values | Rejected at the server (duty 0–100, curve references must exist); floors re-checked at the control loop regardless. |
| Telemetry read fails | No writes that tick; warning alert rate-limited to 1/min. |

## Watchdog & packaging

`thermite.service` is `Type=notify` with `WatchdogSec=30`; the daemon pings
`WATCHDOG=1` every tick. If the loop hangs, systemd restarts it — and a
restart begins by restoring auto before taking control again.
`OOMScoreAdjust=-1000` keeps the OOM killer away from the failsafe path.

## Testing

The control loop is fully deterministic and testable with a scripted fake
backend: every row in the table above has a test (`thermite-daemon` tests,
`control.rs`). The curve engine, hysteresis, floors, and failsafe transitions
have property/unit tests in `thermite-core`.

## What is deliberately not implemented

- Turbo platform profile: rejected by this firmware with EIO (verified
  upstream); hidden rather than offered.
- GPU TGP unlocking: Acer firmware stubs NVIDIA's NVPCF interface (75 W
  unreachable on RTX 4060 SKUs); out of scope.
- Battery charge limit: exposed by community modules via WMI but not
  mainline; a good future contribution candidate.
