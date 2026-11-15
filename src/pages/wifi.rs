//! Wi-Fi: turn the radio on and off, see nearby networks, connect,
//! disconnect and forget networks, via NetworkManager.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

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
        subtitle.push("Connected".to_string());
    } else if net.saved.is_some() {
        subtitle.push("Saved".to_string());
    }
    subtitle.push(format!("Signal {}%", net.strength));
    subtitle.push(net.security.name().to_string());
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
        Some(signal_icon(net.bars())),
        &net.ssid,
        Some(&subtitle),
        &buttons,
    );
}

/// How often to ask for a new scan while the page is showing. GNOME's Wi-Fi
/// panel does much the same; NetworkManager itself scans rarely while
/// connected, so without this the list would go stale.
const SCAN_INTERVAL: Duration = Duration::from_secs(20);
/// Stop showing "Searching…" after this long even if no result arrived.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// What the page shows; it is rebuilt only when this changes.
#[derive(PartialEq)]
struct State {
    enabled: bool,
    devices: Vec<WifiDevice>,
    searching: bool,
}

struct WifiPage {
    radio_list: gtk::ListBox,
    toggle: Toggle,
    container: gtk::Box,
    shown: RefCell<Option<State>>,
    devices: RefCell<Vec<WifiDevice>>,
    /// Set while a scan we asked for is running.
    searching: Cell<bool>,
    /// The newest `last_scan` when the current search started.
    scan_mark: Cell<i64>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl WifiPage {
    fn refresh(self: &Rc<Self>) {
        let radio = nm::wifi_radio();
        self.toggle.set(radio.enabled);
        self.toggle.switch.set_sensitive(radio.hardware_enabled);
        let devices = nm::wifi_devices();
        // The on/off switch means nothing without an adapter.
        if let Some(frame) = self.radio_list.parent() {
            frame.set_visible(!devices.is_empty());
        }
        // A newer scan result means the search we started has finished.
        if self.searching.get() && devices.iter().any(|d| d.last_scan > self.scan_mark.get()) {
            self.searching.set(false);
        }
        let state = State {
            enabled: radio.enabled,
            devices: devices.clone(),
            searching: self.searching.get(),
        };
        *self.devices.borrow_mut() = devices;
        if self.shown.borrow().as_ref() != Some(&state) {
            self.render(&state);
            *self.shown.borrow_mut() = Some(state);
        }
    }

    /// Asks every Wi-Fi adapter to look for networks.
    fn scan(self: &Rc<Self>) {
        let devices = self.devices.borrow().clone();
        if devices.is_empty() || !nm::wifi_radio().enabled || self.searching.get() {
            return;
        }
        self.scan_mark
            .set(devices.iter().map(|d| d.last_scan).max().unwrap_or(-1));
        self.searching.set(true);
        self.refresh();
        for d in devices {
            glib::spawn_future_local(async move {
                // NetworkManager refuses scans that come too soon after the
                // previous one; that is harmless, so don't bother the user.
                if let Err(e) = nm::request_scan(&d.device.path).await {
                    eprintln!("settingsgoo: Wi-Fi scan: {e}");
                }
            });
        }
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(SCAN_TIMEOUT, move || {
            if let Some(this) = this.upgrade() {
                this.searching.set(false);
                this.refresh();
            }
        });
    }

    /// Scans now and then regularly while the page is visible.
    fn start_scanning(self: &Rc<Self>) {
        self.scan();
        let this = Rc::downgrade(self);
        let id = glib::timeout_add_local(SCAN_INTERVAL, move || {
            let Some(this) = this.upgrade() else {
                return glib::ControlFlow::Break;
            };
            this.scan();
            glib::ControlFlow::Continue
        });
        if let Some(old) = self.timer.replace(Some(id)) {
            old.remove();
        }
    }

    fn stop_scanning(&self) {
        if let Some(id) = self.timer.take() {
            id.remove();
        }
    }

    fn render(self: &Rc<Self>, state: &State) {
        let container = &self.container;
        sys::clear(container);
        if state.devices.is_empty() {
            container.append(&sys::dim_label("No Wi-Fi adapter was found."));
            return;
        }
        if !state.enabled {
            container.append(&sys::dim_label("Wi-Fi is turned off."));
            return;
        }
        for wd in &state.devices {
            let dev = &wd.device;
            let suffix = if state.devices.len() > 1 {
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
            let controls = gtk::Box::builder().spacing(12).build();
            let spinner = gtk::Spinner::new();
            spinner.set_spinning(state.searching);
            controls.append(&spinner);
            let scan = gtk::Button::with_label("Search Again");
            scan.set_sensitive(!state.searching);
            let this = Rc::downgrade(self);
            scan.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.scan();
                }
            });
            controls.append(&scan);
            let (title, subtitle) = if state.searching {
                ("Searching…", "Looking for nearby networks")
            } else {
                (
                    "Nearby networks",
                    "Updated automatically while this page is open",
                )
            };
            sys::row(&list, title, Some(subtitle), &controls);

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
}

pub fn build() -> gtk::Widget {
    let page = sys::page("Wi-Fi", "Wireless networks, managed by NetworkManager.");
    if !nm::available() {
        page.append(&sys::dim_label("NetworkManager is not running."));
        return page.upcast();
    }

    let radio_list = sys::group(&page, "");
    let radio = nm::wifi_radio();
    let toggle = Toggle::new(radio.enabled);
    toggle.on_change(nm::set_wifi_enabled);
    let subtitle = if radio.hardware_enabled {
        "Use wireless networks"
    } else {
        "Turned off by a hardware switch"
    };
    toggle.switch.set_sensitive(radio.hardware_enabled);
    sys::row(&radio_list, "Wi-Fi", Some(subtitle), &toggle.switch);

    let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.append(&container);

    let this = Rc::new(WifiPage {
        radio_list,
        toggle,
        container,
        shown: RefCell::default(),
        devices: RefCell::default(),
        searching: Cell::new(false),
        scan_mark: Cell::new(-1),
        timer: RefCell::default(),
    });
    this.refresh();
    {
        let this = this.clone();
        nm::watch(move || this.refresh());
    }
    // A page that isn't showing is unmapped, so scan only while it's visible.
    {
        let this = this.clone();
        page.connect_map(move |_| this.start_scanning());
    }
    page.connect_unmap(move |_| this.stop_scanning());
    page.upcast()
}
