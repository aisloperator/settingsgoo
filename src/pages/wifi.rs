//! Wi-Fi: turn the radio on and off, see nearby networks, connect,
//! disconnect and forget networks, via NetworkManager.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use crate::nm::{self, Network, WifiDevice};
use crate::sys::{self, Toggle};

fn signal_icon(bars: u8) -> &'static str {
    match bars {
        0 | 1 => "network-wireless-signal-weak-symbolic",
        2 => "network-wireless-signal-ok-symbolic",
        3 => "network-wireless-signal-good-symbolic",
        _ => "network-wireless-signal-excellent-symbolic",
    }
}

/// Runs a NetworkManager change and shows any error.
fn spawn(
    widget: &impl IsA<gtk::Widget>,
    what: &'static str,
    fut: impl Future<Output = Result<(), String>> + 'static,
) {
    let widget = widget.clone().upcast::<gtk::Widget>();
    glib::spawn_future_local(async move {
        if let Err(e) = fut.await {
            sys::show_error(&widget, what, &e);
        }
    });
}

/// A small modal window asking for a network's password.
fn ask_password(parent: &impl IsA<gtk::Widget>, ssid: &str, on_ok: impl Fn(String) + 'static) {
    let window = gtk::Window::builder()
        .title("Wi-Fi Password")
        .modal(true)
        .resizable(false)
        .default_width(380)
        .build();
    if let Some(root) = parent.root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&root));
    }

    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let connect = gtk::Button::builder()
        .label("Connect")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();

    let buttons = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&connect);
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let prompt = gtk::Label::builder()
        .label(format!("Enter the password for “{ssid}”."))
        .xalign(0.0)
        .wrap(true)
        .build();
    vbox.append(&prompt);
    vbox.append(&entry);
    vbox.append(&buttons);
    window.set_child(Some(&vbox));
    window.set_default_widget(Some(&connect));

    // Escape cancels.
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    window.add_controller(keys);

    {
        let connect = connect.clone();
        entry.connect_changed(move |e| connect.set_sensitive(nm::valid_psk(&e.text())));
    }
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let (window, entry) = (window.clone(), entry.clone());
        connect.connect_clicked(move |_| {
            on_ok(entry.text().to_string());
            window.close();
        });
    }
    window.present();
    entry.grab_focus();
}

fn connect(widget: &gtk::Button, dev: String, net: Network) {
    if net.saved.is_none() && net.security.needs_password() {
        let (w, ssid) = (widget.clone(), net.ssid.clone());
        ask_password(widget, &ssid, move |pw| {
            let (dev, net) = (dev.clone(), net.clone());
            spawn(&w, "Could not connect", async move {
                nm::connect_wifi(&dev, &net, Some(&pw)).await
            });
        });
    } else {
        spawn(widget, "Could not connect", async move {
            nm::connect_wifi(&dev, &net, None).await
        });
    }
}

fn forget(widget: &gtk::Button, net: Network) {
    let Some(conn) = net.saved.clone() else {
        return;
    };
    let parent = widget.root().and_downcast::<gtk::Window>();
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Forget “{}”?", net.ssid))
        .detail("Its password and settings will be removed. You can connect again later.")
        .buttons(["Cancel", "Forget"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let widget = widget.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(parent.as_ref()).await == Ok(1)
            && let Err(e) = nm::forget(&conn).await
        {
            sys::show_error(&widget, "Could not forget the network", &e);
        }
    });
}

fn network_row(list: &gtk::ListBox, dev: &str, net: &Network) {
    let mut subtitle = vec![];
    if net.active {
        subtitle.push("Connected");
    } else if net.saved.is_some() {
        subtitle.push("Saved");
    }
    subtitle.push(net.security.name());
    let subtitle = subtitle.join(" · ");

    let buttons = gtk::Box::builder().spacing(6).build();
    if net.saved.is_some() {
        let b = gtk::Button::with_label("Forget");
        let n = net.clone();
        b.connect_clicked(move |b| forget(b, n.clone()));
        buttons.append(&b);
    }
    if net.active {
        let b = gtk::Button::with_label("Disconnect");
        let d = dev.to_string();
        b.connect_clicked(move |b| {
            let d = d.clone();
            spawn(b, "Could not disconnect", async move {
                nm::disconnect_device(&d).await
            });
        });
        buttons.append(&b);
    } else {
        let b = gtk::Button::with_label("Connect");
        let can = net.saved.is_some() || net.security.supported();
        b.set_sensitive(can);
        if !can {
            b.set_tooltip_text(Some(&format!(
                "{} networks are not supported yet",
                net.security.name()
            )));
        }
        let (d, n) = (dev.to_string(), net.clone());
        b.connect_clicked(move |b| connect(b, d.clone(), n.clone()));
        buttons.append(&b);
    }
    sys::row_with_icon(
        list,
        Some(signal_icon(net.bars)),
        &net.ssid,
        Some(&subtitle),
        &buttons,
    );
}

