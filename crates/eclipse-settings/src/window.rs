use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use eclipse_config::audio::{AudioDevice, DeviceList, Direction, SoundDevices};
use eclipse_config::edit::{self, Change, EditError};
use eclipse_config::{
    CloseOnLeave, Config, FrameRateLimit, GraphicsOptimizationMode, Loaded, Setting, TouchMode,
};
use gtk4::{self as gtk, gio, glib};

use crate::install::{self, Outcome};

const TITLE: &str = "Eclipse Settings";

const APPLIES_ON_START: &str = "Changes apply the next time Roblox starts.";

const ICON: &str = "io.github.kuenec.Eclipse";

const DEFAULT_SIZE: (i32, i32) = (640, 720);

const CLOSE: &str = "close";

const CANCEL: &str = "cancel";

const SHOW_LOCATION: &str = "show-location";

const ABOUT_ACTION: &str = "about";

const INSTALL_SUBTITLE: &str = "Roblox's APK files or an .apks, .xapk or .apkm bundle";

const CONTROLLERS_SUBTITLE: &str = "Play with game controllers";

const CONTROLLER_ACCESS: &str = "__controller-access";

const CONTROLLER_ACCESS_MISSING: i32 = 1;

const LOG_DIR: &str = "__log-dir";

const AUDIO_DEVICES: &str = "__audio-devices";

const AUDIO_DEVICES_WATCH: &str = "--watch";

const STDERR_LIMIT: usize = 64 * 1024;

const AUDIO_NOTE: &str = "Roblox's own audio settings show these choices as Default";

const SYSTEM_DEFAULT: &str = "System default";

const SUGGESTED_UNFOCUSED_FPS: u8 = 30;

const BUG_REPORT_SUBTITLE: &str = "Leaves out your home folder, tokens, cookies and account IDs";

const SERVER_LOCATION_NOTICE: &str = "Eclipse looks up where each Roblox server is with \
     ipinfo.io. ipinfo.io receives the server's address and sees your IP address.";

pub(crate) struct Settings {
    path: PathBuf,
    eclipse: PathBuf,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    rows: Rows,
    refreshing: Cell<bool>,
    installing: Cell<bool>,
    listing: RefCell<Listing>,
    monitor: gio::FileMonitor,
}

enum Listing {
    Waiting,
    Listed(SoundDevices),
    Failed(String),
}

struct Rows {
    install: adw::ActionRow,
    choose: gtk::Button,
    auto_update: adw::SwitchRow,
    pointer_input: adw::ComboRow,
    controllers: adw::SwitchRow,
    output: DeviceRow,
    microphone: DeviceRow,
    close_on_leave: adw::ComboRow,
    server_location: adw::SwitchRow,
    physical_cores: adw::SwitchRow,
    gamemode: adw::SwitchRow,
    opengl: adw::SwitchRow,
    unfocused_limit: adw::ExpanderRow,
    unfocused_fps: adw::SpinRow,
    bug_report: adw::ActionRow,
    log_folder: adw::ActionRow,
}

struct DeviceRow {
    row: adw::ComboRow,
    direction: Direction,
    choices: RefCell<Vec<AudioDevice>>,
}

#[derive(Debug, PartialEq, Eq)]
struct Choice {
    device: AudioDevice,
    label: String,
}

pub(crate) fn open(
    app: &adw::Application,
    path: PathBuf,
    eclipse: PathBuf,
) -> Result<Rc<Settings>, String> {
    let monitor = gio::File::for_path(&path)
        .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        .map_err(|error| format!("cannot watch {} for changes: {error}", path.display()))?;
    let rows = Rows::new();
    let banner = adw::Banner::builder().button_label("Details").build();
    let page = adw::PreferencesPage::builder()
        .description(APPLIES_ON_START)
        .build();
    page.add(&group(
        "Roblox",
        &[rows.install.upcast_ref(), rows.auto_update.upcast_ref()],
    ));
    page.add(&group(
        "Controls",
        &[
            rows.pointer_input.upcast_ref(),
            rows.controllers.upcast_ref(),
        ],
    ));
    let audio = group(
        "Audio",
        &[
            rows.output.row.upcast_ref(),
            rows.microphone.row.upcast_ref(),
        ],
    );
    audio.set_description(Some(AUDIO_NOTE));
    page.add(&audio);
    page.add(&group(
        "Experiences",
        &[
            rows.close_on_leave.upcast_ref(),
            rows.server_location.upcast_ref(),
        ],
    ));
    page.add(&group(
        "Performance",
        &[
            rows.physical_cores.upcast_ref(),
            rows.gamemode.upcast_ref(),
            rows.opengl.upcast_ref(),
            rows.unfocused_limit.upcast_ref(),
        ],
    ));
    page.add(&group(
        "Troubleshooting",
        &[rows.bug_report.upcast_ref(), rows.log_folder.upcast_ref()],
    ));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let menu = gio::Menu::new();
    menu.append(Some("About Eclipse"), Some(&format!("win.{ABOUT_ACTION}")));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build();
    let header = adw::HeaderBar::new();
    header.pack_end(&menu_button);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&banner);
    view.set_content(Some(&toasts));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(TITLE)
        .icon_name(ICON)
        .default_width(DEFAULT_SIZE.0)
        .default_height(DEFAULT_SIZE.1)
        .content(&view)
        .build();
    let settings = Rc::new(Settings {
        path,
        eclipse,
        window,
        toasts,
        banner,
        rows,
        refreshing: Cell::new(false),
        installing: Cell::new(false),
        listing: RefCell::new(Listing::Waiting),
        monitor,
    });
    settings.connect();
    settings.refresh();
    settings.show_controller_access();
    settings.watch_audio_devices();
    settings.window.present();
    Ok(settings)
}

impl Rows {
    fn new() -> Self {
        let choose = gtk::Button::builder()
            .label("Choose…")
            .valign(gtk::Align::Center)
            .build();
        let install = adw::ActionRow::builder()
            .title("Install from files")
            .subtitle(INSTALL_SUBTITLE)
            .use_markup(false)
            .activatable_widget(&choose)
            .build();
        install.add_suffix(&choose);
        let unfocused_fps = adw::SpinRow::builder()
            .title("Frames per second")
            .adjustment(&gtk::Adjustment::new(
                f64::from(SUGGESTED_UNFOCUSED_FPS),
                f64::from(FrameRateLimit::MIN),
                f64::from(FrameRateLimit::MAX),
                1.0,
                10.0,
                0.0,
            ))
            .build();
        let unfocused_limit = adw::ExpanderRow::builder()
            .title("Limit the frame rate in the background")
            .subtitle("While another window has focus")
            .show_enable_switch(true)
            .build();
        unfocused_limit.add_row(&unfocused_fps);
        Self {
            install,
            choose,
            auto_update: switch_row(
                "Update automatically",
                "Check for a new Roblox version when Eclipse starts, at most every 6 hours",
            ),
            pointer_input: combo_row("Pointer input", TouchMode::ALL.map(pointer_input_label)),
            controllers: adw::SwitchRow::builder()
                .title("Controllers")
                .subtitle(CONTROLLERS_SUBTITLE)
                .use_markup(false)
                .build(),
            output: DeviceRow::new("Output device", Direction::Output),
            microphone: DeviceRow::new("Microphone", Direction::Input),
            close_on_leave: combo_row(
                "Close Eclipse after leaving an experience",
                CloseOnLeave::ALL.map(close_on_leave_label),
            ),
            server_location: switch_row(
                "Show the server location",
                "In the window title and a notification when you join",
            ),
            physical_cores: switch_row(
                "Physical cores only",
                "One thread per CPU core, on CPUs with 8 or more cores and SMT",
            ),
            gamemode: switch_row(
                "GameMode",
                "Turn on GameMode while Roblox runs, if it is installed",
            ),
            opengl: switch_row(
                "Use OpenGL ES",
                "Instead of Vulkan; may stop repeated out-of-memory crashes",
            ),
            unfocused_limit,
            unfocused_fps,
            bug_report: action_row("Copy Bug Report", BUG_REPORT_SUBTITLE, "edit-copy-symbolic"),
            log_folder: action_row(
                "Open Log Folder",
                "eclipse.log is the newest launch",
                "folder-open-symbolic",
            ),
        }
    }

