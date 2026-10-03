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

## Settings

| Key | Default | Effect |
|---|---|---|
| `fflags` | `{}` | Fast Flags passed to Roblox |
| `touch_mode` | `"off"` | `"on"` sends clicks as touch input |
| `graphics_optimization_mode` | `"balanced"` | `"performance"` pins Eclipse to physical cores |
| `enable_gamemode` | `true` | Turns on GameMode while Roblox runs, if GameMode is installed |
| `roblox_auto_update` | `true` | `false` stops launches from updating Roblox; `update` and `run --check-update` still do |

## Files

Eclipse keeps its files in `~/.var/app/io.github.kuenec.Eclipse/`:

| What | Where |
|---|---|
| Settings | `config/eclipse/config.json` |
| Roblox | `data/eclipse/roblox/` |
| Game data and logs | `data/eclipse/app-data/` |
| Roblox's cache, trimmed above 512 MiB | `cache/eclipse/client-cache/` |
| Captures | `~/Pictures/Roblox/`, or `data/eclipse/app-data/Pictures/` until `xdg-user-dirs-update` sets your Pictures folder |

## Safety

- Eclipse only runs files signed by Roblox Corporation (certificate SHA-256 `44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`), and it never downloads an older version. It keeps the previous Roblox until a new one has been played and closed once: if the new one fails to start twice, Eclipse goes back to it, and `rollback` goes back to it by hand.
- It never modifies the game. The only Fast Flag it sets on its own is `FFlagGameBasicSettingsFramerateCap5`, which shows Roblox's frame rate menu.
