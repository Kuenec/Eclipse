# Eclipse user guide

Commands are run as `flatpak run io.github.kuenec.Eclipse <command>`.

## Install from your own files

```bash
flatpak run io.github.kuenec.Eclipse install ~/Downloads/base.apk ~/Downloads/split_config.x86_64.apk
```

This accepts the two APKs, a folder holding them, or an `.apks`, `.xapk` or `.apkm` bundle. Eclipse can read your Downloads folder.

## Download from Google Play

`update --play` downloads Roblox from Google Play after a one-time `play-login`.

> [!WARNING]
> Google may restrict accounts that use unofficial Play clients. Use a secondary account.

## Browser Play button

If another app handles Roblox links, make Eclipse the default:

```bash
xdg-mime default io.github.kuenec.Eclipse.UrlHandler.desktop x-scheme-handler/roblox-player x-scheme-handler/roblox
```

## Voice chat

Voice chat uses your system's default microphone. Choose it in your desktop's sound settings.

## Robux

Roblox for Android sells Robux and Premium only through Google Play, so buying them in Eclipse shows Roblox's "Please setup Google Play Store" message. Buy them at [roblox.com](https://www.roblox.com/upgrades/robux) in a browser.

## Controllers

Controllers need Flatpak 1.16 or newer. On older Flatpak, run `flatpak override --user --device=all io.github.kuenec.Eclipse`. A controller SDL does not recognize needs an [SDL mapping](https://wiki.libsdl.org/SDL3/SDL_HINT_GAMECONTROLLERCONFIG): `flatpak override --user --env=SDL_GAMECONTROLLERCONFIG="<mapping>" io.github.kuenec.Eclipse`. Eclipse can read input devices for controllers, which includes keyboards if your user is in the `input` group. To remove that access, run `flatpak override --user --nodevice=input io.github.kuenec.Eclipse`; to only turn controllers off, set `allow_gamepad_permission` to `false`.

## Settings

| Key | Default | Effect |
|---|---|---|
| `fflags` | `{}` | Fast Flags passed to Roblox |
| `touch_mode` | `"off"` | `"on"` sends clicks and touchscreen fingers as touch and shows Roblox's mobile UI; `"fake-off"` does the same with the desktop UI; with `"off"`, a touchscreen's first finger acts as the mouse |
| `graphics_optimization_mode` | `"balanced"` | `"performance"` pins Eclipse to physical cores |
| `enable_gamemode` | `true` | Turns on GameMode while Roblox runs, if GameMode is installed |
| `allow_gamepad_permission` | `true` | `false` turns controllers off |
| `roblox_auto_update` | `true` | `false` stops launches from updating Roblox; `update` and `run --check-update` still do |
| `close_on_leave` | `"browser"` | Closes Eclipse when you leave an experience that a link started; `true` closes it after any experience, `false` never |
| `server_location_indicator_enabled` | `false` | Shows where the Roblox server is in the window title and a notification. ipinfo.io receives the server's address and sees your IP address |

## Files

Eclipse keeps its files in `~/.var/app/io.github.kuenec.Eclipse/`:

| What | Where |
|---|---|
| Settings | `config/eclipse/config.json` |
| Roblox | `data/eclipse/roblox/` |
| Game data and logs | `data/eclipse/app-data/` |
| Roblox's cache, trimmed above 512 MiB | `cache/eclipse/client-cache/` |
| The experience you are in, for stream tools | `data/eclipse/app-data/runtime/session.json`, removed when you leave |
| Captures | `~/Pictures/Roblox/`, or `data/eclipse/app-data/Pictures/` until `xdg-user-dirs-update` sets your Pictures folder |

## Safety

- Eclipse only runs files signed by Roblox Corporation (certificate SHA-256 `44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`), and it never downloads an older version. It keeps the previous Roblox until a new one has been played and closed once: if the new one fails to start twice, Eclipse goes back to it, and `rollback` goes back to it by hand.
- It never modifies the game. The only Fast Flag it sets on its own is `FFlagGameBasicSettingsFramerateCap5`, which shows Roblox's frame rate menu.
