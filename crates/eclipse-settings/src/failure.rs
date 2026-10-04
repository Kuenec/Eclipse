use std::path::Path;

use adw::prelude::*;
use gtk4::{self as gtk, gio, pango};

const ICON: &str = "io.github.kuenec.Eclipse";

const DEFAULT_WIDTH: i32 = 560;

const COPY: &str = "Copy Report";

const COPIED: &str = "Copied";

const PRIVACY: &str = "Copy Report copies the whole report for a bug report. It leaves out your \
     home folder, tokens, cookies and account IDs.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    CouldNotStart,
    Stopped,
}

impl Ending {
    const fn title(self) -> &'static str {
        match self {
            Self::CouldNotStart => "Eclipse — Roblox could not start",
            Self::Stopped => "Eclipse — Roblox stopped",
        }
    }
}

pub(crate) fn read(path: &Path) -> Result<String, String> {
    let report = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read the report {}: {error}", path.display()))?;
    if report.trim().is_empty() {
        return Err(format!("the report {} is empty", path.display()));
    }
    Ok(report)
}

pub(crate) fn open(
    app: &adw::Application,
    report: &str,
    path: &Path,
    ending: Ending,
) -> adw::ApplicationWindow {
    let summary = gtk::Label::builder()
        .label(first_paragraph(report))
        .selectable(true)
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .build();
    let saved = gtk::Label::builder()
        .label(saved_note(path))
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    let copy = gtk::Button::builder()
        .label(COPY)
        .css_classes(["suggested-action"])
        .build();
    let open_folder = gtk::Button::with_label("Open Log Folder");
    let close = gtk::Button::with_label("Close");
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&open_folder);
    buttons.append(&close);
    buttons.append(&copy);
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    body.append(&summary);
    body.append(&saved);
    body.append(&buttons);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&body));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&toasts));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(ending.title())
        .icon_name(ICON)
        .default_width(DEFAULT_WIDTH)
        .content(&view)
        .build();
    window.set_default_widget(Some(&copy));
    GtkWindowExt::set_focus(&window, Some(&copy));
    let escape = gtk::ShortcutController::new();
    escape.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    window.add_controller(escape);

    let report = report.to_owned();
    copy.connect_clicked(move |copy| {
        copy.clipboard().set_text(&report);
        copy.set_label(COPIED);
    });
    let folder = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let parent = window.clone();
    open_folder.connect_clicked(move |_| {
        let toasts = toasts.clone();
        folder.open_containing_folder(Some(&parent), gio::Cancellable::NONE, move |opened| {
            if let Err(error) = opened {
                toasts.add_toast(
                    adw::Toast::builder()
                        .title(format!("Cannot open the log folder: {}", error.message()))
                        .use_markup(false)
                        .build(),
                );
            }
        });
    });
    let closing = window.clone();
    close.connect_clicked(move |_| closing.close());

    window.present();
    window
}

fn first_paragraph(report: &str) -> &str {
    report
        .split_once("\n\n")
        .map_or(report, |(first, _)| first)
        .trim_end()
}