    fn show(&self, config: &Config, editable: bool, listing: &Listing) {
        let Config {
            graphics_optimization_mode,
            touch_mode,
            enable_gamemode,
            roblox_auto_update,
            close_on_leave,
            server_location_indicator_enabled,
            allow_gamepad_permission,
            audio_output_device,
            audio_input_device,
            fflags: _,
            webview_helper_path: _,
            vulkan_device: _,
            use_opengl,
            unfocused_fps_limit,
        } = config;
        self.auto_update.set_active(*roblox_auto_update);
        self.pointer_input
            .set_selected(position(&TouchMode::ALL, *touch_mode));
        self.close_on_leave
            .set_selected(position(&CloseOnLeave::ALL, *close_on_leave));
        self.server_location
            .set_active(*server_location_indicator_enabled);
        self.physical_cores
            .set_active(*graphics_optimization_mode == GraphicsOptimizationMode::Performance);
        self.gamemode.set_active(*enable_gamemode);
        self.controllers.set_active(*allow_gamepad_permission);
        self.opengl.set_active(*use_opengl);
        self.output.show(audio_output_device, listing);
        self.microphone.show(audio_input_device, listing);
        if let Some(limit) = unfocused_fps_limit {
            self.unfocused_fps.set_value(f64::from(limit.per_second()));
        }
        self.unfocused_limit
            .set_enable_expansion(unfocused_fps_limit.is_some());
        for row in self.settings() {
            row.set_sensitive(editable);
        }
    }

    fn settings(&self) -> [&gtk::Widget; 11] {
        [
            self.auto_update.upcast_ref(),
            self.pointer_input.upcast_ref(),
            self.controllers.upcast_ref(),
            self.output.row.upcast_ref(),
            self.microphone.row.upcast_ref(),
            self.close_on_leave.upcast_ref(),
            self.server_location.upcast_ref(),
            self.physical_cores.upcast_ref(),
            self.gamemode.upcast_ref(),
            self.opengl.upcast_ref(),
            self.unfocused_limit.upcast_ref(),
        ]
    }

    const fn device(&self, direction: Direction) -> &DeviceRow {
        match direction {
            Direction::Output => &self.output,
            Direction::Input => &self.microphone,
        }
    }

    fn unfocused_fps_limit(&self) -> Option<FrameRateLimit> {
        if !self.unfocused_limit.enables_expansion() {
            return None;
        }
        FrameRateLimit::new(self.unfocused_fps.value() as u8)
    }
}

impl DeviceRow {
    fn new(title: &str, direction: Direction) -> Self {
        Self {
            row: adw::ComboRow::builder()
                .title(title)
                .use_markup(false)
                .build(),
            direction,
            choices: RefCell::new(Vec::new()),
        }
    }

    fn show(&self, chosen: &AudioDevice, listing: &Listing) {
        let choices = device_choices(chosen, listing, self.direction);
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        self.row.set_model(Some(&gtk::StringList::new(&labels)));
        let selected = choices
            .iter()
            .position(|choice| choice.device == *chosen)
            .and_then(|index| u32::try_from(index).ok())
            .expect("the chosen device is one of the choices");
        self.row.set_selected(selected);
        let subtitle = match listing {
            Listing::Failed(reason) => format!("Eclipse cannot list the sound devices: {reason}"),
            Listing::Waiting | Listing::Listed(_) => String::new(),
        };
        self.row.set_subtitle(&subtitle);
        self.choices
            .replace(choices.into_iter().map(|choice| choice.device).collect());
    }

    fn chosen(&self) -> Option<AudioDevice> {
        let index = usize::try_from(self.row.selected()).ok()?;
        self.choices.borrow().get(index).cloned()
    }
}

impl Settings {
    fn connect(self: &Rc<Self>) {
        let rows = &self.rows;
        self.on_switch(&rows.auto_update, Setting::RobloxAutoUpdate);
        self.on_switch(&rows.gamemode, Setting::EnableGamemode);
        self.on_switch(&rows.controllers, Setting::AllowGamepadPermission);
        self.on_switch(&rows.opengl, Setting::UseOpengl);
        self.on_switch(&rows.physical_cores, |on| {
            Setting::GraphicsOptimizationMode(if on {
                GraphicsOptimizationMode::Performance
            } else {
                GraphicsOptimizationMode::Balanced
            })
        });
        let settings = Rc::clone(self);
        rows.unfocused_limit
            .connect_enable_expansion_notify(move |_| settings.change_unfocused_fps_limit());
        let settings = Rc::clone(self);
        rows.unfocused_fps
            .connect_value_notify(move |_| settings.change_unfocused_fps_limit());
        self.on_choice(&rows.pointer_input, &TouchMode::ALL, Setting::TouchMode);
        self.on_choice(
            &rows.close_on_leave,
            &CloseOnLeave::ALL,
            Setting::CloseOnLeave,
        );
        for direction in Direction::ALL {
            let settings = Rc::clone(self);
            rows.device(direction)
                .row
                .connect_selected_notify(move |_| {
                    if settings.refreshing.get() {
                        return;
                    }
                    if let Some(device) = settings.rows.device(direction).chosen() {
                        settings.change(Setting::audio_device(direction, device));
                    }
                });
        }

        let settings = Rc::clone(self);
        rows.server_location.connect_active_notify(move |row| {
            if settings.refreshing.get() {
                return;
            }
            if row.is_active() {
                let settings = Rc::clone(&settings);
                glib::idle_add_local_once(move || settings.confirm_server_location());
            } else {
                settings.change(Setting::ServerLocationIndicatorEnabled(false));
            }
        });

        let settings = Rc::clone(self);
        rows.choose
            .connect_clicked(move |_| settings.choose_files());

        let settings = Rc::clone(self);
        rows.bug_report
            .connect_activated(move |_| settings.copy_bug_report());

        let settings = Rc::clone(self);
        rows.log_folder
            .connect_activated(move |_| settings.open_log_folder());

        let settings = Rc::clone(self);
        self.banner
            .connect_button_clicked(move |_| settings.show_details());

        let settings = Rc::clone(self);
        self.monitor
            .connect_changed(move |_, _, _, _| settings.refresh());

        let settings = Rc::clone(self);
        self.window.connect_close_request(move |_| {
            if !settings.installing.get() {
                return glib::Propagation::Proceed;
            }
            settings
                .toasts
                .add_toast(adw::Toast::new("Settings closes after Roblox is installed"));
            glib::Propagation::Stop
        });

        let settings = Rc::clone(self);
        let about = gio::SimpleAction::new(ABOUT_ACTION, None);
        about.connect_activate(move |_, _| settings.show_about());
        self.window.add_action(&about);
    }

