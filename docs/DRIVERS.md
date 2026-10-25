# Adding a device driver to Thermite

This guide is for anyone who wants their laptop supported — you do **not**
need kernel-driver experience. A Thermite "driver" is ordinary Rust that
knows where *your* machine exposes fans and temperatures. If Linux already
shows your fans somewhere in sysfs, you're probably 150 lines away from
support.

The one hard rule: **Thermite never writes to the Embedded Controller.**
If your machine only exposes fan control through raw EC registers (common on
some HP/older models), Thermite will support monitoring + profiles, or
nothing — not EC writes. That's deliberate; a wrong EC write can brick
hardware or trip firmware failsafes that outlive the reboot.

## Step 0 — Probe your laptop (15 minutes, no risk)

```sh
git clone https://github.com/Shanu-Kumawat/Thermite.git
cd Thermite
bash scripts/diagnose-driver.sh > my-laptop-probe.txt   # read-only probing
thermite doctor                                         # if installed
```

Then collect, and include in your issue/PR:

```sh
cat /sys/class/dmi/id/product_name /sys/class/dmi/id/product_family \
    /sys/class/dmi/id/bios_version
for h in /sys/class/hwmon/hwmon*; do echo "$h: $(cat $h/name)"; done
ls /sys/class/hwmon/hwmon*/fan*_input 2>/dev/null
ls /sys/class/hwmon/hwmon*/pwm[0-9] 2>/dev/null
cat /sys/firmware/acpi/platform_profile_choices 2>/dev/null
uname -r
```

Paste `my-laptop-probe.txt` output too — it already gathers most of this.

## Step 1 — Read what the probe tells you

| Probe shows | Meaning |
|---|---|
| A hwmon with `fanX_input` **and** `pwmX` + `pwmX_enable` | Best case: full control via that hwmon. This is exactly the Acer case. |
| `fanX_input` only | Monitoring only (fan RPM readable, no control). |
| `platform_profile_choices` exists | Profiles work regardless of fans. |
| Nothing vendor-specific | There may still be a vendor hwmon module to load (see below) — otherwise out of scope for now. |

**Check for a vendor kernel module first.** Many vendors have mainline
drivers that aren't loaded by default:

- ASUS: `asus-nb-wmi`, `asus_wmi_sensors` (often exposes fan/pwm hwmon on
  gaming models — likely the easiest next driver)
- Acer: `acer_wmi` (`predator_v4=1` parameter enables the gaming interface)
- ThinkPads: `thinkpad_acpi` (fan control with its own protocol — would need
  its own driver here)

Try `modprobe <module>` and re-run the probe. If a hwmon appears with
fan/pwm files, you're in the best case.

## Step 2 — Write the driver

Everything lives in `crates/thermite-backend/src/drivers/`. Copy
`acer_wmi.rs` as a starting point — it's the reference implementation. What
a driver must do:

```rust
pub struct MyLaptopBackend {
    root: PathBuf,          // sysfs root, "/" in production, tempdir in tests
    hwmon: Option<PathBuf>,
    capability: CapabilityReport,
    fans: Vec<FanDescriptor>,
}

impl Driver for MyLaptopBackend {
    const NAME: &'static str = "myvendor-wmi";

    /// Return None when this machine isn't yours — the registry then
    /// tries the next driver. Match on the hwmon name, nothing else.
    fn probe_driver(root: &Path) -> Option<Self> { ... }
}

impl Backend for MyLaptopBackend {
    /// Declare your fans: stable name, human label, and which temperature
    /// input drives each (TempSource::Cpu / Gpu / Named("sensor label")).
    fn fans(&self) -> &[FanDescriptor] { &self.fans }

    /// Report honestly what the hardware allows; the whole app degrades
    /// gracefully from this.
    fn capability(&self) -> CapabilityReport { self.capability.clone() }

    /// Read fan RPM/duty/mode + any temps your interface provides.
    fn read_telemetry(&mut self) -> Result<Telemetry, BackendError> { ... }

    /// Translate abstract intent to your hardware. If your hardware can't
    /// do something, return BackendError::Unsupported — never no-op.
    fn set_fan_hw_mode(&mut self, fan: &FanId, mode: HwFanMode) -> ... { ... }
    fn set_fan_duty(&mut self, fan: &FanId, duty_pct: u8) -> ... { ... }
    fn set_platform_profile(&mut self, profile: &str) -> ... { ... }
}
```

Notes on the conventions:

- `pwmX` duty is 0–255 in sysfs; convert percent↔pwm (see
  `pct_to_pwm`/`pwm_to_pct` in the acer driver). Mind inverted
  `pwmX_enable` conventions — some vendors use 0=auto, 1=manual (the
  standard), Acer uses the opposite. The mapping belongs **in your driver**,
  not in core.
- Unknown fan name passed to a setter → `BackendError::Unsupported`.
- Map `EACCES` → `Permission`, `ENOENT` → `Unsupported`, firmware refusals
  (EIO) → `Invalid` — see `map_write_err` in the acer driver. The daemon's
  alerting keys off these.

## Step 3 — Register it

One line in `crates/thermite-backend/src/drivers/mod.rs`:

```rust
pub mod myvendor_wmi;

pub fn probe_drivers(root: &Path) -> Option<DriverMatch> {
    if let Some(d) = myvendor_wmi::MyLaptopBackend::probe_driver(root) {
        return Some(DriverMatch { name: myvendor_wmi::MyLaptopBackend::NAME, backend: Box::new(d) });
    }
    None
}
```

## Step 4 — Test without hardware

Steal the acer driver's test pattern: create a tempdir that fakes your
hwmon's files, point the backend at it, assert on reads and writes. Also use
`FakeBackend::with_fans(&[("cpu", "CPU Fan", TempSource::Cpu), ...])` to
test N-fan behavior generically. All of CI runs without any hardware.

```rust
#[test]
fn full_capability_detected() {
    let (_d, root) = full_fixture();   // tempdir with fan1_input, pwm1, ...
    let report = crate::capability_like_probe(&root);
    assert_eq!(report.level, CapabilityLevel::Full);
}
```

## Step 5 — Verify on your laptop

```sh
export PATH=/usr/bin:$PATH
cargo test -p thermite-backend
sudo systemctl stop thermite
sudo ./target/debug/thermite-daemon --dry-run   # full loop, no writes — watch the log
thermite doctor                                  # driver should now match by name
# then live, with failsafes armed by default:
sudo systemctl start thermite
thermite fan list
thermite fan set <name> manual 60   # and listen / watch RPM respond
thermite fan set <name> auto        # ALWAYS return to auto when done testing
```

## Step 6 — Open the PR

Include: probe output (`my-laptop-probe.txt`), `thermite doctor` output,
what you tested live (manual duty? curves? profiles?), and
`Tested-on: <DMI product name>` in the PR description. If you can't test
everything, say so — an honest "monitoring only, verified" beats a
speculative "should work".

## Safety checklist for reviewers (and you)

- [ ] No EC register writes anywhere (`/sys/kernel/debug/ec`, port I/O — none of it)
- [ ] Unsupported operations return errors, not silent no-ops
- [ ] Capability reporting is honest (don't claim Full without pwm writes verified)
- [ ] Fixture-based tests cover read + write paths
- [ ] `thermite fan set <name> auto` restores firmware control after tests
