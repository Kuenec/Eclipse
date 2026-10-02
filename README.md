<div align="center">

# 🌘 Eclipse

**Play the official Roblox client on Linux.**

[![Release](https://img.shields.io/github/v/release/Kuenec/Eclipse?color=7c3aed)](https://github.com/Kuenec/Eclipse/releases/latest)
[![CI](https://github.com/Kuenec/Eclipse/actions/workflows/ci.yml/badge.svg)](https://github.com/Kuenec/Eclipse/actions/workflows/ci.yml)
[![License MIT](https://img.shields.io/github/license/Kuenec/Eclipse?color=7c3aed)](LICENSE)

</div>

Eclipse runs Roblox's official Android x86-64 client natively on Linux, with a small Android compatibility layer instead of a full Android VM. It is experimental and not affiliated with Roblox Corporation.

## Install

```bash
flatpak install --user https://kuenec.github.io/Eclipse/io.github.kuenec.Eclipse.flatpakref
```

You need an x86-64 Linux desktop (Wayland or X11) and a Vulkan driver. Eclipse updates with `flatpak update`. A [standalone bundle](https://github.com/Kuenec/Eclipse/releases/latest) is also available.

## Features

- **No setup.** Eclipse downloads and updates Roblox by itself, with no account needed.
- **Official and unmodified.** Only files signed by Roblox run, and the game is never patched.
- **Plays like the desktop client.** Mouse lock, clipboard, input methods and F11 fullscreen.
- **Uncapped frame rate.** Roblox's own frame rate menu, up to 240 FPS on any monitor.
- **Browser Play button.** `roblox://` and `roblox-player:` links open in Eclipse.
- **Sandboxed** in Flatpak, with no telemetry.

## Usage

Start **Eclipse** from your app menu. From a terminal, use `flatpak run io.github.kuenec.Eclipse <command>`:

| Command | What it does |
|---|---|
| `run` | Start Roblox (the default from the app menu) |
| `update` | Check for a newer Roblox now |
| `install <files>` | Install Roblox from your own `base.apk` + `split_config.x86_64.apk`, or an `.apks`/`.xapk` bundle |
| `config` | Show the settings file path and the values in effect |

## Settings

Settings live in `~/.var/app/io.github.kuenec.Eclipse/config/eclipse/config.json`:

```json
{
  "fflags": {},
  "touch_mode": "off"
}
```

`fflags` passes Fast Flags to Roblox. `touch_mode: "on"` sends clicks as touch input. More options are in the [user guide](docs/guide.md#settings).

## How it works

```mermaid
flowchart LR
    APKs["Official Roblox APKs"] --> Verify["Signature check<br/>pinned Roblox certificate"]
    Verify --> Store["Versioned install store"]
    Store --> ART["ART VM + Android framework"]
    Store --> Loader["ELF / Bionic loader<br/>libroblox.so"]
    ART --> Runtime["Eclipse runtime"]
    Loader --> Runtime
    Runtime --> Graphics["Vulkan / EGL"]
    Runtime --> Input["Keyboard, mouse, IME"]
    Runtime --> Audio["AAudio / OpenSL ES"]
    Runtime --> WebView["Sandboxed WebView helper"]
    Graphics --> Linux["Your Linux desktop"]
    Input --> Linux
    Audio --> Linux
    WebView --> Linux
```

## Troubleshooting

- **Something failed?** The log of the last launch is in `~/.var/app/io.github.kuenec.Eclipse/data/eclipse/app-data/logs/eclipse.log`.
- **More detail:** `flatpak run --env=RUST_LOG=debug io.github.kuenec.Eclipse run`
- **Signature error:** the files are not Roblox's official release. Install a clean copy.
- **Uninstall everything:** `flatpak uninstall --user --delete-data io.github.kuenec.Eclipse`

When reporting a bug, include your distro, desktop, GPU and driver, and the log. Never attach APKs, cookies or account data.

## More

- [User guide](docs/guide.md): installing from your own files, Google Play, browser links and file locations.
- [CONTRIBUTING.md](CONTRIBUTING.md): building from source, architecture, tests and releases.

Thanks to [Yoshi-OOF](https://github.com/Yoshi-OOF) for framework, CI and browser launch work.

## License

[MIT](LICENSE). Roblox is a trademark of Roblox Corporation. Eclipse is an independent project and does not redistribute Roblox.