    fn on_switch(self: &Rc<Self>, row: &adw::SwitchRow, setting: fn(bool) -> Setting) {
        let settings = Rc::clone(self);
        row.connect_active_notify(move |row| settings.change(setting(row.is_active())));
    }

    fn on_choice<T: Copy + 'static>(
        self: &Rc<Self>,
        row: &adw::ComboRow,
        choices: &'static [T],
        setting: fn(T) -> Setting,
    ) {
        let settings = Rc::clone(self);
        row.connect_selected_notify(move |row| {
            let chosen = usize::try_from(row.selected())
                .ok()
                .and_then(|index| choices.get(index));
            if let Some(choice) = chosen {
                settings.change(setting(*choice));
            }
        });
    }

    fn change_unfocused_fps_limit(self: &Rc<Self>) {
        self.change(Setting::UnfocusedFpsLimit(self.rows.unfocused_fps_limit()));
    }

    fn change(self: &Rc<Self>, setting: Setting) {
        if self.refreshing.get() {
            return;
        }
        if let Err(error) = edit::apply(&self.path, &Change::Set(setting)) {
            self.report("Eclipse did not change the setting", &error.to_string());
        }
        let settings = Rc::clone(self);
        glib::idle_add_local_once(move || settings.refresh());
    }

    fn refresh(&self) {
        let loaded = eclipse_config::load_from(&self.path);
        let refusal = edit::check(&self.path).err();
        self.refreshing.set(true);
        self.rows
            .show(&loaded.config, refusal.is_none(), &self.listing.borrow());
        self.refreshing.set(false);
        match notice(&loaded, refusal.as_ref()) {
            Some(title) => {
                self.banner.set_title(&title);
                self.banner.set_revealed(true);
            }
            None => self.banner.set_revealed(false),
        }
    }

    fn show_controller_access(self: &Rc<Self>) {
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let subtitle = match controller_access(&settings.eclipse).await {
                Ok(ControllerAccess::Visible) => return,
                Ok(ControllerAccess::Missing(reason)) => format!("Unavailable: {reason}"),
                Err(error) => format!("Eclipse cannot check controller access: {error}"),
            };
            let row = &settings.rows.controllers;
            row.set_subtitle(&subtitle);
            row.set_subtitle_selectable(true);
        });
    }

    fn watch_audio_devices(self: &Rc<Self>) {
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let watched = watch_audio_devices(&settings.eclipse, |devices| {
                settings.listing.replace(Listing::Listed(devices));
                settings.refresh();
            })
            .await;
            if let Err(reason) = watched {
                settings.listing.replace(Listing::Failed(reason));
                settings.refresh();
            }
        });
    }

    fn show_details(&self) {
        let loaded = eclipse_config::load_from(&self.path);
        let refusal = edit::check(&self.path).err();
        self.report("config.json", &details(&loaded, refusal.as_ref()));
    }

    fn confirm_server_location(self: &Rc<Self>) {
        self.refresh();
        let dialog = adw::AlertDialog::new(
            Some("Show the server location?"),
            Some(SERVER_LOCATION_NOTICE),
        );
        dialog.add_responses(&[(CANCEL, "Cancel"), (SHOW_LOCATION, "Show Location")]);
        dialog.set_close_response(CANCEL);
        dialog.set_default_response(Some(CANCEL));
        let settings = Rc::clone(self);
        dialog.connect_response(None, move |_, response| {
            if response == SHOW_LOCATION {
                settings.change(Setting::ServerLocationIndicatorEnabled(true));
            }
        });
        dialog.present(Some(&self.window));
    }

    fn choose_files(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Roblox APK files"));
        for suffix in install::SUFFIXES {
            filter.add_suffix(suffix);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Install Roblox from Files")
            .filters(&filters)
            .default_filter(&filter)
            .modal(true)
            .build();
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let chosen = match dialog.open_multiple_future(Some(&settings.window)).await {
                Ok(chosen) => chosen,
                Err(error)
                    if error.matches(gtk::DialogError::Dismissed)
                        || error.matches(gtk::DialogError::Cancelled) =>
                {
                    return;
                }
                Err(error) => {
                    settings.report("Eclipse cannot open the files", error.message());
                    return;
                }
            };
            match local_paths(&chosen) {
                Ok(files) => settings.install(&files).await,
                Err(error) => settings.report("Eclipse cannot open the files", &error),
            }
        });
    }

    async fn install(&self, files: &[PathBuf]) {
        let row = &self.rows.install;
        row.set_sensitive(false);
        self.installing.set(true);
        let outcome = install::run(&self.eclipse, files, |line| row.set_subtitle(line)).await;
        self.installing.set(false);
        row.set_subtitle(INSTALL_SUBTITLE);
        row.set_sensitive(true);
        match outcome {
            Ok(Outcome::Installed) => self
                .toasts
                .add_toast(adw::Toast::new("Roblox is installed")),
            Ok(Outcome::Failed { output }) => {
                self.report("Roblox was not installed", &output.join("\n"));
            }
            Err(error) => self.report(
                "Roblox was not installed",
                &format!("cannot run {}: {}", self.eclipse.display(), error.message()),
            ),
        }
    }

    fn copy_bug_report(self: &Rc<Self>) {
        let row = &self.rows.bug_report;
        row.set_sensitive(false);
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let report = bug_report(&settings.eclipse).await;
            let row = &settings.rows.bug_report;
            row.set_sensitive(true);
            match report {
                Ok(report) => {
                    row.clipboard().set_text(&report);
                    settings.toasts.add_toast(adw::Toast::new(
                        "Bug report copied; paste it before closing Settings",
                    ));
                }
                Err(error) => settings.report("Eclipse could not write a bug report", &error),
            }
        });
    }

    fn open_log_folder(self: &Rc<Self>) {
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let opened = match log_dir(&settings.eclipse).await {
                Ok(dir) => gtk::FileLauncher::new(Some(&gio::File::for_path(dir)))
                    .launch_future(Some(&settings.window))
                    .await
                    .map_err(|error| error.message().to_owned()),
                Err(error) => Err(error),
            };
            if let Err(error) = opened {
                settings.toasts.add_toast(
                    adw::Toast::builder()
                        .title(format!("Cannot open the log folder: {error}"))
                        .use_markup(false)
                        .build(),
                );
            }
        });
    }

    fn show_about(self: &Rc<Self>) {
        let settings = Rc::clone(self);
        glib::spawn_future_local(async move {
            let repository = env!("CARGO_PKG_REPOSITORY");
            let dialog = adw::AboutDialog::builder()
                .application_name("Eclipse")
                .application_icon(ICON)
                .license_type(gtk::License::MitX11)
                .website(repository)
                .issue_url(format!("{repository}/issues"))
                .build();
            match eclipse_version(&settings.eclipse).await {
                Ok(version) => dialog.set_version(&version),
                Err(error) => settings.toasts.add_toast(
                    adw::Toast::builder()
                        .title(format!("Cannot read Eclipse's version: {error}"))
                        .use_markup(false)
                        .build(),
                ),
            }
            dialog.present(Some(&settings.window));
        });
    }

    fn report(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response(CLOSE, "Close");
        dialog.present(Some(&self.window));
    }
}