fn render(container: &gtk::Box, devices: &[WifiDevice], enabled: bool) {
    sys::clear(container);
    if devices.is_empty() {
        container.append(&sys::dim_label("No Wi-Fi adapter was found."));
        return;
    }
    if !enabled {
        container.append(&sys::dim_label("Wi-Fi is turned off."));
        return;
    }
    for wd in devices {
        let dev = &wd.device;
        let suffix = if devices.len() > 1 {
            format!(" ({})", dev.interface)
        } else {
            String::new()
        };

        if dev.is_connected() {
            let list = sys::group(container, &format!("Connection{suffix}"));
            let status = format!(
                "Connected to {}",
                dev.connection.as_deref().unwrap_or("a network")
            );
            sys::row(
                &list,
                "Status",
                Some(&status),
                &gtk::Box::new(gtk::Orientation::Horizontal, 0),
            );
            crate::pages::network::detail_rows(&list, dev);
        }

        let list = sys::group(container, &format!("Visible networks{suffix}"));
        let scan = gtk::Button::with_label("Search Again");
        let path = dev.path.clone();
        scan.connect_clicked(move |_| {
            let path = path.clone();
            glib::spawn_future_local(async move {
                // NetworkManager refuses scans that come too soon after the
                // previous one; that is harmless, so don't bother the user.
                if let Err(e) = nm::request_scan(&path).await {
                    eprintln!("settingsgoo: Wi-Fi scan: {e}");
                }
            });
        });
        let status = if dev.is_connected() {
            "Searching automatically"
        } else {
            dev.state_name()
        };
        sys::row(&list, "Status", Some(status), &scan);
        if wd.networks.is_empty() {
            sys::row(
                &list,
                "No networks found",
                None,
                &gtk::Box::new(gtk::Orientation::Horizontal, 0),
            );
        }
        for net in &wd.networks {
            network_row(&list, &dev.path, net);
        }
    }
}

pub fn build() -> gtk::Widget {
    let page = sys::page("Wi-Fi", "Wireless networks, managed by NetworkManager.");
    if !nm::available() {
        page.append(&sys::dim_label("NetworkManager is not running."));
        return page.upcast();
    }

    let list = sys::group(&page, "");
    let radio = nm::wifi_radio();
    let toggle = Toggle::new(radio.enabled);
    toggle.on_change(nm::set_wifi_enabled);
    let subtitle = if radio.hardware_enabled {
        "Use wireless networks"
    } else {
        "Turned off by a hardware switch"
    };
    toggle.switch.set_sensitive(radio.hardware_enabled);
    sys::row(&list, "Wi-Fi", Some(subtitle), &toggle.switch);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.append(&container);

    type State = Option<(bool, Vec<WifiDevice>)>;
    let shown: Rc<RefCell<State>> = Rc::default();
    let refresh = move || {
        let radio = nm::wifi_radio();
        toggle.set(radio.enabled);
        toggle.switch.set_sensitive(radio.hardware_enabled);
        let devices = nm::wifi_devices();
        // The on/off switch means nothing without an adapter.
        if let Some(frame) = list.parent() {
            frame.set_visible(!devices.is_empty());
        }
        let state = Some((radio.enabled, devices));
        // Rebuild only when something visible changed.
        if *shown.borrow() != state {
            if let Some((enabled, devices)) = &state {
                render(&container, devices, *enabled);
            }
            *shown.borrow_mut() = state;
        }
    };
    refresh();
    nm::watch(refresh);
    page.upcast()
}
