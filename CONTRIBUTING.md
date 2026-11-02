# Contributing

Bug reports, fixes and ideas are welcome. Hackerman runs on Windows so far, and help with macOS and
Linux would be especially useful.

## Before you start

For anything large, open an issue first, so nobody spends time on something that won't fit.

**Hackerman is written from scratch.** Please don't copy code from GPL-licensed projects such as Lan
Mouse, Input Leap or Deskflow, since their license is not compatible with ours. Reading them for
ideas is fine. Mouse Without Borders is MIT licensed, and code from it can be used with attribution.

Unless you say otherwise, anything you submit is dual licensed under MIT and Apache-2.0, as the rest
of the project is.

## Building

You need stable Rust, Node.js and pnpm. On Windows, use the MSVC toolchain
(`stable-x86_64-pc-windows-msvc`) with the Visual Studio C++ Build Tools.

```sh
cd app
pnpm install
pnpm tauri dev      # run the app during development
pnpm tauri build    # hackerman.exe and an installer, in target/release
```

## Checks

Pull requests have to pass these, which CI also runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check     # rejects dependencies with copyleft licenses
(cd app && pnpm test && pnpm build)
```

The README has more on [testing on real hardware](README.md#building), and on running a simulated
computer when you only have one real one.

## Notes

- All the state of the agent lives in one main loop, in `crates/hk-daemon/src/engine.rs`. Input is
  the exception, as it's too time-critical to pass through there.
- Different versions have to keep working together. `crates/hk-proto/src/lib.rs` explains how to add
  to the protocol without cutting off copies already out there.
- Never log key presses or anything else that was typed.

## Third-party notices

If you add or update a dependency, regenerate the notices that ship with the program. It needs
[cargo-about](https://github.com/EmbarkStudios/cargo-about):

```sh
node scripts/third-party-notices.mjs
```