fn group(title: &str, rows: &[&gtk::Widget]) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    for row in rows {
        group.add(*row);
    }
    group
}

fn switch_row(title: &str, subtitle: &str) -> adw::SwitchRow {
    adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build()
}

fn action_row(title: &str, subtitle: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name(icon));
    row
}

fn combo_row<const N: usize>(title: &str, labels: [&str; N]) -> adw::ComboRow {
    adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&labels))
        .build()
}

const fn pointer_input_label(mode: TouchMode) -> &'static str {
    match mode {
        TouchMode::Off => "Mouse",
        TouchMode::On => "Touch with the phone interface",
        TouchMode::FakeOff => "Touch with the desktop interface",
    }
}

const fn close_on_leave_label(policy: CloseOnLeave) -> &'static str {
    match policy {
        CloseOnLeave::Never => "Never",
        CloseOnLeave::LinkLaunches => "When a link started it",
        CloseOnLeave::Always => "Always",
    }
}

fn device_choices(chosen: &AudioDevice, listing: &Listing, direction: Direction) -> Vec<Choice> {
    let listed = match listing {
        Listing::Listed(devices) => Some(devices.of(direction)),
        Listing::Waiting | Listing::Failed(_) => None,
    };
    let system_default = match listed.and_then(DeviceList::default_device) {
        Some(device) => format!("{SYSTEM_DEFAULT} ({})", device.description),
        None => SYSTEM_DEFAULT.to_owned(),
    };
    let mut choices = vec![Choice {
        device: AudioDevice::SystemDefault,
        label: system_default,
    }];
    choices.extend(listed.into_iter().flat_map(|list| {
        list.devices
            .iter()
            .filter(|device| {
                !device.monitor
                    || matches!(chosen, AudioDevice::Named(name) if *name == device.name)
            })
            .map(|device| Choice {
                device: AudioDevice::Named(device.name.clone()),
                label: device.description.clone(),
            })
    }));
    if let AudioDevice::Named(name) = chosen {
        if !choices.iter().any(|choice| choice.device == *chosen) {
            let label = match listed {
                Some(_) => format!("{name} (not connected)"),
                None => name.to_string(),
            };
            choices.push(Choice {
                device: chosen.clone(),
                label,
            });
        }
    }
    choices
}

fn position<T: PartialEq>(choices: &[T], value: T) -> u32 {
    choices
        .iter()
        .position(|choice| *choice == value)
        .and_then(|index| u32::try_from(index).ok())
        .expect("every variant is one of the choices")
}

fn notice(loaded: &Loaded, refusal: Option<&EditError>) -> Option<String> {
    let title = match refusal {
        Some(EditError::Link { .. } | EditError::ReadOnly { .. }) => {
            "config.json is managed outside Eclipse"
        }
        Some(EditError::Invalid(_)) => "Fix config.json by hand to change settings here",
        Some(EditError::ChangedWhileSaving { .. } | EditError::Io { .. }) => {
            "Eclipse cannot open config.json"
        }
        None => match loaded.problems.len() {
            0 => return None,
            1 => "config.json has 1 problem",
            count => return Some(format!("config.json has {count} problems")),
        },
    };
    Some(title.to_owned())
}

fn details(loaded: &Loaded, refusal: Option<&EditError>) -> String {
    let refusal = refusal
        .filter(|refusal| !matches!(refusal, EditError::Invalid(_)))
        .map(ToString::to_string);
    let problems = loaded.problems.iter().map(ToString::to_string);
    let paragraphs: Vec<String> = refusal
        .into_iter()
        .chain(problems)
        .chain(loaded.unused_keys_message())
        .collect();
    paragraphs.join("\n\n")
}

fn local_paths(chosen: &gio::ListModel) -> Result<Vec<PathBuf>, String> {
    chosen
        .iter::<gio::File>()
        .map(|file| {
            let file = file.map_err(|error| error.to_string())?;
            file.path()
                .ok_or_else(|| format!("{} is not a local file", file.uri()))
        })
        .collect()
}

enum ControllerAccess {
    Visible,
    Missing(String),
}

async fn controller_access(eclipse: &Path) -> Result<ControllerAccess, String> {
    let process = gio::Subprocess::newv(
        &[eclipse.as_os_str(), OsStr::new(CONTROLLER_ACCESS)],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    )
    .map_err(|error| format!("cannot run {}: {}", eclipse.display(), error.message()))?;
    let (output, _) = process
        .communicate_utf8_future(None)
        .await
        .map_err(|error| error.message().to_owned())?;
    let output = output.map(String::from).unwrap_or_default();
    let output = output.trim();
    if process.is_successful() {
        return Ok(ControllerAccess::Visible);
    }
    if process.has_exited() && process.exit_status() == CONTROLLER_ACCESS_MISSING {
        return Ok(ControllerAccess::Missing(output.to_owned()));
    }
    Err(format!(
        "{} {CONTROLLER_ACCESS} failed: {output}",
        eclipse.display()
    ))
}

async fn bug_report(eclipse: &Path) -> Result<String, String> {
    let process = gio::Subprocess::newv(
        &[
            eclipse.as_os_str(),
            OsStr::new("doctor"),
            OsStr::new("--report"),
        ],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|error| format!("cannot run {}: {}", eclipse.display(), error.message()))?;
    let (stdout, stderr) = process
        .communicate_utf8_future(None)
        .await
        .map_err(|error| error.message().to_owned())?;
    if process.is_successful() {
        return Ok(stdout.map(String::from).unwrap_or_default());
    }
    let stderr = stderr.map(String::from).unwrap_or_default();
    Err(format!(
        "{} doctor --report failed: {}",
        eclipse.display(),
        stderr.trim()
    ))
}

async fn log_dir(eclipse: &Path) -> Result<PathBuf, String> {
    let process = gio::Subprocess::newv(
        &[eclipse.as_os_str(), OsStr::new(LOG_DIR)],
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    )
    .map_err(|error| format!("cannot run {}: {}", eclipse.display(), error.message()))?;
    let (output, _) = process
        .communicate_future(None)
        .await
        .map_err(|error| error.message().to_owned())?;
    let output = output.map(|bytes| bytes.to_vec()).unwrap_or_default();
    let dir = output.strip_suffix(b"\n").unwrap_or(&output);
    if process.is_successful() && !dir.is_empty() {
        return Ok(PathBuf::from(OsStr::from_bytes(dir)));
    }
    Err(format!(
        "{} {LOG_DIR} failed: {}",
        eclipse.display(),
        String::from_utf8_lossy(dir).trim()
    ))
}

