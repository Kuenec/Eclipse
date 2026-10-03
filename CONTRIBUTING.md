# Contributing

Bug reports and focused pull requests are welcome. For a bug, include your distro, desktop, GPU and driver, and the log. Never attach APKs, cookies or account data.

## Build

Requirements: Rust 1.95+, a C/C++ toolchain, `pkg-config`, and the ALSA, Fontconfig and FreeType headers. The WebView helper also needs the GTK 4.10+ and WebKitGTK 6.0 (2.42+) headers.

```bash
cargo build --release --locked
cargo build --release --locked --manifest-path crates/eclipse-webview/Cargo.toml
```

A source build also needs the Android runtime the Flatpak bundles (art_standalone, bionic_translation and Android Translation Layer, pinned in the manifest) and the patched framework from `tools/framework-overlay/patch-framework.sh`. Point Eclipse at them with `ECLIPSE_LIBART` and `ECLIPSE_ANDROID_FRAMEWORK_DIR`. The simplest route is to build the Flatpak:

```bash
flatpak-builder --user --install-deps-from=flathub --install --force-clean \
  build/flatpak-app packaging/flatpak/io.github.kuenec.Eclipse.yml
```

The Flatpak build is offline. After changing a `Cargo.lock`, or staging a new one, run `packaging/flatpak/update-cargo-sources.sh`.

## Checks

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Tests that need the real client run when `ECLIPSE_ROBLOX_APK` points at an official APK set, and skip without it. CI runs these checks plus E2E, security audits and the Flatpak source check. Pushes to main and pull requests that change more than docs or tests also build the Flatpak, which fails above the installed-size limit in `packaging/flatpak/check-installed-size.sh`.

Packaging changes must pass `desktop-file-validate` and Flathub's `flatpak-builder-lint`. Each exception in `packaging/flatpak/lint-exceptions.json` has its reason here: `appstream-external-screenshot-url`, because screenshots are served from GitHub, not Flathub's media mirror.

To record frame times, run `flatpak run --filesystem=xdg-run/eclipse-perf:create --env=ECLIPSE_FRAMETIME_LOG=$XDG_RUNTIME_DIR/eclipse-perf/run.bin io.github.kuenec.Eclipse run`, then `tools/perf/frametimes.py $XDG_RUNTIME_DIR/eclipse-perf/run.bin --from 10`. Each run needs a new file.

## Layout

| Path | Contents |
|---|---|
| `src/apk/` | APK parsing, signature verification, install store, downloads |
| `src/https.rs` | Resumable HTTPS downloads from allow-listed hosts |
| `src/links.rs` | Roblox link parsing, Android link rendering and join-code redaction |
| `src/loader/` | ELF/Bionic loader, JNI, NDK, audio and graphics bridges |
| `src/framework.rs` | Android framework natives and lifecycle |
| `src/graphics.rs` | Window, presentation and input |
| `src/webview/`, `crates/eclipse-webview/` | WebView helper and its protocol |
| `crates/eclipse-config/` | Settings schema and config.json loading |
| `tools/framework-overlay/` | Android framework patches and probes |
| `tools/perf/` | Performance measurement harness |
| `packaging/flatpak/` | Flatpak manifest and metadata |

## Releases

Pushing a `vX.Y.Z` tag builds the release and publishes the signed Flatpak repository to GitHub Pages. It needs the `FLATPAK_GPG_PRIVATE_KEY` repository secret. The commit that bumps the version also adds its `<release>` entry and notes to the metainfo, which become the GitHub release notes. A tag build fails while a screenshot URL in the metainfo names `main`; point it at the new tag.
