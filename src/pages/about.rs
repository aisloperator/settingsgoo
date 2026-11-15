//! About: device name, operating system and hardware summary.
//! The device names are managed by systemd-hostnamed (`org.freedesktop.hostname1`).

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::sys;

const SERVICE: &str = "org.freedesktop.hostname1";

fn cpu() -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let model = info.lines().find_map(|l| {
        l.strip_prefix("model name")
            .and_then(|r| r.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
    })?;
    let count = info.lines().filter(|l| l.starts_with("processor")).count();
    Some(if count > 1 {
        format!("{model} × {count}")
    } else {
        model
    })
}

fn memory() -> Option<String> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: f64 = info
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()?;
    Some(format!("{:.1} GiB", kb / 1024.0 / 1024.0))
}

pub fn session_type() -> String {
    let backend = gdk::Display::default()
        .map(|d| d.type_().name().to_string())
        .map(|n| {
            if n.contains("Wayland") {
                "Wayland".to_string()
            } else if n.contains("X11") {
                "X11".to_string()
            } else {
                n
            }
        });
    backend
        .or_else(|| std::env::var("XDG_SESSION_TYPE").ok())
        .unwrap_or_else(|| "Unknown".into())
}

/// An entry that sets a hostnamed property when Enter or "Apply" is pressed.
fn name_row(
    list: &gtk::ListBox,
    proxy: &gtk::gio::DBusProxy,
    title: &str,
    subtitle: &str,
    prop: &str,
    method: &'static str,
) {
    let entry = gtk::Entry::builder()
        .text(sys::prop_str(proxy, prop).unwrap_or_default())
        .width_chars(24)
        .build();
    let apply = gtk::Button::with_label("Apply");
    let hbox = gtk::Box::builder().spacing(6).build();
    hbox.append(&entry);
    hbox.append(&apply);

    let submit = {
        let (proxy, entry) = (proxy.clone(), entry.clone());
        move || {
            let (proxy, entry) = (proxy.clone(), entry.clone());
            glib::spawn_future_local(async move {
                let name = entry.text().trim().to_string();
                if let Err(e) = sys::call(&proxy, method, (name.as_str(), true).to_variant()).await
                {
                    sys::show_error(&entry, "Could not change the name", &e);
                }
            });
        }
    };
    let s = submit.clone();
    entry.connect_activate(move |_| s());
    apply.connect_clicked(move |_| submit());
    sys::row(list, title, Some(subtitle), &hbox);
}

pub fn build() -> gtk::Widget {
    let page = sys::page("About", "");
    let proxy = sys::system_proxy(SERVICE);
    let prop = |name: &str| {
        proxy
            .as_ref()
            .and_then(|p| sys::prop_str(p, name))
            .filter(|s| !s.is_empty())
    };

    let list = sys::group(&page, "Device");
    if let Some(p) = &proxy {
        name_row(
            &list,
            p,
            "Device name",
            "A friendly name for this computer",
            "PrettyHostname",
            "SetPrettyHostname",
        );
        name_row(
            &list,
            p,
            "Hostname",
            "The network name of this computer",
            "StaticHostname",
            "SetStaticHostname",
        );
    } else {
        let host = glib::host_name().to_string();
        sys::row(&list, "Hostname", None, &sys::value_label(&host));
    }
    let hardware = match (prop("HardwareVendor"), prop("HardwareModel")) {
        (Some(v), Some(m)) => Some(format!("{v} {m}")),
        (v, m) => v.or(m),
    };
    if let Some(h) = hardware {
        sys::row(&list, "Hardware", None, &sys::value_label(&h));
    }
    if let Some(c) = cpu() {
        sys::row(&list, "Processor", None, &sys::value_label(&c));
    }
    if let Some(m) = memory() {
        sys::row(&list, "Memory", None, &sys::value_label(&m));
    }

    let list = sys::group(&page, "Software");
    let os = prop("OperatingSystemPrettyName")
        .or_else(|| sys::read_env_file("/etc/os-release", "PRETTY_NAME"))
        .unwrap_or_else(|| "Linux".into());
    sys::row(&list, "Operating system", None, &sys::value_label(&os));
    let kernel = prop("KernelRelease")
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    sys::row(&list, "Kernel", None, &sys::value_label(&kernel));
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "Unknown".into());
    sys::row(&list, "Desktop", None, &sys::value_label(&desktop));
    sys::row(
        &list,
        "Windowing system",
        None,
        &sys::value_label(&session_type()),
    );
    let gtk_version = format!(
        "{}.{}.{}",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version()
    );
    sys::row(&list, "GTK", None, &sys::value_label(&gtk_version));
    sys::row(
        &list,
        crate::APP_NAME,
        Some("By AI Sloperator with Claude Code"),
        &sys::value_label(env!("CARGO_PKG_VERSION")),
    );

    page.upcast()
}