async fn watch_audio_devices(
    eclipse: &Path,
    mut listed: impl FnMut(SoundDevices),
) -> Result<(), String> {
    let process = gio::Subprocess::newv(
        &[
            eclipse.as_os_str(),
            OsStr::new(AUDIO_DEVICES),
            OsStr::new(AUDIO_DEVICES_WATCH),
        ],
        gio::SubprocessFlags::STDIN_PIPE
            | gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|error| format!("cannot run {}: {}", eclipse.display(), error.message()))?;
    let lines = gio::DataInputStream::new(
        &process
            .stdout_pipe()
            .expect("the device watch's stdout is piped at spawn"),
    );
    while let Some(line) = lines
        .read_line_utf8_future(glib::Priority::DEFAULT)
        .await
        .map_err(|error| error.message().to_owned())?
    {
        listed(SoundDevices::from_json(&line).map_err(|error| {
            format!(
                "{} {AUDIO_DEVICES} printed an unreadable list: {error}",
                eclipse.display()
            )
        })?);
    }
    process
        .wait_future()
        .await
        .map_err(|error| error.message().to_owned())?;
    if process.is_successful() {
        return Ok(());
    }
    let stderr = process
        .stderr_pipe()
        .expect("the device watch's stderr is piped at spawn")
        .read_bytes_future(STDERR_LIMIT, glib::Priority::DEFAULT)
        .await
        .map_err(|error| error.message().to_owned())?;
    Err(match String::from_utf8_lossy(&stderr).trim() {
        "" => format!("{} {AUDIO_DEVICES} failed", eclipse.display()),
        reason => reason.to_owned(),
    })
}

