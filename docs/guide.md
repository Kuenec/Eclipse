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

## Settings

| Key | Default | Effect |
|---|---|---|
| `fflags` | `{}` | Fast Flags passed to Roblox |
| `touch_mode` | `"off"` | `"on"` sends clicks as touch input |
| `graphics_optimization_mode` | `"balanced"` | `"performance"` pins Eclipse to physical cores |

## Files

Everything lives in `~/.var/app/io.github.kuenec.Eclipse/`:

| What | Where |
|---|---|
| Settings | `config/eclipse/config.json` |
| Roblox | `data/eclipse/roblox/` |
| Game data and logs | `data/eclipse/app-data/` |

## Safety

- Eclipse only runs files signed by Roblox Corporation (certificate SHA-256 `44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`), and it never installs an older version.
- It never modifies the game. The only Fast Flag it sets on its own is `FFlagGameBasicSettingsFramerateCap5`, which shows Roblox's frame rate menu.
