#!/usr/bin/env bash
# diagnose-driver.sh — figure out why the ANV15-51 quirk isn't applying.
# Run:  sudo bash scripts/diagnose-driver.sh
# Safe: only reloads the acer_wmi module (3 times, different modes) and
# prints state; never writes to any fan/pwm attribute.

set -u

hdr() { printf '\n================ %s ================\n' "$*"; }
state() {
    echo "-- hwmon names:   $(for d in /sys/class/hwmon/hwmon*; do cat "$d/name" 2>/dev/null | tr '\n' ' '; done)"
    echo "-- acer hwmon:    $(for d in /sys/class/hwmon/hwmon*; do n=$(cat "$d/name" 2>/dev/null); [[ $n == acer* ]] && { echo "$d:"; ls "$d" | grep -E 'fan|pwm|temp' | tr '\n' ' '; echo; }; done | tr -d '\n')"
    echo "-- profile file:  $(ls /sys/firmware/acpi/platform_profile* 2>/dev/null || echo none)"
}

[[ $EUID -eq 0 ]] || exec sudo bash "$0" "$@"

hdr "0. current state"
state

hdr "1. reload patched module, verbose (shows which .ko file loads)"
modprobe -r acer_wmi 2>&1
modprobe -v acer_wmi 2>&1
sleep 2
state

hdr "2. kernel log (acer/wmi lines, last 30)"
dmesg | grep -iE 'acer|wmi' | tail -30

hdr "3. reload with predator_v4=1 (community-verified path for this model)"
modprobe -r acer_wmi 2>&1
modprobe acer_wmi predator_v4=1 2>&1
sleep 2
state
dmesg | grep -iE 'acer|wmi' | tail -8

hdr "4. reload with force_caps=0x1C00 (profiles+hwmon+pwm bits forced)"
modprobe -r acer_wmi 2>&1
modprobe acer_wmi force_caps=0x1C00 2>&1
sleep 2
state
dmesg | grep -iE 'acer|wmi' | tail -8

hdr "5. back to default parameters"
modprobe -r acer_wmi 2>&1
modprobe acer_wmi 2>&1
sleep 1
state

echo
echo "Done. Copy EVERYTHING between the ==== markers and paste it back."
