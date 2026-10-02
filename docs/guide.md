# Eclipse user guide

The [README](../README.md) covers installing and everyday use. This page has the details.

Commands below are written as `eclipse <command>`. With the Flatpak, run `flatpak run io.github.kuenec.Eclipse <command>`, or add `alias eclipse='flatpak run io.github.kuenec.Eclipse'` to your shell.

## Other ways to install Eclipse

- Add the repository and install from it (Flathub must be configured, because the runtime comes from there):

  ```bash
  flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
  flatpak remote-add --user --if-not-exists eclipse https://kuenec.github.io/Eclipse/io.github.kuenec.Eclipse.flatpakrepo
  flatpak install --user eclipse io.github.kuenec.Eclipse
  ```

- Download `eclipse-x86_64.flatpak` from the [latest release](https://github.com/Kuenec/Eclipse/releases/latest) and run `flatpak install --user eclipse-x86_64.flatpak`. `flatpak update` keeps it current afterwards.

## Getting Roblox

Eclipse does not include Roblox. It installs the official Android client into its own data directory and runs only files signed by Roblox Corporation, as described in [What Eclipse checks and what it never does](#what-eclipse-checks-and-what-it-never-does).

### Automatic download

You do not need to do anything: the first time you start Eclipse, it downloads the newest Roblox client that has an x86-64 build, which APKCombo often adds some time after a new release, verifies Roblox's signature and installs it, then launches it. No Google or other account is needed. Eclipse gets the client from [APKCombo](https://apkcombo.com/roblox/com.roblox.client/), which mirrors Roblox's own Google Play release files; Eclipse keeps only `base.apk` and `split_config.x86_64.apk` from the download and discards everything if Roblox's signature does not verify.

After that, starting Eclipse checks for a newer Roblox version at most every six hours and installs it before launching. If the check fails, Eclipse starts the version you already have, shows a warning, and checks again 30 minutes later at the earliest. A download that stalls or drops continues where it stopped. To check right away:

```bash
eclipse update
```

Eclipse never replaces the installed client with an older version, and it keeps the previous version until the new one is installed.

### Install the client from APK files

You can also install the official files yourself, for example ones you downloaded elsewhere.

Eclipse needs two files from Roblox's official Android release: `base.apk` and `split_config.x86_64.apk`, which holds the x86-64 engine. `eclipse install` accepts any of these:

- the two APK files, under any file names
- a directory that holds them as `base.apk` and `split_config.x86_64.apk`
- an `.apks`, `.xapk` or `.apkm` bundle that contains them (encrypted bundles, such as newer `.apkm` files, are not supported)
- a single APK that already contains the x86-64 engine

```bash
eclipse install ~/Downloads/base.apk ~/Downloads/split_config.x86_64.apk
eclipse install ~/Downloads/roblox.apks
```

The Flatpak can read your Downloads folder, so keep the files there. To let it read another folder, grant access once, for example with `flatpak override --user --filesystem=~/Games/roblox:ro io.github.kuenec.Eclipse`.

Eclipse verifies the signature of every file before it installs anything. If a file is not Roblox's official, unmodified release, the install stops and the client you already have stays in place.

### Download from Google Play instead

`eclipse update --play` downloads the client from Google Play with your own Google account instead of APKCombo. It needs a one-time `eclipse play-login`, which explains each step. Automatic updates always use APKCombo.

> [!WARNING]
> Google's terms do not allow unofficial Play clients, and Google may restrict an account that uses one. Use a secondary Google account, not your main one.

## Playing

Start **Eclipse** from your application menu. Its window shows the update check, the download and any error until Roblox opens, and an error names Eclipse's log of that launch (see [Where Eclipse keeps its files](#where-eclipse-keeps-its-files)). Only one Roblox client runs at a time: starting Eclipse again, or clicking Play in a browser while Roblox is open, shows a message instead of a second copy. To see all of Eclipse's messages, start it from a terminal instead:

```bash
flatpak run io.github.kuenec.Eclipse run
```

`flatpak run io.github.kuenec.Eclipse` without a command prints the help. `eclipse run <PATH>` runs an APK file, or a directory that holds the split set, without installing it; the same signature check applies.

### Browser Play button

The Flatpak registers Eclipse for `roblox-player:` and `roblox://` links, so the Play button on the Roblox website opens the experience in Eclipse. If another app already handles these links, make Eclipse the default by running this outside the sandbox:

```bash
xdg-mime default io.github.kuenec.Eclipse.UrlHandler.desktop x-scheme-handler/roblox-player x-scheme-handler/roblox
```

Outside the Flatpak, run `eclipse install-url-handler` once to register the binary you are running.

Eclipse reads only the place ID from the link. It discards the launch ticket that `roblox-player:` links carry, and the client opens the place with the account signed in inside it. Browser launches start the installed client without checking for a Roblox update first.

### Mouse and keyboard

- Over the game, Eclipse hides the desktop cursor, because Roblox draws its own cursor as it does on Android. The desktop cursor comes back over web pages the client opens, such as sign-in pages.
- Hold the right mouse button to turn the camera. The cursor stays where it is while you drag and is at the same spot when you release the button.
- When the game locks the mouse, as in shift lock and first person, Eclipse locks the pointer too.
- On Wayland, Eclipse uses the compositor's pointer lock. On X11, which has no pointer lock, it confines the cursor to the window and moves it back to where the lock began.
- Switching to another window releases the lock and every key and mouse button the game still holds.
- F11 toggles fullscreen.

This applies with the default `touch_mode` of `"off"`.

In a Roblox text box, Enter submits a single-line box and starts a new line in a multi-line box, and Escape leaves the box, as on Android. The arrow keys, Home, End, Delete and Ctrl+Backspace edit as usual, and Ctrl+A, Ctrl+C, Ctrl+X and Ctrl+V use the desktop clipboard on Wayland and X11. Input methods such as fcitx5 and IBus work while a text box has focus. Password boxes take keys directly, without the input method, so it cannot show or learn what you type.

## Settings

`eclipse config` prints the path of the configuration file and the values in effect. The file is optional plain JSON, and keys you leave out keep their defaults. With the Flatpak it is `~/.var/app/io.github.kuenec.Eclipse/config/eclipse/config.json`; otherwise it is `~/.config/eclipse/config.json`. Changes take effect the next time you start Eclipse.

```json
{
  "graphics_optimization_mode": "performance",
  "touch_mode": "off",
  "fflags": {}
}
```

| Key | Default | Effect |
|---|---|---|
| `fflags` | `{}` | Fast Flags for the client. Eclipse writes these to the client's `ClientAppSettings.json` together with one default of its own, `"FFlagGameBasicSettingsFramerateCap5": "True"`, which shows the client's Maximum Frame Rate setting. A value you give that flag replaces the default, so `{"FFlagGameBasicSettingsFramerateCap5": "False"}` turns it off. |
| `graphics_optimization_mode` | `"balanced"` | `"performance"` pins Eclipse to one logical CPU per physical core on CPUs with SMT and at least eight physical cores. `"quality"` and `"balanced"` currently behave the same. |
| `touch_mode` | `"off"` | `"off"` delivers the mouse to the game as a desktop mouse. `"on"` and `"fake-off"` deliver clicks as touch input and keep the desktop cursor visible. `"on"` also tells the client that the device has a touchscreen and is not a PC; `"off"` and `"fake-off"` report a PC without a touchscreen. |
| `webview_allow_unsandboxed` | `false` | Outside the Flatpak only: lets the WebView helper run without Chromium's sandbox when the host cannot provide one. The Flatpak always sandboxes it. |
| `webview_helper_path` | `null` | Overrides the path of the `eclipse-webview` helper. It is only needed for source builds where the helper is neither next to the `eclipse` binary nor in `crates/eclipse-webview/target/`. Leave it unset in the Flatpak. |

`eclipse config` also lists keys such as `use_opengl`, `enable_gamemode` and `discord_rpc_enabled`. Eclipse accepts them but does not act on them yet.

### Frame rate

The client's own Maximum Frame Rate setting, under Settings in the in-game menu, is the only frame-rate limit Eclipse leaves in place. It offers 60, 120, 144, 160, 165, 180, 200 and 240 FPS, and Default, which is 60 FPS in the Android client. Without Eclipse's default flag the row is hidden and the client always runs at Default. Eclipse does not pace the client's frames.

The client asks the graphics driver for immediate presentation, which does not wait for the monitor's refresh. Where the driver offers it, Eclipse passes that request through unchanged. Some drivers offer immediate presentation only when the compositor supports tearing control, and otherwise offer only FIFO, which waits for every refresh and would hold the game to the monitor's refresh rate, and mailbox, which does not wait. On those systems Eclipse still offers the client immediate presentation and carries it out with mailbox: the game renders up to its limit, and at each refresh the compositor shows the newest finished frame, without tearing. A frame that a newer one replaces before the next refresh is never shown.

Eclipse tells the client the refresh rate of the monitor the window is on and any other refresh rates the display server lists for that monitor at the same resolution, and updates them when the window moves to another monitor or the mode changes. Some Wayland compositors, Hyprland among them, announce only the current mode, so the client sees the current refresh rate and any other rate that monitor has switched to since Eclipse started, not every rate the monitor supports. When the host reports no refresh rate, the client keeps Android's default of 60 Hz, or the last rate Eclipse reported. The client uses these rates for its own performance tuning but does not limit the frame rate to them, so a limit above the monitor's refresh rate still applies. On a variable refresh rate monitor, the rate reported is the top of its range, the refresh rate of its current mode.

## Where Eclipse keeps its files

These paths assume the default XDG base directories.

| What | Flatpak, under `~/.var/app/io.github.kuenec.Eclipse/` | Outside the Flatpak |
|---|---|---|
| Settings | `config/eclipse/config.json` | `~/.config/eclipse/config.json` |
| Installed Roblox client (current and previous version) | `data/eclipse/roblox/<versionCode>/` | `~/.local/share/eclipse/roblox/<versionCode>/` |
| Game data, WebView profile and staged Fast Flags | `data/eclipse/app-data/` | `~/.local/share/eclipse/app-data/` |
| Log of the last launch: `eclipse.log`, and in long sessions `eclipse.log.1` with the 8 MiB before it | `data/eclipse/app-data/logs/` | `~/.local/share/eclipse/app-data/logs/` |
| Last update check | `data/eclipse/roblox/last-update-check.json` | `~/.local/share/eclipse/roblox/last-update-check.json` |
| Google Play sign-in (only with `play-login`), readable only by you | `data/eclipse/google-play.json` | `~/.local/share/eclipse/google-play.json` |
| Extracted native libraries | `cache/eclipse/native-libs/` | `~/.cache/eclipse/native-libs/` |

To remove Eclipse, the installed Roblox client and all of this data:

```bash
flatpak uninstall --user --delete-data io.github.kuenec.Eclipse
flatpak remote-delete --user eclipse
```

Run the second command only if you added the `eclipse` remote, either by keeping it when you installed from the `.flatpakref` or with `flatpak remote-add`.

## What Eclipse checks and what it never does

- Every APK that Eclipse installs or runs must carry a valid APK Signature Scheme v2 signature from Roblox Corporation's certificate, whose SHA-256 digest is `44932ea35a17a267372d71b54d1a0cb3da0dca5113e94406ae2fe18090ba1477`. If the base APK also has a v3 or v3.1 signature, its key-rotation proof must start at that certificate. A file changed after signing fails the check. Eclipse checks at install time and again at every launch.
- Eclipse does not modify or patch the client, and it does not host, mirror or redistribute it. Downloads come from APKCombo's copy of Roblox's Google Play release (or Google Play itself with `--play`) over HTTPS from an allowlisted host, and must match the size and hash that the source declares before the signature check runs.
- Eclipse never installs an older Roblox version than the one you have.
- Eclipse sets exactly one Fast Flag of its own, `FFlagGameBasicSettingsFramerateCap5` set to `True`, which makes the client show its Maximum Frame Rate setting as Roblox already does for its Windows and Mac clients. It goes through the client's own `ClientAppSettings.json` override file, and everything else in that file comes from the `fflags` in your settings; `{"FFlagGameBasicSettingsFramerateCap5": "False"}` there replaces the default and turns the setting off.
- When the client asks the Android package manager for its signing certificates, Eclipse reports the real certificates from the verified APK.
- Browser launches hand only the place ID to the client.

## Troubleshooting

- If Roblox closes or fails to start, read `logs/eclipse.log` in Eclipse's app-data directory (see [Where Eclipse keeps its files](#where-eclipse-keeps-its-files)). Starting Roblox again replaces it, so copy it first.
- Start Eclipse from a terminal to see what it is doing. For more detail, run `flatpak run --env=RUST_LOG=debug io.github.kuenec.Eclipse run`.
- If the first download fails, Eclipse prints why. Run `eclipse update` to try again, or install the files yourself with `eclipse install`.
- A signature error means the files are not Roblox's official, unmodified release. Get a clean copy of the files and install again.
- Eclipse hands the client its `ClientAppSettings.json` through a small library that it keeps in the app-data directory and loads into itself at every start, so that directory must allow executable files and its path must not contain a colon or semicolon. If Eclipse stops with an error about this, set `ECLIPSE_APP_DATA_DIR` to a directory that meets both.
- To report a problem, see [CONTRIBUTING.md](../CONTRIBUTING.md) for what to include.
