# acer-wmi stopgap for the ANV15-51 (kernel 7.2)

**This module is temporary. Delete it when your kernel is ≥ 7.3.**

## License

`acer-wmi.c` here is a patched copy of **GPL-2.0** kernel source and stays
GPL-2.0-only; the project-wide MIT license does **not** apply to files in
this directory.

## What this is

The mainline `acer-wmi` driver with the **Acer Nitro ANV15-51 quirk** that
was accepted upstream on 2026-10-05 (Justin Vargas, pdx86 tree) and will
ship in kernel 7.3. On kernel 7.2 that quirk is missing, so the machine has
no fan-control interface. This module carries the upstream code so Thermite
(and any hwmon client, e.g. CoolerControl) can use it today.

Provenance: `drivers/platform/x86/acer-wmi.c` from the pdx86 `for-next` tree
as of 2026-10-24 (includes the ANV15-51 quirk plus the follow-up fixes that
tree carries: quirk-field refactor, turbo-key handling on models without a
turbo button, AMW0 error handling, Aspire A315-58G / Nitro AN16-41 quirks).
License GPL-2.0-or-later, copyright the kernel authors — unchanged except
for this build harness.

With this module loaded, the machine gains:

- hwmon `acer_wmi`: `temp1-3_input` (CPU/GPU/external), `fan1-2_input`
  (CPU/GPU RPM), `pwm1-2` + `pwm1-2_enable` (0=turbo, 1=custom, 2=auto)
- `/sys/firmware/acpi/platform_profile[_choices]`
  (low-power/quiet/balanced/balanced-performance; turbo is rejected by
  firmware — known)

Verify with `thermite doctor` → capability should read **full**.

## Build & install (manual)

```sh
make                                    # needs linux-headers for your kernel
sudo modprobe -r acer_wmi
sudo insmod acer-wmi.ko
thermite doctor
```

## Install (DKMS, rebuilds per kernel)

```sh
sudo cp -r driver/acer-wmi-anv15 /usr/src/acer-wmi-anv15-7.2.stopgap.1
sudo dkms add -m acer-wmi-anv15 -v 7.2.stopgap.1
sudo dkms build -m acer-wmi-anv15 -v 7.2.stopgap.1
sudo dkms install -m acer-wmi-anv15 -v 7.2.stopgap.1
# modules in /updates override the in-tree acer_wmi on next boot
```

## Remove

```sh
sudo dkms remove acer-wmi-anv15/7.2.stopgap.1 --all
sudo rm -rf /usr/src/acer-wmi-anv15-7.2.stopgap.1
sudo modprobe -r acer_wmi && sudo modprobe acer_wmi   # back to in-tree
```

`thermite doctor` showing `full` on kernel ≥ 7.3 without this module means
it is obsolete — remove it. Keeping it installed past 7.3 would shadow the
in-tree driver with stale code.

## Notes & caveats

- The module **replaces** the in-tree `acer_wmi` while loaded (same module
  name). `modprobe -r acer_wmi` first; some setups unload it automatically
  via the usual module dependency rules.
- Secure Boot: the module must be MOK-enrolled or Secure Boot disabled.
- It exposes the same sysfs as the future in-tree version; Thermite needs no
  configuration either way (capability probing handles both).
- Functional check after loading (return to auto when done!):

```sh
H=$(grep -l acer_wmi /sys/class/hwmon/hwmon*/name | xargs dirname)
echo 1 | sudo tee $H/pwm1_enable; echo 128 | sudo tee $H/pwm1   # CPU ~50%
cat $H/fan1_input
echo 2 | sudo tee $H/pwm1_enable                               # back to auto
```
