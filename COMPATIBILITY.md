# Device compatibility

Thermite's fan control is not model-by-model reverse engineering: it rides
on the **mainline kernel `acer-wmi` driver** (and, on old kernels, the
[stopgap DKMS module](driver/acer-wmi-anv15/), which is a patched copy of
that same driver). A laptop is therefore compatible with Thermite to exactly
the degree the kernel supports it. This page lists every model the kernel
driver's quirk table knows (as of the pdx86 for-next tree, October 2026 /
kernel 7.3), what level of support that translates to, and what is
hardware-tested.

## Support levels

These mirror what `thermite doctor` reports:

| Level | Meaning |
|---|---|
| **Full** | Per-fan manual duty, custom curves, thermal failsafes, platform profiles |
| **Monitor + profiles** | Fan RPM + temperatures readable, platform profiles settable — no manual duty (the kernel exposes no pwm for these models; no tool can change that except a kernel patch) |
| **Monitor only** | Temperatures readable |
| **None** | No vendor interface detected; generic sensors only |

## Acer — models in the kernel quirk table

| Model (DMI product name) | Level | Status |
|---|---|---|
| Nitro V 15 (ANV15-51) | **Full** | ✅ **hardware-tested** — daily driver of the project author; BIOS V1.60, kernel 7.2 + stopgap module |
| Nitro (AN515-58) | Full | 🔶 expected — untested |
| Nitro (AN16-41) | Full | 🔶 expected — untested |
| Predator (PH16-72) | Full | 🔶 expected — untested |
| Predator Helios Neo 16 (PHN16-72) | Full | 🔶 expected — untested |
| Predator (PT14-51) | Full | 🔶 expected — untested |
| Predator (PHN16-71) | Monitor + profiles | no pwm in the kernel driver yet |
| Predator (PH16-71) | Monitor + profiles | no pwm in the kernel driver yet |
| Predator (PH18-71) | Monitor + profiles | no pwm in the kernel driver yet |
| Aspire (A315-58G) | Monitor only | temperatures only |
| Predator Helios 300 (PH315-53) | None (for Thermite) | pre-hwmon fan interface — the kernel driver cannot expose it through hwmon |

Notes:

- **"Full — expected"** means every ingredient is in place (kernel quirk
  exposes `fanX`/`pwmX`/`tempX` + platform profiles, and Thermite's acer
  driver reads exactly those). The only missing ingredient is a human
  running it on real hardware. If that's you: `thermite doctor`, then the
  probe in [`DRIVERS.md`](DRIVERS.md), then a
  [device-support issue](https://github.com/Shanu-Kumawat/Thermite/issues/new?template=device-support.md).
- On kernel **7.2** (and other pre-7.3 kernels), ANV15-51 needs the
  stopgap DKMS module; models already quirked in your running kernel work
  without it.
- Models **not listed** here may still expose monitoring + profiles via
  `modprobe acer_wmi predator_v4=1` (any laptop whose firmware speaks the
  v4 gaming WMI interface). Run `thermite doctor` to find out.

## Other vendors

| Vendor | Status |
|---|---|
| ASUS | 🔶 driver wanted — many models expose fan hwmon via `asus-nb-wmi`; likely the easiest next driver (see [DRIVERS.md](DRIVERS.md)) |
| HP | 🔶 driver wanted — many HP laptops expose no fan interface at all; monitoring/profiles-only support is realistic, EC writes are not (see [SAFETY.md](SAFETY.md)) |
| ThinkPads | 🔶 driver wanted — `thinkpad_acpi` exposes fan control with its own protocol |
| Anything else | 🔶 run the probe from [DRIVERS.md](DRIVERS.md) and open a device-support issue |

## Tested-by

| Model | Tested by | What was verified |
|---|---|---|
| Acer Nitro V 15 (ANV15-51), BIOS V1.60 | @Shanu-Kumawat | manual duty byte-exact, curves w/ hysteresis, failsafe handback, profile writes, suspend restore, PPD hands-off coexistence |

When your model passes testing, add a row here in your PR — this table is
the project's evidence base, and it grows one honest report at a time.