fn saved_note(path: &Path) -> String {
    format!(
        "{PRIVACY} It is also saved at {}, because the clipboard may empty once this window \
         closes.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use gtk4::glib;

    use super::*;
    use crate::headless;

    const RUN: &str = "eclipse-20261003T091434.289Z";

    const PROBLEM: &str = "Roblox is not installed. Start Eclipse to download it, or run \
        `flatpak run io.github.kuenec.Eclipse update`.";

    const SUMMARY: &str = "Outcome: cannot download Roblox: APKCombo did not answer\n\
        Problem: Roblox is not installed. Start Eclipse to download it, or run \
        `flatpak run io.github.kuenec.Eclipse update`.\n\
        Log: ~/app-data/logs/eclipse-20261003T091434.289Z.log";

    fn report() -> String {
        format!("{SUMMARY}\n\n```text\nEclipse\n  Version: 0.1.9\n\nLog excerpt\n```\n")
    }

    fn report_path(root: &Path) -> PathBuf {
        root.join("logs").join(format!("{RUN}.report.txt"))
    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut found = Vec::new();
        let mut child = widget.first_child();
        while let Some(widget) = child {
            found.extend(descendants(&widget));
            child = widget.next_sibling();
            found.push(widget);
        }
        found
    }

    fn labels(window: &adw::ApplicationWindow) -> Vec<String> {
        descendants(window.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
            .map(|label| label.label().into())
            .collect()
    }

    fn button(window: &adw::ApplicationWindow, label: &str) -> gtk::Button {
        descendants(window.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.label().as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no {label} button"))
    }

    #[test]
    fn the_failure_window_shows_the_outcome_and_copy_puts_the_whole_report_on_the_clipboard() {
        if let Some(root) = headless::child_root() {
            let path = report_path(&root);
            let report = read(&path).expect("read the report");
            let window = open(&headless::app(), &report, &path, Ending::CouldNotStart);
            headless::wait_until("the window is shown", || window.is_mapped());
            assert_eq!(
                window.title().as_deref(),
                Some("Eclipse — Roblox could not start")
            );
            let texts = labels(&window);
            assert!(texts.iter().any(|text| text == SUMMARY), "{texts:?}");
            assert!(texts.iter().any(|text| text.contains(PROBLEM)), "{texts:?}");
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains(&format!("saved at {},", path.display()))),
                "{texts:?}"
            );
            let copy = button(&window, COPY);
            let copy_widget = copy.clone().upcast::<gtk::Widget>();
            assert_eq!(window.default_widget(), Some(copy_widget.clone()));
            assert_eq!(GtkWindowExt::focus(&window), Some(copy_widget));
            WidgetExt::activate_action(&window, "default.activate", None)
                .expect("Enter's action exists");
            headless::wait_until("Copy confirms the copy", || {
                copy.label().as_deref() == Some(COPIED)
            });
            let copied = glib::MainContext::default()
                .block_on(copy.clipboard().read_text_future())
                .expect("read the clipboard");
            assert_eq!(copied.as_deref(), Some(report.as_str()));
            button(&window, "Close").emit_clicked();
            headless::wait_until("the window closes", || !window.is_visible());
            return;
        }
        let root = headless::root("failure-report");
        let path = report_path(&root);
        fs::create_dir_all(path.parent().expect("a log directory")).expect("mkdir");
        fs::write(&path, report()).expect("write the report");

        headless::run_child(
            "failure::tests::the_failure_window_shows_the_outcome_and_copy_puts_the_whole_report_on_the_clipboard",
            "failure-report",
            &root,
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_title_says_whether_roblox_started() {
        assert_eq!(
            Ending::CouldNotStart.title(),
            "Eclipse — Roblox could not start"
        );
        assert_eq!(Ending::Stopped.title(), "Eclipse — Roblox stopped");
    }

    #[test]
    fn the_summary_is_the_report_up_to_its_first_blank_line() {
        assert_eq!(first_paragraph(&report()), SUMMARY);
        assert_eq!(first_paragraph("Outcome: unknown\n"), "Outcome: unknown");
    }

    #[test]
    fn an_empty_or_missing_report_is_refused_with_its_path() {
        let dir = std::env::temp_dir().join("eclipse-settings-failure-read");
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("mkdir");
        let empty = dir.join("empty.report.txt");
        fs::write(&empty, " \n").expect("write the report");
        let missing = dir.join("missing.report.txt");

        let refused = [read(&empty), read(&missing)];

        fs::remove_dir_all(&dir).ok();
        assert_eq!(
            refused[0],
            Err(format!("the report {} is empty", empty.display()))
        );
        let Err(missing_error) = &refused[1] else {
            panic!("a missing report is refused");
        };
        assert!(
            missing_error.starts_with(&format!("cannot read the report {}: ", missing.display())),
            "{missing_error}"
        );
    }
}
