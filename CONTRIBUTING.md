# Contributing to Thermite

Thanks for helping! Three kinds of contributions matter most right now:

1. **New device drivers** — the fastest way to grow device support. Start
   with [`docs/DRIVERS.md`](docs/DRIVERS.md); it assumes no kernel experience.
2. **Hardware reports** — even if you can't write code, run the probe in
   `docs/DRIVERS.md` step 0 and open a "device support" issue with the
   output. It builds the evidence base for future drivers.
3. **Bug reports** — especially anything touching failsafe behavior, fan
   control drift, or suspend/resume. Safety bugs go to the front of the
   queue.

## Building and testing

```sh
git clone https://github.com/Shanu-Kumawat/Thermite.git
cd Thermite
cargo test --workspace          # 64 tests, none need hardware
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

Notes:

- Rust **stable**, edition 2024. No nightly features.
- Tests cover the whole policy stack with fake backends; hardware-only
  behavior (firmware quirks, override timing) is verified manually and
  documented in `docs/HARDWARE.md` — add your machine's findings there when
  you add a driver.
- The GUI is GPUI + gpui-kit; keep UI logic thin over the IPC types (see
  `docs/ARCHITECTURE.md`).

## Pull requests

- One logical change per PR; conventional-ish commit subjects
  (`fix:`, `feat:`, `driver:`, `docs:` …).
- `cargo test` + `cargo clippy` + `cargo fmt` clean — CI enforces all three.
- New drivers: follow the checklist at the end of `docs/DRIVERS.md` and
  include your probe output + a `Tested-on:` line.
- Anything that changes fan-control or failsafe behavior needs a test that
  would have failed before the change.

## Safety invariants (never negotiate these in a PR)

- No EC RAM writes, no port I/O — vendor interfaces only.
- Crit temperatures always hand control back to the firmware.
- Duty floors and curve validation stay in the write path.
- Unsupported hardware reports `Unsupported` errors honestly.

## Licensing

By contributing you agree your contributions are licensed MIT (matching the
project), except contributions to `driver/acer-wmi-anv15/`, which are
GPL-2.0-only (upstream kernel code).
