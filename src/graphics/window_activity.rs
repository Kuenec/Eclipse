use std::ffi::OsString;
use std::fmt;
use std::time::{Duration, Instant};

use eclipse_config::FrameRateLimit;

use crate::gpu::Graphics;
use crate::loader::present_pacing::PresentPace;

const HIDDEN_PACING_ENV: &str = "ECLIPSE_HIDDEN_PACING";

const HIDDEN_PACING_OFF: &str = "off";

const UNANSWERED_WHEN_HIDDEN: Duration = Duration::from_millis(400);

const HIDDEN_PRESENT_INTERVAL: Duration = Duration::from_millis(200);

const SECOND: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Visibility {
    Visible,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Game,
    Dialog,
    Elsewhere,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HiddenPacing {
    On,
    Off,
}

#[derive(Debug)]
pub struct UnknownHiddenPacing(OsString);

impl HiddenPacing {
    pub(super) fn from_env() -> Result<Self, UnknownHiddenPacing> {
        Self::parse(std::env::var_os(HIDDEN_PACING_ENV))
    }

    fn parse(value: Option<OsString>) -> Result<Self, UnknownHiddenPacing> {
        match value {
            None => Ok(Self::On),
            Some(value) if value == HIDDEN_PACING_OFF => Ok(Self::Off),
            Some(value) => Err(UnknownHiddenPacing(value)),
        }
    }
}

impl fmt::Display for UnknownHiddenPacing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{HIDDEN_PACING_ENV} must be unset or `{HIDDEN_PACING_OFF}`, not {:?}",
            self.0
        )
    }
}

impl std::error::Error for UnknownHiddenPacing {}

