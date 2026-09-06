# Contributing

Thanks for helping out. This is embedded firmware, so the one rule that matters
most: **if a change affects what the chip does, flash it to a real board before
you open the PR.** CI can prove the code compiles; it cannot prove the LED
blinks.

## Getting set up

```sh
# The toolchain and the riscv32imc-unknown-none-elf target install themselves
# from rust-toolchain.toml on the first cargo command.
cargo install espflash --locked        # flashing + serial monitor
cargo install probe-rs-tools --locked  # breakpoint debugging (optional)
```

See the [README](README.md) for the Linux udev rules — without them flashing
fails with a permission error.

## Layout

A Cargo workspace. [`kolibri-core`](kolibri-core) is the portable crate — no HAL
may appear in its dependency tree — and each directory under
[`boards/`](boards) is one board's firmware. [`boards/README.md`](boards/README.md)
explains which layer a change belongs in, and how to add a board.

## Before opening a PR

```sh
cargo fmt --all --check
cargo clippy -p kolibri-core --all-features -- -D warnings   # host target
cargo test -p kolibri-core --all-features                   # host target

cd boards/xiao-esp32c3
cargo clippy --all-features -- -D warnings
cargo build --release
cargo run --release          # on hardware
```

Only `kolibri-core` has tests: it is the half that builds for the host, so a
format that has to survive a power cycle can be checked without a board on the
desk. Board crates are `no_std` binaries with nothing to link a test harness
against. One of those tests shells out to `python3 tools/mk-settings.py` and
loads what it produces, which is what keeps the provisioning script and the
firmware reading the same format; `python3` therefore has to be on PATH.

Board commands have to run from inside the board directory: that is how Cargo
finds its `.cargo/config.toml`, and with it the target triple, the linker script
and the flashing runner.

Flash and RAM are finite in a way a host program's are not, so CI reports both
per board and comments the numbers on the PR, against a baseline published by
the last build of `main`. To see them before you push:

```sh
python3 tools/fw-size.py \
    target/riscv32imc-unknown-none-elf/release/kolibri --chip esp32c3
```

CI runs all of the above plus `cargo deny`. Warnings are errors, so please don't
`#[allow]` your way past a lint without a comment explaining why.

## Conventions

- **Keep the core portable.** Nothing in `kolibri-core` may depend on a HAL:
  write against `embedded-hal-async`, `embedded-graphics` and the crate's own
  `temperature::Source` / `display::Panel` traits. CI builds it for the host, so
  a stray `esp-hal` fails the build rather than the review.
- **Board crates hold pins, not policy.** Pin numbers, bus addresses and clock
  rates belong in the board crate; periods and layout belong in `kolibri-core`
  so every board agrees on them.
- **Lints live in the workspace `Cargo.toml`**, under `[workspace.lints]`, not as
  crate-level attributes in `main.rs`. That way rust-analyzer and CI read exactly
  the same config, and every crate is held to it.
- **Don't block in async code.** No `Delay::delay_ms`, no busy-wait loops inside
  a task — they stall the whole executor. Use `embassy_time::Timer` or `Ticker`.
- **Prefer ownership over sharing.** Hand a peripheral to the task that uses it
  rather than parking it in a global `Mutex<RefCell<..>>`. Reach for
  `embassy_sync` only when two tasks genuinely need the same resource.
- **Use `Ticker` for periodic work**, not `Timer::after` in a loop — the latter
  drifts by however long each iteration takes.
- **Pin versions deliberately.** Shared crates go in `[workspace.dependencies]`;
  a board's HAL stays in that board's manifest. The esp-rs crates are a matched
  set (`esp-rtos` 0.4 requires `esp-hal` ~1.2), so bumping one alone will not
  resolve. Let Dependabot propose bumps and review them as a group.

## Commit messages

A short imperative subject line (`Add I2C display driver`), and a body
explaining *why* if it isn't obvious. No strict format is enforced.

Describe what the change does and why, and nothing else. Leave out filler prose
and leave out what the change does *not* do -- testing status, deferred work,
caveats. Those go stale the moment the state changes, and the commit is a record
of the change, not of the moment it was written. If it matters, it belongs in
the PR or an issue.

## Licence

By contributing you agree that your work is licensed under the same
Apache-2.0 terms as the rest of the project.
