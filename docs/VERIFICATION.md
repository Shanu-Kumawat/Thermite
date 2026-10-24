# M0 hardware verification procedure

These experiments validate the architecture's hardware assumptions on the
ANV15-51 before/while relying on them. They need root and change system state
temporarily — each step restores the previous state. Run `thermite doctor`
afterwards to confirm.

## 1. Baseline

```sh
thermite doctor                     # expect: capability none, sensors listed
lsmod | grep acer_wmi               # loaded, default params
cat /sys/module/acer_wmi/parameters/predator_v4   # N
```

## 2. Monitoring + profiles via `predator_v4=1`

Reloads acer-wmi with the v4 force-flag (wireless rfkill may blip; the
module re-probes):

```sh
sudo modprobe -r acer_wmi && sudo modprobe acer_wmi predator_v4=1
sleep 2
thermite doctor
```

**Expected:** capability `monitor-profiles`:
- `/sys/class/hwmon/hwmonX/name` = `acer_wmi` with `fan1_input`/`fan2_input`
  (CPU/GPU RPM) and `temp1-3_input`, but **no `pwm1_enable`** (the param sets
  only the `predator_v4` quirk — verified in mainline `acer-wmi.c`).
- `/sys/firmware/acpi/platform_profile_choices` appears (low-power, quiet,
  balanced, balanced-performance on BIOS V1.60; community reports on
  ANV15-51).

```sh
cat /sys/firmware/acpi/platform_profile_choices
echo quiet | sudo tee /sys/firmware/acpi/platform_profile   # should stick
echo balanced | sudo tee /sys/firmware/acpi/platform_profile
```

**Record:** actual `platform_profile_choices` list, whether profile writes
persist (readback), fan RPM sanity (~2000-3000 idle).

## 3. Full manual control via the stopgap module (kernel 7.2)

The accepted upstream ANV15-51 quirk (landing in 7.3) equals
`predator_v4=1 + pwm=1`. The stopgap DKMS module in `driver/acer-wmi-anv15`
carries exactly that patch on top of the stock 7.2 driver:

```sh
cd driver/acer-wmi-anv15
make                     # builds acer-wmi.ko against current headers
sudo modprobe -r acer_wmi
sudo insmod acer-wmi.ko  # or install via DKMS
sleep 2
thermite doctor          # expect: capability full (pwm1_enable present)
```

**Cautious functional check** (these write to hardware — respect the order,
return to auto at the end):

```sh
H=/sys/class/hwmon/hwmon*   # the acer_wmi one
echo 1 | sudo tee $H/pwm1_enable   # custom mode, CPU fan
echo 128 | sudo tee $H/pwm1        # ~50% duty — listen/verify RPM change
cat $H/fan1_input
echo 255 | sudo tee $H/pwm1        # 100%
cat $H/fan1_input                  # expect near-max RPM (~7000 class)
echo 2 | sudo tee $H/pwm1_enable   # BACK TO AUTO — always end here
```

**Record:**
- Does fan2 (GPU) respond independently (`pwm2`)?
- After ~10 s in custom mode, does the firmware revert `pwm1_enable`
  (the re-assert logic exists precisely for this)?
- Does `echo 0 > pwm1_enable` (turbo) work or EIO?
- Any `dmesg` warnings from acer_wmi during the above.

**Restore:** `sudo rmmod acer_wmi && sudo modprobe acer_wmi` (stock params),
or simply reboot. Uninstall the DKMS module once kernel ≥ 7.3 is running
(`thermite doctor` will show `full` without it).

## 4. Daemon on-target checks

```sh
sudo systemctl start thermite
thermite status           # fans, modes, capability
thermite fan cpu curve balanced
thermite monitor          # watch a minute of ticks; Ctrl-C
sudo systemctl stop thermite
cat /sys/.../pwm1_enable  # back to 2 (auto) after stop
sudo systemctl suspend    # resume, then:
journalctl -u thermite -b --since "10 min ago"   # grace re-apply messages
```

## 5. Findings so far (2026-10-24, before these runs)

| Assumption | Status | Source |
|---|---|---|
| EC writes useless for fan control on ANV15-51 | verified by community (nbfc #219); Thermite never writes EC | docs/INVESTIGATION.md |
| `predator_v4=1` → hwmon + profiles, no pwm | verified in mainline source + community test | HARDWARE.md |
| ANV15-51 quirk = full control incl. independent dual fans | patch author-tested on BIOS V1.60 (lore v4) | INVESTIGATION.md |
| turbo profile EIO | patch author-tested | INVESTIGATION.md |
