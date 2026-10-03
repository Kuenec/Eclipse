use std::sync::{Mutex, PoisonError};

const SEPARATOR: &str = " — ";
const MAX_SUFFIX_CHARS: usize = 128;

static REQUESTED: Mutex<Option<Requested>> = Mutex::new(None);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Requested(Option<String>);

impl Requested {
    pub(super) fn title(&self, base: &str) -> String {
        compose_title(base, self.0.as_deref())
    }
}

pub(crate) fn request(suffix: Option<String>) {
    *REQUESTED.lock().unwrap_or_else(PoisonError::into_inner) = Some(Requested(suffix));
    crate::framework::wake_main_looper();
}

pub(super) fn take_request() -> Option<Requested> {
    REQUESTED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

fn compose_title(base: &str, suffix: Option<&str>) -> String {
    let shown: String = suffix
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_SUFFIX_CHARS)
        .collect();
    let shown = shown.trim();
    if shown.is_empty() {
        return base.to_owned();
    }
    format!("{base}{SEPARATOR}{shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "Eclipse — Roblox";

    #[test]
    fn a_suffix_follows_the_base_title() {
        assert_eq!(compose_title(BASE, None), BASE);
        assert_eq!(compose_title(BASE, Some("")), BASE);
        assert_eq!(
            compose_title(BASE, Some("San Mateo, California, US")),
            "Eclipse — Roblox — San Mateo, California, US"
        );
        assert_eq!(
            Requested(Some("Hesse, DE".to_owned())).title(BASE),
            "Eclipse — Roblox — Hesse, DE"
        );
        assert_eq!(Requested(None).title(BASE), BASE);
    }

    #[test]
    fn control_characters_never_reach_the_title() {
        assert_eq!(
            compose_title(BASE, Some("Frank\nfurt\u{1b}[2J,\t DE\u{7f}")),
            "Eclipse — Roblox — Frankfurt[2J, DE"
        );
        assert_eq!(compose_title(BASE, Some("\r\n\u{0}")), BASE);
    }

    #[test]
    fn a_long_suffix_is_cut_at_128_characters() {
        let long = "é".repeat(MAX_SUFFIX_CHARS + 10);
        let title = compose_title(BASE, Some(&long));
        let suffix = title.strip_prefix("Eclipse — Roblox — ").unwrap();
        assert_eq!(suffix, "é".repeat(MAX_SUFFIX_CHARS));
    }

    #[test]
    fn every_title_keeps_the_eclipse_prefix() {
        for suffix in [None, Some("x"), Some("\n")] {
            assert!(compose_title(BASE, suffix).starts_with("Eclipse — "));
        }
    }
}
