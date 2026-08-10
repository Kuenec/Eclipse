# Eclipse

Eclipse is a Rust runtime that runs the Android x86-64 Roblox client on Linux. It provides the Android-facing runtime, loader, graphics, input, audio, and WebView bridges the client expects, while using the host’s graphics and audio stack. It is not Wine or a full Android emulator.

The project keeps the game client separate: supply your own APK, or configure an optional download source and checksum. Eclipse does not distribute Roblox files or store account credentials in this repository.

## Build

```sh
cargo build --release
cargo test
```

The main executable is in `src/`. Supporting crates live in `crates/`, generated Android framework sources are in `tools/framework-overlay/`, and packaged shader assets are in `shaders/`.

Eclipse is unofficial and is not affiliated with Roblox Corporation.
