//! SettingsGoo — a desktop-environment-neutral settings app for Linux (X11 and Wayland).

mod bluez;
mod nm;
mod pages;
mod sys;

use gtk::prelude::*;
use gtk::{gdk, glib};

/// The program's display name. Code and filenames use lowercase `settingsgoo`.
pub const APP_NAME: &str = "SettingsGoo";
const APP_ID: &str = "com.aisloperator.settingsgoo";

const CSS: &str = "
.page-title { font-size: 1.6em; font-weight: bold; }
.group-title { font-weight: bold; margin-top: 6px; }
.settings-list row { padding: 4px 0; }
";

fn main() -> glib::ExitCode {
    glib::set_application_name(APP_NAME);
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|app| {
        // Ctrl+Q quits; Ctrl+W closes the window (which also quits, as there is only one).
        let quit = gtk::gio::ActionEntry::builder("quit")
            .activate(|app: &gtk::Application, _, _| app.quit())
            .build();
        app.add_action_entries([quit]);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("window.close", &["<Control>w"]);

        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &gtk::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .hexpand(true)
        .vexpand(true)
        .build();

    for page in pages::all() {
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&page.widget)
            .build();
        stack.add_titled(&scroller, Some(page.id), page.title);
    }

    let sidebar = gtk::StackSidebar::builder()
        .stack(&stack)
        .width_request(190)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar)
        .end_child(&stack)
        .shrink_start_child(false)
        .resize_start_child(false)
        .build();

    // No custom titlebar: GTK then uses the platform's normal decorations.
    // On X11 the window manager draws the titlebar; on Wayland GTK uses
    // server-side decorations if the compositor offers them, and otherwise
    // draws a plain titlebar itself (GTK_CSD=1 forces client-side ones).
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(APP_NAME)
        .default_width(920)
        .default_height(640)
        .child(&paned)
        .build();
    window.present();
}
