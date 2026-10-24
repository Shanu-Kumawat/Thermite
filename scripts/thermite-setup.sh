#!/usr/bin/env bash
# thermite-setup.sh — one-shot installer for Thermite on the Acer Nitro V 15
# (ANV15-51), kernel 7.2.
#
# What it does:
#   1. Preflight checks (binaries built, dkms + kernel headers, Secure Boot)
#   2. Installs the stopgap acer-wmi driver (DKMS, auto-rebuilds on kernel
#      updates) and reloads it
#   3. Installs thermite-daemon / thermite / thermite-gui + systemd unit
#   4. Creates the `thermite` group and adds the invoking user to it
#   5. Enables + starts thermite.service
#   6. Verifies: capability should read "full fan control"
#
# Undo everything:  sudo bash scripts/thermite-remove.sh
#
# NOTE: remove the driver when kernel >= 7.3 ships the quirk in-tree
# (`thermite doctor` showing "full" without this module is your signal).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DKMS_NAME="acer-wmi-anv15"
DKMS_VER="7.2.stopgap.1"
INVOKING_USER="${SUDO_USER:-$USER}"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
info()  { printf '    %s\n' "$*"; }
warn()  { printf '\033[1;33m    WARNING: %s\033[0m\n' "$*"; }
die()   { printf '\n\033[1;31mERROR: %s\033[0m\n\n' "$*" >&2; exit 1; }

if [[ $EUID -ne 0 ]]; then
    exec sudo bash "$0" "$@"
fi

# ---------------------------------------------------------------- 1. checks
step "1/6  Checking prerequisites"

for bin in thermite-daemon thermite thermite-gui; do
    [[ -x "$REPO_ROOT/target/release/$bin" ]] || die "missing $REPO_ROOT/target/release/$bin
    Run this first:  cargo build --release -p thermite-daemon -p thermite-cli -p thermite-gui"
done
info "release binaries: ok"

command -v dkms >/dev/null || die "dkms is not installed. Run: sudo pacman -S dkms"
info "dkms: ok"

KBUILD="/lib/modules/$(uname -r)/build"
[[ -d $KBUILD ]] || die "kernel headers for $(uname -r) are missing. Run: sudo pacman -S linux-headers"
info "kernel headers: ok"

if command -v mokutil >/dev/null; then
    if mokutil --sb-state 2>/dev/null | grep -qi enabled; then
        warn "Secure Boot is ON — the driver module is unsigned and will likely be refused."
        warn "Disable Secure Boot in the BIOS, or enroll a MOK key, then re-run this script."
        die "cannot continue with Secure Boot enforcing unsigned modules"
    fi
    info "secure boot: off"
else
    info "mokutil not found — skipping Secure Boot check (fine if it's disabled in BIOS)"
fi

# ---------------------------------------------------------------- 2. driver
step "2/6  Installing the stopgap acer-wmi driver (DKMS)"

if dkms status -m "$DKMS_NAME" -v "$DKMS_VER" 2>/dev/null | grep -q ': installed'; then
    info "already installed for this kernel — skipping build"
else
    [[ -d "/usr/src/${DKMS_NAME}-${DKMS_VER}" ]] || true
    rm -rf "/usr/src/${DKMS_NAME}-${DKMS_VER}"
    mkdir -p "/usr/src/${DKMS_NAME}-${DKMS_VER}"
    cp "$REPO_ROOT/driver/acer-wmi-anv15/acer-wmi.c" \
       "$REPO_ROOT/driver/acer-wmi-anv15/Makefile" \
       "$REPO_ROOT/driver/acer-wmi-anv15/dkms.conf" \
       "/usr/src/${DKMS_NAME}-${DKMS_VER}/"
    dkms add -m "$DKMS_NAME" -v "$DKMS_VER" 2>/dev/null || true
    dkms build -m "$DKMS_NAME" -v "$DKMS_VER"
    dkms install -m "$DKMS_NAME" -v "$DKMS_VER"
    info "driver installed (will auto-rebuild on kernel updates)"
fi

step "      Swapping in the patched driver (fans stay untouched)"
if lsmod | grep -q '^acer_wmi '; then
    modprobe -r acer_wmi || warn "could not unload acer_wmi (in use?) — reboot will pick the new module up"
fi
modprobe acer_wmi
sleep 2

