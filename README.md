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

- **No setup.** On first start Eclipse downloads Roblox, verifies it and launches it. It keeps Roblox updated by itself, with no account needed.
- **Official and unmodified.** Every file must carry Roblox Corporation's signature (certificate SHA-256 `44932ea3…ba1477`), checked at install and at every launch. Eclipse never patches the game.
- **Plays like the desktop client.** Mouse lock and right-click camera, a full keyboard in text boxes, clipboard, input methods, and F11 fullscreen.
- **Your frame rate.** Roblox's own Maximum Frame Rate setting (up to 240 FPS) is the only limit, whatever your monitor's refresh rate.
- **Browser Play button.** `roblox://` and `roblox-player:` links open in Eclipse.
- **Sandboxed** in Flatpak, with no telemetry.

## Usage

Start **Eclipse** from your app menu. Its window shows downloads and errors until Roblox opens. Commands are run with `flatpak run io.github.kuenec.Eclipse <command>`:

| Command | What it does |
|---|---|
| `run` | Start Roblox (the default from the app menu) |
| `update` | Check for a newer Roblox now |
| `install <files>` | Install Roblox from your own `base.apk` + `split_config.x86_64.apk`, or an `.apks`/`.xapk` bundle |
| `config` | Show the settings file path and the values in effect |

## Settings

Settings live in `~/.var/app/io.github.kuenec.Eclipse/config/eclipse/config.json`. Every key is optional:

```json
{
  "fflags": {},
  "touch_mode": "off",
  "graphics_optimization_mode": "balanced"
}
```

- `fflags` adds Fast Flags for Roblox. Eclipse sets one by default, `FFlagGameBasicSettingsFramerateCap5`, which shows Roblox's frame rate menu. Set it to `"False"` to hide the menu.
- `touch_mode` set to `"on"` sends clicks as touch input.
- `graphics_optimization_mode` set to `"performance"` pins Eclipse to physical cores on large SMT CPUs.

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

Roblox's Java code runs in ART with a patched Android framework. Its native engine is loaded by Eclipse's own ELF loader, with Android's graphics, input and audio interfaces mapped onto the Linux desktop.

## Troubleshooting

- **Something failed?** The log of the last launch is in `~/.var/app/io.github.kuenec.Eclipse/data/eclipse/app-data/logs/eclipse.log`.
- **More detail:** `flatpak run --env=RUST_LOG=debug io.github.kuenec.Eclipse run`
- **Signature error:** the files are not Roblox's official release. Install a clean copy.
- **Uninstall everything:** `flatpak uninstall --user --delete-data io.github.kuenec.Eclipse`

When reporting a bug, include your distro, desktop, GPU and driver, and the log. Never attach APKs, cookies or account data.

## More

- [User guide](docs/guide.md): Google Play downloads, file locations, browser links, controls and frame-rate details.
- [CONTRIBUTING.md](CONTRIBUTING.md): building from source, architecture, tests and releases.

Thanks to [Yoshi-OOF](https://github.com/Yoshi-OOF) for framework, CI and browser launch work.

## License

[MIT](LICENSE). Roblox is a trademark of Roblox Corporation. Eclipse is an independent project and does not redistribute Roblox.
