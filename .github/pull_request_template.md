## What changed

<!-- One or two sentences. Link the issue if there is one. -->

## How it was tested

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-features -- -D warnings`
- [ ] `cargo build --release`
- [ ] Flashed to a real XIAO ESP32-C3 and observed the expected behaviour

<!-- CI covers the first three. The last one is the only thing CI cannot do:
     if this change touches GPIO, timing, peripherals or the boot path, say
     what you saw on the board. If it genuinely does not touch hardware
     behaviour (docs, CI config), say that instead. -->

## Notes for the reviewer

<!-- Anything surprising: a pinned version, a workaround, a known limitation. -->