ACER_HWMON=""
for d in /sys/class/hwmon/hwmon*; do
    case $(cat "$d/name" 2>/dev/null || true) in
        acer|acer_wmi) ACER_HWMON=$d ;;
    esac
done
[[ -n $ACER_HWMON ]] || die "acer_wmi loaded but no hwmon appeared.
Check: dmesg | grep -i acer_wmi (last 30 lines)"
info "hwmon found: $ACER_HWMON"
info "fan RPMs: $(cat "$ACER_HWMON"/fan*_input 2>/dev/null | tr '\n' ' ')"

if [[ -e $ACER_HWMON/pwm1_enable ]]; then
    info "manual fan control (pwm): available"
else
    warn "pwm control NOT exposed — Thermite will run in monitor+profiles mode only."
    warn "Check 'dmesg | grep -i acer_wmi' and whether the machine actually matched the quirk."
fi

# ---------------------------------------------------------------- 3. install
step "3/6  Installing Thermite"

install -Dm755 "$REPO_ROOT/target/release/thermite-daemon" /usr/bin/thermite-daemon
install -Dm755 "$REPO_ROOT/target/release/thermite"        /usr/bin/thermite
install -Dm755 "$REPO_ROOT/target/release/thermite-gui"    /usr/bin/thermite-gui
install -Dm644 "$REPO_ROOT/packaging/thermite.service" /usr/lib/systemd/system/thermite.service
install -Dm644 "$REPO_ROOT/packaging/sysusers.conf"    /usr/lib/sysusers.d/thermite.conf
install -Dm644 "$REPO_ROOT/packaging/tmpfiles.conf"    /usr/lib/tmpfiles.d/thermite.conf
systemd-sysusers /usr/lib/sysusers.d/thermite.conf
systemd-tmpfiles --create /usr/lib/tmpfiles.d/thermite.conf
info "binaries, service, users and runtime dirs installed"

if [[ -f /etc/thermite/thermite.toml ]]; then
    info "config already exists at /etc/thermite/thermite.toml — leaving it alone"
else
    install -Dm600 "$REPO_ROOT/examples/thermite.toml" /etc/thermite/thermite.toml
    info "installed example config to /etc/thermite/thermite.toml (safe defaults)"
fi

# ---------------------------------------------------------------- 4. group
step "4/6  Granting your user access"

if [[ -n $INVOKING_USER && $INVOKING_USER != root ]]; then
    if id -nG "$INVOKING_USER" | grep -qw thermite; then
        info "$INVOKING_USER is already in the thermite group"
    else
        usermod -aG thermite "$INVOKING_USER"
        info "added $INVOKING_USER to the thermite group (takes effect at next login)"
    fi
else
    warn "could not determine invoking user — add yourself later with: sudo usermod -aG thermite \$USER"
fi

# ---------------------------------------------------------------- 5. service
step "5/6  Starting the daemon"

systemctl daemon-reload
systemctl enable --now thermite
sleep 2
systemctl is-active --quiet thermite || die "thermite.service is not running.
Check: journalctl -u thermite -b --no-pager | tail -30"
info "thermite.service is active and enabled at boot"

# ---------------------------------------------------------------- 6. verify
step "6/6  Verifying"

thermite doctor | sed 's/^/    /'
echo
if thermite doctor 2>/dev/null | grep -q 'full fan control'; then
    printf '\033[1;32m    All good: full fan control is live.\033[0m\n'
else
    warn "capability did not read 'full fan control' — see the doctor output above."
fi

cat <<EOF

------------------------------------------------------------------------
 Done. Try it out:

     thermite status                      # snapshot of temps/fans/modes
     thermite fan cpu curve balanced      # put the CPU fan on a curve
     thermite monitor                     # live view (Ctrl-C to stop)
     thermite-gui                         # desktop app (after next login)

 Suspend test (recommended once):
     systemctl suspend
     journalctl -u thermite -b --since "5 min ago"

 Undo everything:
     sudo bash "$REPO_ROOT/scripts/thermite-remove.sh"

 When kernel >= 7.3 arrives, remove the driver module
 (thermite doctor will still say "full" without it):
     sudo bash "$REPO_ROOT/scripts/thermite-remove.sh --keep-app"

 Emergency, any time: return all fans to Acer's own control:
     for f in /sys/class/hwmon/hwmon*/pwm[12]_enable; do echo 2 | sudo tee \$f; done
------------------------------------------------------------------------
EOF
