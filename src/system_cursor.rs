#![forbid(unsafe_code)]

use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::TouchMode;

const TRANSPARENT_CURSOR_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60, 0x60, 0x00, 0x00,
    0x00, 0x03, 0x00, 0x01, 0x2b, 0x09, 0x4d, 0x84, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

const ROBLOX_DESKTOP_CURSOR_ASSETS: &[&str] = &[
    "content/textures/ArrowCursor.png",
    "content/textures/ArrowFarCursor.png",
    "content/textures/Cursors/CrossMouseIcon.png",
    "content/textures/Cursors/KeyboardMouse/ArrowCursor.png",
    "content/textures/Cursors/KeyboardMouse/ArrowFarCursor.png",
    "content/textures/Cursors/KeyboardMouse/IBeamCursor.png",
    "content/textures/Cursors/mouseIconCameraOrbit.png",
    "content/textures/Cursors/mouseIconCameraTrack.png",
    "content/textures/Cursors/mouseIconCameraZoom.png",
    "content/textures/IBeamCursor.png",
    "content/textures/MouseLockedCursor.png",
    "content/textures/advCursor-default.png",
    "content/textures/advCursor-openedHand.png",
    "content/textures/advCursor-white.png",
];

static SYSTEM_CURSOR_OVERRIDE_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn install(assets_dir: &Path, touch_mode: TouchMode) -> io::Result<usize> {
    SYSTEM_CURSOR_OVERRIDE_ENABLED.store(false, Ordering::Release);
    if touch_mode != TouchMode::Off {
        return Ok(0);
    }

    let changed = replace_extracted_cursor_assets(assets_dir)?;
    SYSTEM_CURSOR_OVERRIDE_ENABLED.store(true, Ordering::Release);
    Ok(changed)
}

pub(crate) fn replacement_apk_entry(name: &str) -> Option<&'static [u8]> {
    replacement_for(name, SYSTEM_CURSOR_OVERRIDE_ENABLED.load(Ordering::Acquire))
}

fn replacement_for(name: &str, enabled: bool) -> Option<&'static [u8]> {
    if !enabled {
        return None;
    }
    let relative = name.strip_prefix("assets/").unwrap_or(name);
    ROBLOX_DESKTOP_CURSOR_ASSETS
        .contains(&relative)
        .then_some(TRANSPARENT_CURSOR_PNG)
}

fn replace_extracted_cursor_assets(assets_dir: &Path) -> io::Result<usize> {
    let mut changed = 0;
    for relative in ROBLOX_DESKTOP_CURSOR_ASSETS {
        let path = assets_dir.join(relative);
        match std::fs::read(&path) {
            Ok(bytes) if bytes == TRANSPARENT_CURSOR_PNG => continue,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(with_path(error, &path)),
        }

        write_atomically(&path, TRANSPARENT_CURSOR_PNG)?;
        changed += 1;
    }
    Ok(changed)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cursor.png");
    let temporary = path.with_file_name(format!(
        ".{file_name}.eclipse-system-cursor-{}.partial",
        std::process::id()
    ));

    let result = (|| {
        let mut output = std::fs::File::create(&temporary)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| with_path(error, path))
}

fn with_path(error: io::Error, path: &Path) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Component;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eclipse-system-cursor-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).expect("create cursor test directory");
        path
    }

    #[test]
    fn transparent_cursor_is_a_one_pixel_grayscale_alpha_png() {
        assert_eq!(&TRANSPARENT_CURSOR_PNG[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&TRANSPARENT_CURSOR_PNG[12..16], b"IHDR");
        assert_eq!(
            u32::from_be_bytes(TRANSPARENT_CURSOR_PNG[16..20].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_be_bytes(TRANSPARENT_CURSOR_PNG[20..24].try_into().unwrap()),
            1
        );
        assert_eq!(TRANSPARENT_CURSOR_PNG[24], 8, "eight-bit channels");
        assert_eq!(
            TRANSPARENT_CURSOR_PNG[25], 4,
            "grayscale + alpha color type"
        );
    }

    #[test]
    fn cursor_asset_list_is_unique_relative_and_traversal_free() {
        let unique: HashSet<_> = ROBLOX_DESKTOP_CURSOR_ASSETS.iter().copied().collect();
        assert_eq!(unique.len(), ROBLOX_DESKTOP_CURSOR_ASSETS.len());
        for name in ROBLOX_DESKTOP_CURSOR_ASSETS {
            assert!(
                Path::new(name)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_))),
                "cursor override path must stay beneath the asset root: {name}"
            );
        }
    }

    #[test]
    fn replacement_matches_both_apk_and_asset_relative_names_only_when_enabled() {
        for name in ROBLOX_DESKTOP_CURSOR_ASSETS {
            assert_eq!(replacement_for(name, true), Some(TRANSPARENT_CURSOR_PNG));
            assert_eq!(
                replacement_for(&format!("assets/{name}"), true),
                Some(TRANSPARENT_CURSOR_PNG)
            );
            assert_eq!(replacement_for(name, false), None);
        }
        assert_eq!(
            replacement_for("content/textures/Cursors/Gamepad/Pointer.png", true),
            None
        );
        assert_eq!(
            replacement_for("content/textures/GunCursor.png", true),
            None
        );
    }

    #[test]
    fn extracted_override_is_atomic_idempotent_and_leaves_custom_cursors_untouched() {
        let root = temp_dir("files");
        let standard = root.join(ROBLOX_DESKTOP_CURSOR_ASSETS[0]);
        let custom = root.join("content/textures/GunCursor.png");
        std::fs::create_dir_all(standard.parent().unwrap()).expect("create standard parent");
        std::fs::write(&standard, b"ROBLOX-WHITE-CURSOR").expect("write standard cursor");
        std::fs::write(&custom, b"GAME-CUSTOM-CURSOR").expect("write custom cursor");

        assert_eq!(replace_extracted_cursor_assets(&root).unwrap(), 1);
        assert_eq!(std::fs::read(&standard).unwrap(), TRANSPARENT_CURSOR_PNG);
        assert_eq!(std::fs::read(&custom).unwrap(), b"GAME-CUSTOM-CURSOR");
        assert_eq!(replace_extracted_cursor_assets(&root).unwrap(), 0);

        std::fs::remove_dir_all(root).ok();
    }
}
