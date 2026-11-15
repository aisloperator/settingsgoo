//! settingsgoo — a desktop-environment-neutral settings app for Linux (X11 and Wayland).

mod pages;
mod sys;

use gtk::prelude::*;
use gtk::{gdk, glib};

const APP_ID: &str = "com.aisloperator.settingsgoo";

const CSS: &str = "
.page-title { font-size: 1.6em; font-weight: bold; }
.group-title { font-weight: bold; margin-top: 6px; }
.settings-list row { padding: 4px 0; }
";

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
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

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Settings")
        .default_width(920)
        .default_height(640)
        .titlebar(&gtk::HeaderBar::new())
        .child(&paned)
        .build();
    window.present();
}
