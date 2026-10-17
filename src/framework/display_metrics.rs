use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use jni::objects::JObject;
use jni::strings::JNIStr;
use jni::vm::JavaVM;
use jni::{jni_str, Env, JValue};

use super::{checked, int_field_sig, publish_window_size, FrameworkError};
use crate::runtime::Vm;

const BASELINE_DPI: i32 = 160;
const MIN_DPI: i32 = 120;
const MAX_DPI: i32 = 640;

const SCREENLAYOUT_LONG_NO: i32 = 0x10;
const SCREENLAYOUT_LONG_YES: i32 = 0x20;

const WINDOWLESS: DisplayModel = DisplayModel {
    width_px: 1280,
    height_px: 720,
    density_dpi: BASELINE_DPI,
};

const DISPLAY_METRICS_CLASS: &JNIStr = jni_str!("android/util/DisplayMetrics");
const DENSITY_DEVICE_FIELD: &JNIStr = jni_str!("DENSITY_DEVICE");

static APPLIED: OnceLock<DisplayModel> = OnceLock::new();
static SCALE_DIFFERENCE_NOTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Portrait,
    Landscape,
}

impl Orientation {
    const fn configuration_value(self) -> i32 {
        match self {
            Self::Portrait => 1,
            Self::Landscape => 2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::Landscape => "landscape",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeClass {
    Small,
    Normal,
    Large,
    XLarge,
}

impl SizeClass {
    const fn of(long_dp: i32, short_dp: i32) -> Self {
        if long_dp < 470 {
            Self::Small
        } else if long_dp >= 960 && short_dp >= 720 {
            Self::XLarge
        } else if long_dp >= 640 && short_dp >= 480 {
            Self::Large
        } else {
            Self::Normal
        }
    }

    pub const fn configuration_value(self) -> i32 {
        match self {
            Self::Small => 1,
            Self::Normal => 2,
            Self::Large => 3,
            Self::XLarge => 4,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Normal => "normal",
            Self::Large => "large",
            Self::XLarge => "xlarge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayModel {
    width_px: i32,
    height_px: i32,
    density_dpi: i32,
}

impl DisplayModel {
    #[must_use]
    pub fn new(width_px: u32, height_px: u32, monitor_scale: f64) -> Self {
        Self {
            width_px: pixels(width_px),
            height_px: pixels(height_px),
            density_dpi: density_for(monitor_scale),
        }
    }

    #[must_use]
    pub fn with_window(self, width_px: u32, height_px: u32) -> Self {
        Self {
            width_px: pixels(width_px),
            height_px: pixels(height_px),
            ..self
        }
    }

    #[must_use]
    pub const fn window_px(self) -> (i32, i32) {
        (self.width_px, self.height_px)
    }

    #[must_use]
    pub const fn density_dpi(self) -> i32 {
        self.density_dpi
    }

    #[must_use]
    pub fn width_dp(self) -> i32 {
        self.dp(self.width_px)
    }

    #[must_use]
    pub fn height_dp(self) -> i32 {
        self.dp(self.height_px)
    }

    #[must_use]
    pub fn smallest_width_dp(self) -> i32 {
        self.width_dp().min(self.height_dp())
    }

    #[must_use]
    pub const fn orientation(self) -> Orientation {
        if self.width_px > self.height_px {
            Orientation::Landscape
        } else {
            Orientation::Portrait
        }
    }

    #[must_use]
    pub fn size_class(self) -> SizeClass {
        SizeClass::of(self.longest_dp(), self.smallest_width_dp())
    }

    fn is_long(self) -> bool {
        self.longest_dp() >= 470 && self.longest_dp() * 3 / 5 >= self.smallest_width_dp() - 1
    }

    #[must_use]
    pub fn screen_layout(self) -> i32 {
        let long = if self.is_long() {
            SCREENLAYOUT_LONG_YES
        } else {
            SCREENLAYOUT_LONG_NO
        };
        self.size_class().configuration_value() | long
    }

    fn longest_dp(self) -> i32 {
        self.width_dp().max(self.height_dp())
    }

    fn dp(self, px: i32) -> i32 {
        let dp = i64::from(px) * i64::from(BASELINE_DPI) / i64::from(self.density_dpi);
        i32::try_from(dp).unwrap_or(i32::MAX)
    }
}

impl fmt::Display for DisplayModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let length = if self.is_long() { " long" } else { "" };
        write!(
            f,
            "{}x{} px, densityDpi {}, xdpi {}, {}x{} dp, {}, {}{length}",
            self.width_px,
            self.height_px,
            self.density_dpi,
            self.density_dpi,
            self.width_dp(),
            self.height_dp(),
            self.orientation().name(),
            self.size_class().name(),
        )
    }
}

fn pixels(px: u32) -> i32 {
    i32::try_from(px.max(1)).unwrap_or(i32::MAX)
}

fn density_for(monitor_scale: f64) -> i32 {
    (f64::from(BASELINE_DPI) * monitor_scale)
        .round()
        .max(f64::from(MIN_DPI))
        .min(f64::from(MAX_DPI)) as i32
}

#[must_use]
pub fn current() -> DisplayModel {
    APPLIED.get().copied().unwrap_or(WINDOWLESS)
}

pub fn apply(vm: &Vm, model: DisplayModel) -> Result<(), FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    APPLIED
        .set(model)
        .map_err(|_| FrameworkError::DisplayAlreadyApplied)?;
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            set_device_density(env, model.density_dpi)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })?;
    let (width, height) = model.window_px();
    publish_window_size(vm, width, height)?;
    tracing::info!("display: {model}");
    Ok(())
}

