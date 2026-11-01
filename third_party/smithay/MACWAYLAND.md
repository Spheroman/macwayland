# Vendored smithay 0.7.0

Upstream: https://github.com/Smithay/smithay, crates.io 0.7.0 (MIT, see
LICENSE.txt). Sources only (no examples or benches). Used through
`[patch.crates-io]` in the workspace `Cargo.toml`.

## Changes

- `src/utils/sealed_file.rs`: on macOS, `SealedFile::with_data` writes into
  an unlinked temporary file (re-opened read-only) instead of a POSIX shm
  object, which macOS can't `write` to. Without this, keymaps never reach
  clients (`Error when creating sealed keymap file: Device not configured`).
- `src/wayland/shm/{mod.rs,pool.rs}`: `shm_buffer_pool` and
  `ShmPoolHandle`, which give a host a buffer's pool (its fd and size, kept
  alive by the handle), so the shell can map pools itself and draw from
  them without copying. An addition; nothing upstream changes behaviour.
- `src/lib.rs`: `#![allow(warnings)]`, so upstream's warnings don't fill
  every build now that it's a path dependency.
- `Cargo.toml`: the `[[example]]` and `[[bench]]` tables are removed (their
  sources aren't vendored).

The keymap fix is worth upstreaming; drop this copy once a release has it
(and either upstream has something like `shm_buffer_pool`, or the shell
gets pools another way).
