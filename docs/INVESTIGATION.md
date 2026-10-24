# Investigation report (2026-10-24)

The technical investigation that determined Thermite's architecture. Sources
are URLs; claims without a URL were verified first-hand on the target machine
(local probe, BIOS V1.60, kernel 7.2.11-arch1-1).

## 1. The reference project: PXDiv/Div-Acer-Manager-Max

<https://github.com/PXDiv/Div-Acer-Manager-Max>

- **Stack**: Avalonia (C#) GUI + Python daemon (PyInstaller), talking over
  `/var/run/DAMX.sock` (chmod 0666), built on the **Div-Linuwu-Sense** kernel
  module — PXDiv's fork of <https://github.com/0x7375646F/Linuwu-Sense>
  (a ~143 KB single-file out-of-tree module that **replaces/blacklists the
  stock `acer_wmi`**).
- **Hardware access**: never touches EC registers for control — everything
  goes through WMI `WMID GUID4` (`7A4DDFE7-…`): SetGamingFanBehavior (method
  14, magic payloads `0x820009`/`0x410009`/`0x10001`+computed duty),
  sysinfo (method 5) for RPM/temps, misc-setting (method 22) for profiles
  (quiet `0x00` … turbo `0x05`). Exposed as sysfs under
  `/sys/module/linuwu_sense/.../{predator,nitro}_sense/fan_speed`.
- **ANV15-51 is its only "fully supported/verified" model**
  (Compatibility.md); DMI quirk `quirk_acer_nitro`.
- **Features**: profiles, single fixed cpu,gpu fan %, battery limiter, RGB,
  Nitro key. **No fan curves, no CLI, no failsafe/watchdog, no suspend
  handling.**
- **Known issues** (tracker, 135 open):
  - [#136](https://github.com/PXDiv/Div-Acer-Manager-Max/issues/136) manual
    fan speeds revert after ~10 s (sibling model) — firmware override.
  - [#253](https://github.com/PXDiv/Div-Acer-Manager-Max/issues/253) after
    S3 resume the daemon bounces profiles for ~15 s — the Acer EC needs
    12–15 s to reinitialize; daemon has no suspend awareness/debouncing.
  - [#268](https://github.com/PXDiv/Div-Acer-Manager-Max/issues)/#284:
    kernel 7.2 / LLVM kernels fail to build the module (`strncpy` removal).
- GPL-3.0, "passive development", releases through 2026-07.

**Lessons taken**: firmware-override resistance (re-assert + drift
detection), suspend grace windows, curve engine instead of a single duty,
no forked kernel module.

## 2. The EC is a dead end for control on this model

- [nbfc-linux #219](https://github.com/nbfc-linux/nbfc-linux/issues/219) —
  the deepest public ANV15-51 EC investigation: EC RAM reads work
  (`0x58`/`0x59` temps, `0x5C`/`0x6A` RPM), but duty writes to `0xF5` and
  mode writes to `0x54` are **no-ops**; fan control moved to chipset-MMIO
  mailboxes (DSDT `ERM6` @ `0xFE708607`, EC6E regions) reachable only via
  WMI. PXDiv's own verdict: "Nitro V uses wmi methods and nbfc uses ec
  registers and both are not compatible."
- [nbfc-linux #188](https://github.com/nbfc-linux/nbfc-linux/issues/188):
  IT5570E unsupported by it87; firmware overrides EC writes on Acer.
- ChrisCoral/acer-nitro-5-ec-control (deleted; forks survive, e.g.
  [Jebaitedneko's](https://github.com/Jebaitedneko/Acer-Nitro-5-AN515-58-EC-Control-Linux))
  documented the **AN515-era** register set (modes 0x21/0x22, duties
  0x37/0x3A, RPM 0x13-0x16, shutdown-temp 0xAB) — **wrong generation for
  ANV15-51**.
- Kernel's own warning on `ec_sys write_support`: "Dangerous, reboot and
  removal of battery may be needed."
  <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/drivers/acpi/ec_sys.c>

**Decision**: Thermite never writes EC RAM; control goes exclusively through
mainline `acer-wmi` interfaces.

## 3. Mainline kernel already won

- Armin Wolf's acer-wmi fan-control series (RFC Feb 2025 → v4 Oct 2025 →
  applied 2025-10-30): hwmon `temp1-3`, `fan1-2`, `pwm1-2`, `pwmX_enable`
  (0=turbo, 1=custom, 2=auto) for predator_v4-class machines.
  <https://lore.kernel.org/platform-driver-x86/?q=acer-wmi+fan+control>,
  [LWN 1039259](https://lwn.net/Articles/1039259).
- **ANV15-51 quirk accepted 2026-10-05** (Justin Vargas, v4
  2026-09-20), in kernel 7.3+:
  <https://lore.kernel.org/platform-driver-x86/20260920053918.1888-1-justinyvar@gmail.com/>
  — author-tested **on BIOS V1.60**: both fans independently controllable
  (`pwm2`→255 gave ~7309 RPM), `pwm_enable=2` restores auto, **turbo
  profile rejected EIO**.
- On 7.2, `modprobe acer_wmi predator_v4=1` sets only the `predator_v4`
  quirk → hwmon + platform_profile but **no `ACER_CAP_PWM`** (verified in
  mainline source). Community confirmation on ANV15-51:
  [Linuwu-Sense #106](https://github.com/0x7375646F/Linuwu-Sense/issues/106).
- Local probe of the target machine confirmed: stock 7.2 exposes zero
  fan/pwm/profile interfaces (acer_wmi inert, no hwmon, NVIDIA 615.x has no
  hwmon — NVML works, ec_sys available but unloaded).

**Decision**: no new/maintained kernel driver — mainline already provides
the interface; a stopgap DKMS copy of the accepted patch covers 7.2 until
7.3. Writing an EC driver would add risk with zero capability gain.

## 4. Daemon/GUI prior art

- **fw-fan-ctrl** <https://github.com/TamtamHero/fw-fanctrl>: failsafe by
  design — dead daemon → firmware default behavior (periodic refresh lapses);
  separate AC/battery strategies; suspend hooks.
- **asusctl** <https://gitlab.com/asus-linux/asusctl>: daemon+CLI+GUI
  reference; fan curves stored per platform profile.
- **NBFC** <https://github.com/nbfc-linux/nbfc-linux>: client/server, EC
  backends, OOM-hardening, read-only mode — but no ANV15-51 profile possible
  (EC control dead, see §2).
- **GPUI**: published on crates.io (`gpui 0.2.2`, Oct 2025, ~393k downloads)
  plus **gpui-kit/gpui-component 0.7**
  <https://github.com/longbridge/gpui-component> (75+ widgets incl.
  real-time charts, production-proven in Longbridge Pro, Apache-2.0) — makes
  the GUI a crates.io dependency, not a Zed-tree gamble.

## 5. Risks register (what the design defends against)

| Risk | Evidence | Thermite's answer |
|---|---|---|
| Firmware reverts manual fan settings | DAMX #136 (10 s reverts, sibling model) | 2 s re-assert + readback-drift detection + alert |
| Resume storms / EC reinit delay | DAMX #253 (12–15 s) | logind PrepareForSleep → auto before sleep, 20 s no-write grace, then re-apply |
| Dead daemon pins fans | fw-fan-ctrl's model | firmware auto resumes when re-asserts lapse; SIGTERM path restores auto explicitly |
| Miswritten curve starves a hot chip | generic | post-decision duty floors + validation |
| Unsafe client values | generic | server-side validation + floors |
| Wrong-model register writes | brick lore (Reddit r/AcerNitro) | impossible by construction: only mainline driver interfaces are used |
| Turbo profile EIO | upstream patch test | hidden from UX |

## 6. Explicit non-goals (with reasons)

EC RAM writes (§2); keyboard RGB (out of scope, linuwu does it); GPU TGP
unlock (NVPCF stubbed by firmware — Linuwu #106); battery charge limit (WMI
method exists but not mainline; future contribution candidate); turbo
profile (firmware rejects).