fn set_device_density(env: &mut Env, density_dpi: i32) -> Result<(), FrameworkError> {
    let class = checked(env, "find DisplayMetrics", |env| {
        env.find_class(DISPLAY_METRICS_CLASS)
    })?;
    checked(env, "DisplayMetrics.DENSITY_DEVICE", |env| {
        env.set_static_field(
            &class,
            DENSITY_DEVICE_FIELD,
            int_field_sig(),
            JValue::Int(density_dpi),
        )
    })
}

pub(super) fn write_configuration(env: &mut Env, config: &JObject) -> jni::errors::Result<()> {
    let model = current();
    let fields = [
        (jni_str!("densityDpi"), model.density_dpi()),
        (jni_str!("screenWidthDp"), model.width_dp()),
        (jni_str!("screenHeightDp"), model.height_dp()),
        (jni_str!("smallestScreenWidthDp"), model.smallest_width_dp()),
        (
            jni_str!("orientation"),
            model.orientation().configuration_value(),
        ),
        (jni_str!("screenLayout"), model.screen_layout()),
    ];
    for (field, value) in fields {
        env.set_field(config, field, int_field_sig(), JValue::Int(value))?;
    }
    Ok(())
}

pub fn note_game_window_scale(scale: f64) {
    let density_dpi = current().density_dpi;
    if density_for(scale) == density_dpi || SCALE_DIFFERENCE_NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::info!(
        scale,
        density_dpi,
        "the game window's scale differs from the launch window's; Roblox keeps densityDpi \
         {density_dpi} until it starts again"
    );
}

#[cfg(test)]
mod tests {
    use jni::objects::JClass;

    use super::super::fake_jvm;
    use super::*;

    #[test]
    fn scale_1_keeps_android_baseline_density() {
        let model = DisplayModel::new(1280, 720, 1.0);
        assert_eq!(model.density_dpi(), 160);
        assert_eq!((model.width_dp(), model.height_dp()), (1280, 720));
        assert_eq!(model.smallest_width_dp(), 720);
        assert_eq!(model.orientation(), Orientation::Landscape);
    }

    #[test]
    fn scale_2_doubles_density_and_keeps_the_logical_size_in_dp() {
        let model = DisplayModel::new(2560, 1440, 2.0);
        assert_eq!(model.density_dpi(), 320);
        assert_eq!((model.width_dp(), model.height_dp()), (1280, 720));
        assert_eq!(model.smallest_width_dp(), 720);
        assert_eq!(model.orientation(), Orientation::Landscape);
        assert_eq!(model.size_class(), SizeClass::XLarge);
        assert_eq!(model.screen_layout(), 0x24);
    }

