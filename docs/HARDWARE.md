# Hardware notes — Acer Nitro V 15 (ANV15-51)

Machine under management: `Nitro ANV15-51`, board `Sportage_RTH`, BIOS
**V1.60**, i5-13420H + RTX 4050 (AD107M), ITE IT5570E EC. Findings below are
sourced from the investigation in `INVESTIGATION.md` and verified on this
exact machine (BIOS V1.60) unless noted.

## Control path

The only working fan-control plane is the firmware's WMI gaming interface on
GUID `7A4DDFE7-5B5D-40B4-8595-4408E0CC7F56` (WMID GUID4):

| Function | WMI method ID | Notes |
|---|---|---|
| SetGamingFanBehavior (mode) | 14 | auto=1, turbo=2, custom=3; fan IDs CPU=0x01, GPU=0x04 |
| SetGamingFanSpeed (duty) | 16 | duty = `(pct * 25600 / 100) & 0xFF00 + fan_index` (per linuwu_sense) |
| GetGamingFanBehavior | 15 | |
| sysinfo (RPM/temps) | 5 | CPU fan cmd `0x0201`, GPU fan `0x0601`, `GENMASK(20,8)` |
| misc setting (profiles) | 22 | quiet=0x00, balanced=0x01, performance=0x04, turbo=0x05*, eco=0x06 |

Userspace cannot call WMI methods directly — the mainline `acer-wmi` driver
does it for us and exposes:

- hwmon `acer_wmi`: `fan1/2_input` (CPU/GPU RPM), `temp1-3_input`
  (CPU/GPU/external), `pwm1/2` (duty 0-255), `pwm1/2_enable`
  (**0 = turbo, 1 = custom, 2 = auto — inverted vs the usual hwmon
  convention**).
- `/sys/firmware/acpi/platform_profile[_choices]`.

### Capability ladder (verified on BIOS V1.60)

| Kernel state | What you get |
|---|---|
| stock 7.2 (`acer_wmi` default) | nothing fan-related; generic sensors only |
| 7.2 + `modprobe acer_wmi predator_v4=1` | hwmon temps + both fan RPMs + platform profiles (**no pwm** — the module param sets only the `predator_v4` quirk, not `pwm`) |
| 7.3+ (ANV15-51 quirk accepted 2026-10-05) or stopgap DKMS | everything incl. manual pwm on both fans |

Profile behavior: low-power/quiet/balanced/balanced-performance work;
**turbo is rejected by firmware with EIO**.

## Why EC writes are dead on this model

- EC RAM is readable via ec_sys (useful sensors!) but the AN515-era control
  registers do nothing here: duty writes to 0xF5 / mode writes to 0x54
  confirmed no-ops (nbfc-linux #219, deepest public investigation).
- Gaming/fan state lives in chipset-MMIO mailboxes (DSDT `ERM6` @
  `0xFE708607`, EC6E mailbox regions with busy/error bits) — unreachable
  from ec_sys or `/proc/acpi/ec`.
- Windows NitroSense itself drives fans via the WMI interface above
  (per firmware BMOF).

## Read-only EC sensors (via ec_sys, supplementary)

EC RAM (MMIO-mapped `ERAM` @ `0xFE708500`): `0x58` CTMP (CPU temp), `0x59`
GTMP (GPU temp), `0x5C` CPUF (CPU fan RPM, 16-bit), `0x6A` GPUF (GPU fan RPM,
16-bit). Thermite reads the same values through the WMI hwmon instead; ec_sys
stays optional.

## Other hardware specifics

- NVIDIA driver 615.x exposes **no hwmon** on this machine — GPU temperature
  comes from NVML (`nvidia-smi` works) or the WMI hwmon `temp2`.
- `IT5570E` is unsupported by the `it87` driver.
- Battery: no `charge_control_end_threshold`; charge limiting is only
  reachable through a WMI method mainline doesn't expose.
- Thermal zones: `acpitz`, `SEN2-5`, `TCPU`, `INT3400`, `x86_pkg_temp` +
  coretemp + 2×nvme + iwlwifi — all readable unprivileged.
- RTX 4060 SKUs: Dynamic Boost (75 W TGP) blocked by stubbed NVPCF `_DSM`
  (firmware limitation, not a driver issue).

## Reference projects

- Div-Acer-Manager-Max (reference app): C# Avalonia GUI + Python daemon +
  Div-Linuwu-Sense kernel module (fork of 0x7375646F/Linuwu-Sense); the
  module blacklists/replaces `acer_wmi` — now obsolete for ANV15-51 given
  mainline support.
- ChrisCoral/acer-nitro-5-ec-control (deleted; lives on in forks) — origin
  of the AN515 EC register knowledge (wrong generation for us).
- nbfc-linux #219 / #188, Linuwu-Sense #106 — the ANV15-51 EC/TGP evidence.