async fn eclipse_version(eclipse: &Path) -> Result<String, String> {
    let process = gio::Subprocess::newv(
        &[eclipse.as_os_str(), OsStr::new("--version")],
        gio::SubprocessFlags::STDOUT_PIPE,
    )
    .map_err(|error| format!("cannot run {}: {}", eclipse.display(), error.message()))?;
    let (stdout, _) = process
        .communicate_utf8_future(None)
        .await
        .map_err(|error| error.message().to_owned())?;
    let stdout = stdout.map(String::from).unwrap_or_default();
    stdout
        .trim()
        .strip_prefix("eclipse ")
        .filter(|_| process.is_successful())
        .map(str::to_owned)
        .ok_or_else(|| format!("{} --version printed {stdout:?}", eclipse.display()))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::io;
    use std::os::unix::fs::{symlink, PermissionsExt};

    use eclipse_config::{Position, Problem};

    use super::*;
    use crate::headless;
    use crate::stub_script;

    const FIXTURE: &str = r#"{
  "zeta_unknown": 1,
  "touch_mode": "fake-off",
  "allow_gamepad_permission": false,
  "alpha_unknown": {"b": 2, "a": 1},
  "fflags": {"FFlagGameBasicSettingsFramerateCap5": "True"}
}
"#;

    fn show(root: &Path) -> Rc<Settings> {
        let settings = open(
            &headless::app(),
            headless::config_path(root),
            headless::eclipse_path(root),
        )
        .expect("open the settings window");
        headless::wait_until("the window is shown", || settings.window.is_mapped());
        settings
    }

    fn alert(settings: &Settings) -> adw::AlertDialog {
        headless::wait_until("a dialog is shown", || {
            settings.window.visible_dialog().is_some()
        });
        settings
            .window
            .visible_dialog()
            .and_downcast()
            .expect("the dialog is an alert")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o7777
    }

    #[test]
    fn choosing_a_pointer_input_rewrites_only_touch_mode() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            assert!(!settings.banner.is_revealed());
            assert_eq!(
                rows.pointer_input.selected(),
                position(&TouchMode::ALL, TouchMode::FakeOff)
            );
            assert!(rows.auto_update.is_active());
            assert!(rows.gamemode.is_active());
            assert!(!rows.controllers.is_active());
            assert!(!rows.physical_cores.is_active());
            assert!(rows.settings().iter().all(|row| row.is_sensitive()));
            rows.pointer_input
                .set_selected(position(&TouchMode::ALL, TouchMode::On));
            return;
        }
        let root = headless::root("pointer-input");
        let config = headless::config_path(&root);
        fs::write(&config, FIXTURE).expect("write config.json");
        fs::set_permissions(&config, Permissions::from_mode(0o640)).expect("chmod");

        headless::run_child(
            "window::tests::choosing_a_pointer_input_rewrites_only_touch_mode",
            "pointer-input",
            &root,
        );

        let written = fs::read_to_string(&config).expect("read config.json");
        let written_mode = mode(&config);
        fs::remove_dir_all(&root).ok();
        assert_eq!(
            written,
            FIXTURE.replace(r#""touch_mode": "fake-off""#, r#""touch_mode": "on""#)
        );
        assert_eq!(written_mode, 0o640);
    }

    #[test]
    fn a_linked_config_locks_every_setting_and_says_why() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            assert!(settings
                .rows
                .settings()
                .iter()
                .all(|row| !row.is_sensitive()));
            assert!(settings.rows.install.is_sensitive());
            assert!(settings.banner.is_revealed());
            assert_eq!(
                settings.banner.title(),
                "config.json is managed outside Eclipse"
            );
            return;
        }
        let root = headless::root("linked");
        let target = root.join("dotfiles.json");
        fs::write(&target, FIXTURE).expect("write the link target");
        let config = headless::config_path(&root);
        symlink(&target, &config).expect("link config.json");

        headless::run_child(
            "window::tests::a_linked_config_locks_every_setting_and_says_why",
            "linked",
            &root,
        );

        let link = fs::read_link(&config).expect("config.json is still a link");
        let target_text = fs::read_to_string(&target).expect("read the link target");
        fs::remove_dir_all(&root).ok();
        assert_eq!(link, target);
        assert_eq!(target_text, FIXTURE);
    }

    #[test]
    fn problems_show_in_the_banner_and_the_settings_stay_editable() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            assert!(settings.banner.is_revealed());
            assert_eq!(settings.banner.title(), "config.json has 2 problems");
            assert_eq!(
                rows.pointer_input.selected(),
                position(&TouchMode::ALL, TouchMode::Off)
            );
            assert!(rows.settings().iter().all(|row| row.is_sensitive()));
            rows.pointer_input
                .set_selected(position(&TouchMode::ALL, TouchMode::On));
            headless::wait_until("the fixed problem leaves the banner", || {
                settings.banner.title() == "config.json has 1 problem"
            });
            return;
        }
        let root = headless::root("problems");
        let config = headless::config_path(&root);
        fs::write(&config, r#"{"touch_mode": 5, "enable_gamemode": "yes"}"#)
            .expect("write config.json");

        headless::run_child(
            "window::tests::problems_show_in_the_banner_and_the_settings_stay_editable",
            "problems",
            &root,
        );

        let written = fs::read_to_string(&config).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(
            written,
            "{\n  \"touch_mode\": \"on\",\n  \"enable_gamemode\": \"yes\"\n}\n"
        );
    }

    const WRITTEN_ELSEWHERE: &str = r#"{"touch_mode": "on", "x": [1,2]}"#;

    #[test]
    fn a_change_made_elsewhere_shows_without_being_written_back() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let config = headless::config_path(&root);
            let replacement = config.with_file_name("config.json.new");
            fs::write(&replacement, WRITTEN_ELSEWHERE).expect("write the replacement");
            fs::rename(&replacement, &config).expect("replace config.json");
            headless::wait_until("the new pointer input shows", || {
                settings.rows.pointer_input.selected() == position(&TouchMode::ALL, TouchMode::On)
            });
            headless::settle();
            return;
        }
        let root = headless::root("elsewhere");
        let config = headless::config_path(&root);
        fs::write(&config, FIXTURE).expect("write config.json");

        headless::run_child(
            "window::tests::a_change_made_elsewhere_shows_without_being_written_back",
            "elsewhere",
            &root,
        );

        let written = fs::read_to_string(&config).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, WRITTEN_ELSEWHERE);
    }

    #[test]
    fn the_server_location_is_saved_only_after_consent() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.server_location;
            row.set_active(true);
            let dialog = alert(&settings);
            assert!(!row.is_active());
            assert!(dialog.body().contains("ipinfo.io"), "{}", dialog.body());
            assert!(
                !headless::config_path(&root).exists(),
                "{:?}",
                fs::read_to_string(headless::config_path(&root))
            );
            dialog.emit_by_name::<()>("response", &[&SHOW_LOCATION]);
            headless::wait_until("the switch shows the saved value", || row.is_active());
            return;
        }
        let root = headless::root("server-location");

        headless::run_child(
            "window::tests::the_server_location_is_saved_only_after_consent",
            "server-location",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(
            written,
            "{\n  \"server_location_indicator_enabled\": true\n}\n"
        );
    }

    #[test]
    fn the_opengl_es_switch_writes_use_opengl() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.opengl;
            assert!(!row.is_active());
            row.set_active(true);
            headless::settle();
            assert!(row.is_active());
            return;
        }
        let root = headless::root("opengl");

        headless::run_child(
            "window::tests::the_opengl_es_switch_writes_use_opengl",
            "opengl",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, "{\n  \"use_opengl\": true\n}\n");
    }

    #[test]
    fn the_background_frame_rate_row_writes_unfocused_fps_limit() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            assert!(rows.unfocused_limit.enables_expansion());
            assert_eq!(rows.unfocused_fps.value(), 60.0);
            rows.unfocused_fps.set_value(45.0);
            headless::settle();
            assert_eq!(
                fs::read_to_string(headless::config_path(&root)).expect("read config.json"),
                "{\n  \"unfocused_fps_limit\": 45\n}\n"
            );
            rows.unfocused_limit.set_enable_expansion(false);
            headless::settle();
            assert!(!rows.unfocused_limit.enables_expansion());
            return;
        }
        let root = headless::root("unfocused-limit");
        fs::write(
            headless::config_path(&root),
            "{\n  \"unfocused_fps_limit\": 60\n}\n",
        )
        .expect("write config.json");

        headless::run_child(
            "window::tests::the_background_frame_rate_row_writes_unfocused_fps_limit",
            "unfocused-limit",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, "{\n  \"unfocused_fps_limit\": null\n}\n");
    }

    #[test]
    fn a_refused_change_is_reported_once_and_the_row_shows_the_file_again() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.physical_cores;
            assert!(!row.is_active());
            assert!(row.is_sensitive());
            let config = headless::config_path(&root);
            fs::set_permissions(&config, Permissions::from_mode(0o444)).expect("chmod");
            row.set_active(true);
            let dialog = alert(&settings);
            assert_eq!(
                dialog.heading().as_deref(),
                Some("Eclipse did not change the setting")
            );
            headless::wait_until("the row shows the file again", || {
                !row.is_active() && !row.is_sensitive()
            });
            headless::settle();
            assert_eq!(settings.window.dialogs().n_items(), 1);
            return;
        }
        let root = headless::root("refused");
        let config = headless::config_path(&root);
        let quality = "{\"graphics_optimization_mode\": \"quality\"}";
        fs::write(&config, quality).expect("write config.json");

        headless::run_child(
            "window::tests::a_refused_change_is_reported_once_and_the_row_shows_the_file_again",
            "refused",
            &root,
        );

        let written = fs::read_to_string(&config).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, quality);
    }

    const FAILING_INSTALL: &str = "#!/bin/sh\n\
        case \"$1\" in __controller-access | __audio-devices) exit 0 ;; esac\n\
        printf '%s\\n' \"$@\" > \"${0%/*}/arguments\"\n\
        echo '# Verifying and installing the Roblox client…'\n\
        echo 'not a Roblox APK' >&2\n\
        while [ ! -e \"${0%/*}/release\" ]; do sleep 0.05; done\n\
        exit 1\n";

    #[test]
    fn a_failed_install_shows_its_output_and_frees_the_row() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.install;
            let files = [root.join("base one.apk"), root.join("split two.apk")];
            let installing = Rc::clone(&settings);
            glib::spawn_future_local(async move { installing.install(&files).await });
            headless::wait_until("the second line shows", || {
                row.subtitle().as_deref() == Some("not a Roblox APK")
            });
            assert!(!row.is_sensitive());
            settings.window.close();
            headless::settle();
            assert!(settings.window.is_visible());
            fs::write(root.join("bin").join("release"), "").expect("release the stub");
            let dialog = alert(&settings);
            assert!(row.is_sensitive());
            assert_eq!(row.subtitle().as_deref(), Some(INSTALL_SUBTITLE));
            assert_eq!(
                dialog.heading().as_deref(),
                Some("Roblox was not installed")
            );
            assert_eq!(
                dialog.body(),
                "Verifying and installing the Roblox client…\nnot a Roblox APK"
            );
            return;
        }
        let root = headless::root("install");
        stub(&root, FAILING_INSTALL);

        headless::run_child(
            "window::tests::a_failed_install_shows_its_output_and_frees_the_row",
            "install",
            &root,
        );

        let arguments = fs::read_to_string(root.join("bin").join("arguments"));
        fs::remove_dir_all(&root).ok();
        assert_eq!(
            arguments.expect("the stub ran"),
            format!(
                "install\n{}\n{}\n",
                root.join("base one.apk").display(),
                root.join("split two.apk").display()
            )
        );
    }

    const NO_INPUT_DEVICES: &str = "/dev/input is not visible in the sandbox because an override \
        removed it; to allow controllers, run `flatpak override --user --device=input \
        io.github.kuenec.Eclipse`";

    #[test]
    fn the_controllers_row_says_why_controllers_cannot_work() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.controllers;
            headless::wait_until("the row shows Eclipse's answer", || {
                row.subtitle().as_deref() != Some(CONTROLLERS_SUBTITLE)
            });
            assert_eq!(
                row.subtitle().as_deref(),
                Some(format!("Unavailable: {NO_INPUT_DEVICES}").as_str())
            );
            assert!(row.is_subtitle_selectable());
            assert!(row.is_active() && row.is_sensitive());
            return;
        }
        let root = headless::root("controller-access");
        stub(
            &root,
            &format!(
                "#!/bin/sh\n[ \"$1\" = __controller-access ] || exit 2\n\
                 echo '{NO_INPUT_DEVICES}'\nexit 1\n"
            ),
        );

        headless::run_child(
            "window::tests::the_controllers_row_says_why_controllers_cannot_work",
            "controller-access",
            &root,
        );

        fs::remove_dir_all(&root).ok();
    }

    const DEVICES: &str = r#"{
  "outputs": {
    "devices": [
      {"name": "speakers", "description": "Speakers", "monitor": false},
      {"name": "headset", "description": "Headset", "monitor": false}
    ],
    "system_default": "headset"
  },
  "inputs": {
    "devices": [
      {"name": "headset.monitor", "description": "Monitor of Headset", "monitor": true},
      {"name": "headset_mic", "description": "Headset Microphone", "monitor": false}
    ],
    "system_default": null
  }
}"#;

    const REPLUGGED: &str = r#"{"outputs": {"devices": [
      {"name": "speakers", "description": "Speakers", "monitor": false},
      {"name": "usb_dac", "description": "USB DAC", "monitor": false}
    ], "system_default": "speakers"},
    "inputs": {"devices": [], "system_default": null}}"#;

    const UNPLUGGED: &str = "{\n  \"audio_output_device\": \"unplugged_dac\"\n}\n";

    const HEADSET_CHOSEN: &str = "{\n  \"audio_output_device\": \"headset\"\n}\n";

    fn one_line(json: &str) -> String {
        json.replace('\n', " ")
    }

    fn labels(row: &adw::ComboRow) -> Vec<String> {
        let model = row
            .model()
            .and_downcast::<gtk::StringList>()
            .expect("a string list");
        (0..model.n_items())
            .filter_map(|index| model.string(index))
            .map(String::from)
            .collect()
    }

    fn stub(root: &Path, script: &str) {
        let eclipse = headless::eclipse_path(root);
        fs::create_dir_all(eclipse.parent().expect("a bin directory")).expect("mkdir");
        stub_script::write(&eclipse, script);
    }

    #[test]
    fn choosing_a_microphone_rewrites_only_audio_input_device() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            headless::wait_until("the devices are listed", || {
                labels(&rows.microphone.row).len() > 1
            });
            assert_eq!(
                labels(&rows.output.row),
                [
                    "System default (Headset)",
                    "Speakers",
                    "Headset",
                    "unplugged_dac (not connected)"
                ]
            );
            assert_eq!(rows.output.row.selected(), 3);
            assert_eq!(
                labels(&rows.microphone.row),
                ["System default", "Headset Microphone"]
            );
            assert_eq!(rows.microphone.row.selected(), 0);
            assert_eq!(rows.output.row.subtitle().as_deref(), Some(""));
            rows.microphone.row.set_selected(1);
            return;
        }
        let root = headless::root("microphone");
        fs::write(headless::config_path(&root), UNPLUGGED).expect("write config.json");
        stub(
            &root,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n__audio-devices) printf '%s\\n' '{}' ;;\n\
                 __controller-access) exit 0 ;;\n*) exit 2 ;;\nesac\n",
                one_line(DEVICES)
            ),
        );

        headless::run_child(
            "window::tests::choosing_a_microphone_rewrites_only_audio_input_device",
            "microphone",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(
            written,
            "{\n  \"audio_output_device\": \"unplugged_dac\",\n  \
             \"audio_input_device\": \"headset_mic\"\n}\n"
        );
    }

    #[test]
    fn devices_plugged_in_or_out_while_settings_is_open_update_the_rows() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            headless::wait_until("the second list is shown", || {
                labels(&rows.output.row).contains(&"USB DAC".to_owned())
            });
            assert_eq!(
                labels(&rows.output.row),
                [
                    "System default (Speakers)",
                    "Speakers",
                    "USB DAC",
                    "headset (not connected)"
                ]
            );
            assert_eq!(rows.output.row.selected(), 3);
            return;
        }
        let root = headless::root("replugged");
        fs::write(headless::config_path(&root), HEADSET_CHOSEN).expect("write config.json");
        stub(
            &root,
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n__audio-devices) [ \"$2\" = --watch ] || exit 2\n\
                 printf '%s\\n' '{}'\nsleep 0.3\nprintf '%s\\n' '{}'\ncat >/dev/null ;;\n\
                 __controller-access) exit 0 ;;\n*) exit 2 ;;\nesac\n",
                one_line(DEVICES),
                one_line(REPLUGGED)
            ),
        );

        headless::run_child(
            "window::tests::devices_plugged_in_or_out_while_settings_is_open_update_the_rows",
            "replugged",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, HEADSET_CHOSEN);
    }

    #[test]
    fn without_a_device_list_the_rows_say_why_and_keep_the_choice() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let rows = &settings.rows;
            headless::wait_until("the rows say why", || {
                rows.output
                    .row
                    .subtitle()
                    .is_some_and(|subtitle| !subtitle.is_empty())
            });
            assert_eq!(
                rows.output.row.subtitle().as_deref(),
                Some(
                    "Eclipse cannot list the sound devices: `pactl info` failed (exit status: \
                     1): Connection refused"
                )
            );
            assert_eq!(
                labels(&rows.output.row),
                ["System default", "unplugged_dac"]
            );
            assert_eq!(rows.output.row.selected(), 1);
            assert_eq!(labels(&rows.microphone.row), ["System default"]);
            assert!(rows.settings().iter().all(|row| row.is_sensitive()));
            return;
        }
        let root = headless::root("no-devices");
        fs::write(headless::config_path(&root), UNPLUGGED).expect("write config.json");
        stub(
            &root,
            "#!/bin/sh\ncase \"$1\" in\n__audio-devices) echo '`pactl info` failed (exit status: 1): \
             Connection refused' >&2; exit 1 ;;\n__controller-access) exit 0 ;;\n*) exit 2 ;;\nesac\n",
        );

        headless::run_child(
            "window::tests::without_a_device_list_the_rows_say_why_and_keep_the_choice",
            "no-devices",
            &root,
        );

        let written = fs::read_to_string(headless::config_path(&root)).expect("read config.json");
        fs::remove_dir_all(&root).ok();
        assert_eq!(written, UNPLUGGED);
    }

    #[test]
    fn a_chosen_device_is_named_plainly_until_the_list_says_it_is_missing() {
        let chosen = AudioDevice::Named(
            eclipse_config::audio::DeviceName::parse("unplugged_dac").expect("a device name"),
        );
        let labels = |listing: &Listing| -> Vec<String> {
            device_choices(&chosen, listing, Direction::Output)
                .into_iter()
                .map(|choice| choice.label)
                .collect()
        };
        assert_eq!(
            labels(&Listing::Waiting),
            ["System default", "unplugged_dac"]
        );
        assert_eq!(
            labels(&Listing::Listed(SoundDevices::default())),
            ["System default", "unplugged_dac (not connected)"]
        );
        let listed = SoundDevices::from_json(DEVICES).expect("a device list");
        assert_eq!(
            device_choices(
                &AudioDevice::SystemDefault,
                &Listing::Listed(listed),
                Direction::Input
            )
            .into_iter()
            .map(|choice| choice.device)
            .collect::<Vec<_>>(),
            [
                AudioDevice::SystemDefault,
                AudioDevice::Named(
                    eclipse_config::audio::DeviceName::parse("headset_mic").expect("a name")
                ),
            ]
        );
    }

    #[test]
    fn monitor_sources_are_offered_only_when_already_chosen() {
        let monitor = eclipse_config::audio::DeviceName::parse("headset.monitor").expect("a name");
        let listing = Listing::Listed(SoundDevices::from_json(DEVICES).expect("a device list"));
        let labels: Vec<String> =
            device_choices(&AudioDevice::Named(monitor), &listing, Direction::Input)
                .into_iter()
                .map(|choice| choice.label)
                .collect();
        assert_eq!(
            labels,
            ["System default", "Monitor of Headset", "Headset Microphone"]
        );
    }

    const REPORT: &str = "Outcome: no launch is logged yet\nLog: none in ~/app-data/logs\n";

    #[test]
    fn copy_bug_report_puts_what_doctor_prints_on_the_clipboard() {
        if let Some(root) = headless::child_root() {
            let settings = show(&root);
            let row = &settings.rows.bug_report;
            ActionRowExt::activate(row);
            assert!(!row.is_sensitive());
            headless::wait_until("the report is copied", || row.is_sensitive());
            let copied = glib::MainContext::default()
                .block_on(row.clipboard().read_text_future())
                .expect("read the clipboard");
            assert_eq!(copied.as_deref(), Some(REPORT));
            assert!(settings.window.visible_dialog().is_none());
            return;
        }
        let root = headless::root("bug-report");
        stub(
            &root,
            &format!(
                "#!/bin/sh\ncase \"$1\" in __controller-access | __audio-devices) exit 0 ;; esac\n\
                 printf '%s\\n' \"$@\" > \"${{0%/*}}/arguments\"\nprintf '{}'\n",
                REPORT.replace('\n', "\\n")
            ),
        );

        headless::run_child(
            "window::tests::copy_bug_report_puts_what_doctor_prints_on_the_clipboard",
            "bug-report",
            &root,
        );

        let arguments = fs::read_to_string(root.join("bin").join("arguments"));
        fs::remove_dir_all(&root).ok();
        assert_eq!(arguments.expect("the stub ran"), "doctor\n--report\n");
    }

    fn with_stub<T>(tag: &str, script: &str, ask: impl AsyncFnOnce(&Path) -> T) -> (PathBuf, T) {
        let dir = std::env::temp_dir().join(format!("eclipse-settings-stub-{tag}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create the stub directory");
        let eclipse = dir.join("eclipse");
        stub_script::write(&eclipse, script);
        let answer = glib::MainContext::new().block_on(ask(&eclipse));
        fs::remove_dir_all(&dir).ok();
        (eclipse, answer)
    }

    #[test]
    fn the_version_comes_from_eclipse_itself() {
        let (_, version) = with_stub(
            "version-ok",
            "#!/bin/sh\necho 'eclipse 0.1.8'\n",
            eclipse_version,
        );
        assert_eq!(version.as_deref(), Ok("0.1.8"));

        let (eclipse, version) = with_stub(
            "version-failed",
            "#!/bin/sh\necho 'eclipse 0.1.8'\nexit 1\n",
            eclipse_version,
        );
        assert_eq!(
            version,
            Err(format!(
                "{} --version printed \"eclipse 0.1.8\\n\"",
                eclipse.display()
            ))
        );
    }

    #[test]
    fn the_log_folder_is_the_one_eclipse_names() {
        let (_, dir) = with_stub(
            "log-dir-ok",
            "#!/bin/sh\n[ \"$1\" = __log-dir ] || exit 2\nprintf '/data/odd \\351/logs\\n'\n",
            log_dir,
        );
        assert_eq!(
            dir,
            Ok(PathBuf::from(OsStr::from_bytes(b"/data/odd \xe9/logs")))
        );

        let (eclipse, dir) = with_stub(
            "log-dir-failed",
            "#!/bin/sh\necho 'cannot resolve the app-data directory' >&2\nexit 1\n",
            log_dir,
        );
        assert_eq!(
            dir,
            Err(format!(
                "{} __log-dir failed: cannot resolve the app-data directory",
                eclipse.display()
            ))
        );
    }

    fn loaded(problems: Vec<Problem>, unused_keys: &[&str]) -> Loaded {
        Loaded {
            path: Some(PathBuf::from("/c/config.json")),
            config: Config::default(),
            problems,
            unused_keys: unused_keys.iter().map(|key| (*key).to_owned()).collect(),
        }
    }

    fn invalid_touch_mode() -> Problem {
        Problem::InvalidValue {
            path: PathBuf::from("/c/config.json"),
            key: "touch_mode".to_owned(),
            at: Position {
                line: 2,
                column: 17,
            },
            reason: "expected one of `off`, `on`, `fake-off`".to_owned(),
        }
    }

    #[test]
    fn the_banner_says_why_settings_are_locked_or_counts_the_problems() {
        let link = EditError::Link {
            path: PathBuf::from("/c/config.json"),
            target: PathBuf::from("/nix/store/x-hm/config.json"),
        };
        let syntax = EditError::Invalid(Problem::NotAnObject {
            path: PathBuf::from("/c/config.json"),
        });
        let io = EditError::Io {
            action: "read",
            path: PathBuf::from("/c/config.json"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let clean = loaded(Vec::new(), &["use_opengl"]);
        let one = loaded(vec![invalid_touch_mode()], &[]);
        let two = loaded(vec![invalid_touch_mode(), invalid_touch_mode()], &[]);

        assert_eq!(notice(&clean, None), None);
        assert_eq!(
            notice(&one, None).as_deref(),
            Some("config.json has 1 problem")
        );
        assert_eq!(
            notice(&two, None).as_deref(),
            Some("config.json has 2 problems")
        );
        assert_eq!(
            notice(&clean, Some(&link)).as_deref(),
            Some("config.json is managed outside Eclipse")
        );
        assert_eq!(
            notice(&one, Some(&syntax)).as_deref(),
            Some("Fix config.json by hand to change settings here")
        );
        assert_eq!(
            notice(&clean, Some(&io)).as_deref(),
            Some("Eclipse cannot open config.json")
        );
    }

    #[test]
    fn details_list_the_refusal_each_problem_and_the_unused_keys_once() {
        let link = EditError::Link {
            path: PathBuf::from("/c/config.json"),
            target: PathBuf::from("/nix/store/x-hm/config.json"),
        };
        let with_unused = loaded(vec![invalid_touch_mode()], &["use_opengl"]);
        assert_eq!(
            details(&with_unused, Some(&link)),
            [
                link.to_string(),
                invalid_touch_mode().to_string(),
                with_unused.unused_keys_message().expect("unused keys"),
            ]
            .join("\n\n")
        );

        let unreadable = Problem::NotAnObject {
            path: PathBuf::from("/c/config.json"),
        };
        let invalid = EditError::Invalid(unreadable.clone());
        assert_eq!(
            details(&loaded(vec![unreadable.clone()], &[]), Some(&invalid)),
            unreadable.to_string()
        );
    }
}