    #[test]
    fn fractional_scales_round_to_whole_dpi() {
        assert_eq!(DisplayModel::new(1600, 900, 1.25).density_dpi(), 200);
        assert_eq!(DisplayModel::new(1920, 1080, 1.5).density_dpi(), 240);
        assert_eq!(DisplayModel::new(1280, 720, 4.0 / 3.0).density_dpi(), 213);
    }

    #[test]
    fn density_is_clamped_to_android_buckets() {
        assert_eq!(DisplayModel::new(3840, 2160, 3.0).density_dpi(), 480);
        assert_eq!(DisplayModel::new(3840, 2160, 5.0).density_dpi(), 640);
        assert_eq!(DisplayModel::new(640, 360, 0.5).density_dpi(), 120);
    }

    #[test]
    fn a_window_taller_than_wide_is_portrait() {
        assert_eq!(
            DisplayModel::new(720, 1280, 1.0).orientation(),
            Orientation::Portrait
        );
        assert_eq!(
            DisplayModel::new(800, 800, 1.0).orientation(),
            Orientation::Portrait
        );
    }

    #[test]
    fn size_classes_follow_aosp_thresholds() {
        let class = |width, height| DisplayModel::new(width, height, 1.0).size_class();
        assert_eq!(class(469, 320), SizeClass::Small);
        assert_eq!(class(470, 320), SizeClass::Normal);
        assert_eq!(class(639, 480), SizeClass::Normal);
        assert_eq!(class(640, 479), SizeClass::Normal);
        assert_eq!(class(640, 480), SizeClass::Large);
        assert_eq!(class(480, 640), SizeClass::Large);
        assert_eq!(class(959, 720), SizeClass::Large);
        assert_eq!(class(960, 719), SizeClass::Large);
        assert_eq!(class(960, 720), SizeClass::XLarge);
    }

    #[test]
    fn screen_layout_marks_screens_wider_than_5_to_3_as_long() {
        let layout = |width, height| DisplayModel::new(width, height, 1.0).screen_layout();
        assert_eq!(layout(854, 480), 0x23);
        assert_eq!(layout(800, 600), 0x13);
        assert_eq!(layout(400, 240), 0x11);
    }

    #[test]
    fn a_resized_window_keeps_the_density() {
        let model = DisplayModel::new(2560, 1440, 2.0).with_window(1000, 1600);
        assert_eq!(model.density_dpi(), 320);
        assert_eq!((model.width_dp(), model.height_dp()), (500, 800));
        assert_eq!(model.orientation(), Orientation::Portrait);
        assert_eq!(model.size_class(), SizeClass::Large);
    }

    #[test]
    fn empty_window_sizes_count_as_one_pixel() {
        assert_eq!(DisplayModel::new(0, 0, 1.0).window_px(), (1, 1));
    }

    #[test]
    fn the_log_line_names_every_reported_value() {
        assert_eq!(
            DisplayModel::new(2560, 1440, 2.0).to_string(),
            "2560x1440 px, densityDpi 320, xdpi 320, 1280x720 dp, landscape, xlarge long"
        );
        assert_eq!(
            DisplayModel::new(800, 600, 1.0).to_string(),
            "800x600 px, densityDpi 160, xdpi 160, 800x600 dp, landscape, large"
        );
    }

    #[test]
    fn without_a_window_the_screen_is_1280_by_720_dp_at_baseline_density() {
        assert_eq!(current(), DisplayModel::new(1280, 720, 1.0));
    }

    #[test]
    fn native_update_config_writes_the_whole_screen_into_the_configuration() {
        let written = fake_jvm::with_env(|env| {
            let config = fake_jvm::new_object(env);
            let passed = env.new_local_ref(&config).expect("a second reference");
            super::super::native_update_config(fake_jvm::native_env(), JClass::null(), passed);
            [
                "densityDpi",
                "screenWidthDp",
                "screenHeightDp",
                "smallestScreenWidthDp",
                "orientation",
                "screenLayout",
            ]
            .map(|field| fake_jvm::int_field(&config, field))
        });
        assert_eq!(
            written,
            [160, 1280, 720, 720, 2, 0x24].map(Some),
            "densityDpi, dp size, smallest width, landscape and xlarge long"
        );
    }
}