#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FrameProbe {
    Send,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Outstanding {
    sent: Instant,
    presents_at_send: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowActivity {
    FrameCallbacks {
        visibility: Visibility,
        outstanding: Option<Outstanding>,
    },
    WindowSystem {
        occluded: bool,
        minimized: bool,
    },
}

impl Default for WindowActivity {
    fn default() -> Self {
        Self::WindowSystem {
            occluded: false,
            minimized: false,
        }
    }
}

impl WindowActivity {
    pub(super) const fn frame_callbacks() -> Self {
        Self::FrameCallbacks {
            visibility: Visibility::Visible,
            outstanding: None,
        }
    }

    pub(super) const fn visibility(self) -> Visibility {
        match self {
            Self::FrameCallbacks { visibility, .. } => visibility,
            Self::WindowSystem {
                occluded: false,
                minimized: false,
            } => Visibility::Visible,
            Self::WindowSystem { .. } => Visibility::Hidden,
        }
    }

    pub(super) fn tick(
        &mut self,
        now: Instant,
        presents: u64,
        minimized: Option<bool>,
    ) -> FrameProbe {
        match self {
            Self::FrameCallbacks {
                visibility,
                outstanding: Some(probe),
            } => {
                if now.duration_since(probe.sent) > UNANSWERED_WHEN_HIDDEN
                    && presents > probe.presents_at_send
                {
                    *visibility = Visibility::Hidden;
                }
                FrameProbe::Skip
            }
            Self::FrameCallbacks { outstanding, .. } => {
                *outstanding = Some(Outstanding {
                    sent: now,
                    presents_at_send: presents,
                });
                FrameProbe::Send
            }
            Self::WindowSystem {
                minimized: shown_minimized,
                ..
            } => {
                if let Some(minimized) = minimized {
                    *shown_minimized = minimized;
                }
                FrameProbe::Skip
            }
        }
    }

    pub(super) fn frame_shown(&mut self, now: Instant, presents: u64) -> FrameProbe {
        let Self::FrameCallbacks {
            visibility,
            outstanding,
        } = self
        else {
            return FrameProbe::Skip;
        };
        let Some(probe) = outstanding.take() else {
            return FrameProbe::Skip;
        };
        if now.duration_since(probe.sent) <= UNANSWERED_WHEN_HIDDEN {
            *visibility = Visibility::Visible;
            return FrameProbe::Skip;
        }
        *outstanding = Some(Outstanding {
            sent: now,
            presents_at_send: presents,
        });
        FrameProbe::Send
    }

    pub(super) fn occluded(&mut self, occluded: bool) {
        if let Self::WindowSystem {
            occluded: shown_occluded,
            ..
        } = self
        {
            *shown_occluded = occluded;
        }
    }
}

pub(super) fn pace_for(
    graphics: Graphics,
    focus: Focus,
    visibility: Visibility,
    hidden_pacing: HiddenPacing,
    unfocused_limit: Option<FrameRateLimit>,
) -> PresentPace {
    if let Graphics::Gles(_) = graphics {
        return PresentPace::Unpaced;
    }
    let limit_interval = unfocused_limit.map(|limit| SECOND / u32::from(limit.per_second()));
    match (focus, visibility, hidden_pacing, limit_interval) {
        (Focus::Game | Focus::Dialog, _, _, _) => PresentPace::Unpaced,
        (Focus::Elsewhere, Visibility::Hidden, HiddenPacing::On, limit) => {
            PresentPace::Every(limit.map_or(HIDDEN_PRESENT_INTERVAL, |limit| {
                limit.max(HIDDEN_PRESENT_INTERVAL)
            }))
        }
        (Focus::Elsewhere, _, _, Some(limit)) => PresentPace::Every(limit),
        (Focus::Elsewhere, _, _, None) => PresentPace::Unpaced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::GlesReason;

    const AFTER_THE_LIMIT: Duration = Duration::from_millis(401);

    fn limit(per_second: u8) -> Option<FrameRateLimit> {
        Some(FrameRateLimit::new(per_second).expect("a frame-rate limit"))
    }

    fn unfocused_vulkan(
        visibility: Visibility,
        hidden_pacing: HiddenPacing,
        unfocused_limit: Option<FrameRateLimit>,
    ) -> PresentPace {
        pace_for(
            Graphics::Vulkan,
            Focus::Elsewhere,
            visibility,
            hidden_pacing,
            unfocused_limit,
        )
    }

    fn every_case(mut check: impl FnMut(Visibility, HiddenPacing, Option<FrameRateLimit>)) {
        for visibility in [Visibility::Visible, Visibility::Hidden] {
            for hidden_pacing in [HiddenPacing::On, HiddenPacing::Off] {
                for unfocused_limit in [None, limit(1), limit(30), limit(240)] {
                    check(visibility, hidden_pacing, unfocused_limit);
                }
            }
        }
    }

    #[test]
    fn a_focused_game_window_is_never_paced() {
        every_case(|visibility, hidden_pacing, unfocused_limit| {
            assert_eq!(
                pace_for(
                    Graphics::Vulkan,
                    Focus::Game,
                    visibility,
                    hidden_pacing,
                    unfocused_limit
                ),
                PresentPace::Unpaced
            );
        });
    }

    #[test]
    fn a_focused_dialog_keeps_the_game_unpaced() {
        every_case(|visibility, hidden_pacing, unfocused_limit| {
            assert_eq!(
                pace_for(
                    Graphics::Vulkan,
                    Focus::Dialog,
                    visibility,
                    hidden_pacing,
                    unfocused_limit
                ),
                PresentPace::Unpaced
            );
        });
    }

    #[test]
    fn opengl_es_is_never_paced() {
        for graphics in [
            Graphics::Gles(GlesReason::Configured),
            Graphics::Gles(GlesReason::NoUsableVulkan),
        ] {
            every_case(|visibility, hidden_pacing, unfocused_limit| {
                assert_eq!(
                    pace_for(
                        graphics,
                        Focus::Elsewhere,
                        visibility,
                        hidden_pacing,
                        unfocused_limit
                    ),
                    PresentPace::Unpaced
                );
            });
        }
    }

    #[test]
    fn a_hidden_window_presents_5_frames_a_second_or_fewer() {
        let hidden = |unfocused_limit| {
            unfocused_vulkan(Visibility::Hidden, HiddenPacing::On, unfocused_limit)
        };
        assert_eq!(hidden(None), PresentPace::Every(Duration::from_millis(200)));
        assert_eq!(
            hidden(limit(30)),
            PresentPace::Every(Duration::from_millis(200))
        );
        assert_eq!(
            hidden(limit(2)),
            PresentPace::Every(Duration::from_millis(500))
        );
        assert_eq!(hidden(limit(1)), PresentPace::Every(Duration::from_secs(1)));
    }

    #[test]
    fn an_unfocused_window_follows_only_the_configured_limit_unless_hidden_pacing_applies() {
        for (visibility, hidden_pacing) in [
            (Visibility::Visible, HiddenPacing::On),
            (Visibility::Visible, HiddenPacing::Off),
            (Visibility::Hidden, HiddenPacing::Off),
        ] {
            assert_eq!(
                unfocused_vulkan(visibility, hidden_pacing, None),
                PresentPace::Unpaced
            );
            assert_eq!(
                unfocused_vulkan(visibility, hidden_pacing, limit(30)),
                PresentPace::Every(Duration::from_nanos(33_333_333))
            );
            assert_eq!(
                unfocused_vulkan(visibility, hidden_pacing, limit(240)),
                PresentPace::Every(Duration::from_nanos(4_166_666))
            );
        }
    }

    #[test]
    fn hidden_pacing_is_on_unless_the_environment_turns_it_off() {
        assert_eq!(HiddenPacing::parse(None).expect("unset"), HiddenPacing::On);
        assert_eq!(
            HiddenPacing::parse(Some("off".into())).expect("off"),
            HiddenPacing::Off
        );
        for value in ["", "on", "0", "OFF", "off "] {
            let error = HiddenPacing::parse(Some(value.into())).expect_err(value);
            assert_eq!(
                error.to_string(),
                format!("ECLIPSE_HIDDEN_PACING must be unset or `off`, not {value:?}")
            );
        }
    }

    #[test]
    fn an_unanswered_probe_without_presents_never_hides_the_window() {
        let start = Instant::now();
        let mut activity = WindowActivity::frame_callbacks();
        assert_eq!(activity.tick(start, 7, None), FrameProbe::Send);
        for seconds in 1..10 {
            let later = start + Duration::from_secs(seconds);
            assert_eq!(activity.tick(later, 7, None), FrameProbe::Skip);
            assert_eq!(activity.visibility(), Visibility::Visible);
        }
    }

    #[test]
    fn a_probe_unanswered_for_400_ms_while_the_game_presents_hides_the_window() {
        let start = Instant::now();
        let mut activity = WindowActivity::frame_callbacks();
        assert_eq!(activity.tick(start, 7, None), FrameProbe::Send);
        let at_the_limit = start + UNANSWERED_WHEN_HIDDEN;
        assert_eq!(activity.tick(at_the_limit, 20, None), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Visible);
        assert_eq!(
            activity.tick(start + AFTER_THE_LIMIT, 20, None),
            FrameProbe::Skip
        );
        assert_eq!(activity.visibility(), Visibility::Hidden);
    }

    #[test]
    fn a_prompt_answer_shows_the_window_and_the_next_tick_probes_again() {
        let start = Instant::now();
        let mut activity = WindowActivity::frame_callbacks();
        assert_eq!(activity.tick(start, 0, None), FrameProbe::Send);
        let answered = start + Duration::from_millis(16);
        assert_eq!(activity.frame_shown(answered, 1), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Visible);
        let next_tick = start + Duration::from_millis(500);
        assert_eq!(activity.tick(next_tick, 30, None), FrameProbe::Send);
    }

    #[test]
    fn a_late_answer_probes_again_and_only_a_prompt_one_shows_the_window() {
        let start = Instant::now();
        let mut activity = WindowActivity::frame_callbacks();
        assert_eq!(activity.tick(start, 0, None), FrameProbe::Send);
        assert_eq!(
            activity.tick(start + Duration::from_millis(500), 30, None),
            FrameProbe::Skip
        );
        assert_eq!(activity.visibility(), Visibility::Hidden);
        let late = start + Duration::from_secs(3);
        assert_eq!(activity.frame_shown(late, 45), FrameProbe::Send);
        assert_eq!(activity.visibility(), Visibility::Hidden);
        let prompt = late + Duration::from_millis(216);
        assert_eq!(activity.frame_shown(prompt, 46), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Visible);
    }

    #[test]
    fn a_frame_without_a_probe_changes_nothing() {
        let now = Instant::now();
        let mut activity = WindowActivity::frame_callbacks();
        assert_eq!(activity.frame_shown(now, 3), FrameProbe::Skip);
        assert_eq!(activity, WindowActivity::frame_callbacks());
    }

    #[test]
    fn an_x11_window_is_hidden_while_minimized_or_occluded() {
        let now = Instant::now();
        let mut activity = WindowActivity::default();
        assert_eq!(activity.visibility(), Visibility::Visible);
        assert_eq!(activity.tick(now, 5, Some(true)), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Hidden);
        assert_eq!(activity.tick(now, 5, None), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Hidden);
        assert_eq!(activity.tick(now, 5, Some(false)), FrameProbe::Skip);
        assert_eq!(activity.visibility(), Visibility::Visible);
        activity.occluded(true);
        assert_eq!(activity.visibility(), Visibility::Hidden);
        activity.occluded(false);
        assert_eq!(activity.visibility(), Visibility::Visible);
        assert_eq!(activity.frame_shown(now, 6), FrameProbe::Skip);
    }

    #[test]
    fn occlusion_reports_do_not_override_frame_callbacks() {
        let mut activity = WindowActivity::frame_callbacks();
        activity.occluded(true);
        assert_eq!(activity.visibility(), Visibility::Visible);
    }
}
