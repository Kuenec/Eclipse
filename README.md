# Eclipse

Eclipse is an open-source Rust runtime for running the Android x86-64 Roblox client on Linux.

Rather than running a full Android virtual machine, Eclipse provides the Android-facing runtime and bridges the client expects while using the host system for graphics, audio, input, networking, and window management. It is not Wine or a full Android emulator.

> Eclipse is under active development. Compatibility and packaging are still evolving.

## Features

- Rust-based runtime and Android compatibility layer
- Native Linux graphics, audio, input, and window integration
- Android x86-64 client support
- WebView and runtime bridging
- User-supplied APKs; Eclipse does not redistribute Roblox client files
- Optional download from a source and checksum configured by the user

## Requirements

- Linux x86-64
- Rust 1.95 or newer
- A C compiler and the native libraries required by the enabled Rust dependencies
- A compatible Android x86-64 Roblox APK and the runtime assets required by Eclipse

## Build

```sh
cargo build --release
cargo test
```

## Usage

```sh
cargo run --release -- run /path/to/Roblox.apk
```

Run `eclipse help` for the available commands. The optional fetch workflow requires a download source that you configure; Eclipse never hard-codes or hosts a Roblox APK source.

## Architecture

The main runtime is in `src/`. Supporting crates are in `crates/`, Android framework overlay sources are in `tools/framework-overlay/`, and shaders are in `shaders/`. Eclipse keeps the game client and account data outside the repository.

## Contributing

Bug reports, compatibility results, and focused pull requests are welcome. Please include the Linux distribution, graphics stack, APK version, and the relevant logs when reporting runtime issues.

## Disclaimer

Eclipse is an independent project and is not affiliated with or endorsed by Roblox Corporation.

## License

Eclipse is released under the [MIT License](LICENSE).

Eclipse is unofficial and is not affiliated with Roblox Corporation.
