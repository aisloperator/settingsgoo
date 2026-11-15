//! Bluetooth: turn adapters on and off, rename them and make them visible;
//! pair, connect, disconnect and forget devices, via BlueZ.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::bluez::{self, Adapter, Device, Reply, Request};
use crate::sys::{self, Toggle};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Busy {
    Pairing,
    Connecting,
    Disconnecting,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Pairing => "Pairing…",
            Busy::Connecting => "Connecting…",
            Busy::Disconnecting => "Disconnecting…",
        }
    }
}

/// What the device lists show; they are rebuilt only when this changes.
#[derive(PartialEq)]
struct State {
    adapters: Vec<Adapter>,
    devices: Vec<Device>,
    busy: BTreeMap<String, Busy>,
}

/// An open pairing prompt.
struct Prompt {
    window: gtk::Window,
    /// The changing part of a "type this code" prompt.
    detail: gtk::Label,
    device: String,
    /// Only shows a code; closes by itself once the device is paired.
    display_only: bool,
    /// Set once answered, so closing the window doesn't answer again.
    answered: Rc<Cell<bool>>,
}

impl Prompt {
    /// Closes the window without answering.
    fn dismiss(&self) {
        self.answered.set(true);
        self.window.close();
    }
}

struct BluetoothPage {
    adapter_box: gtk::Box,
    device_box: gtk::Box,
    /// Adapter controls are rebuilt only when these change (ignoring
    /// `discovering`), so typing a new name isn't interrupted.
    shown_adapters: RefCell<Option<Option<Vec<Adapter>>>>,
    shown: RefCell<Option<State>>,
    /// The latest devices, to name them in prompts.
    devices: RefCell<Vec<Device>>,
    busy: RefCell<BTreeMap<String, Busy>>,
    /// Whether the page is on screen; it searches for devices only then.
    visible: Cell<bool>,
    /// Adapters this page has started discovery on.
    discovering: RefCell<HashSet<String>>,
    prompt: RefCell<Option<Prompt>>,
    /// Devices whose pairing the user turned down in a prompt here, so the
    /// failed pairing needs no error message.
    refused: RefCell<HashSet<String>>,
}

fn suffix(adapters: &[Adapter], a: &Adapter) -> String {
    if adapters.len() > 1 {
        let name = a.path.rsplit('/').next().unwrap_or(&a.path);
        format!(" ({name})")
    } else {
        String::new()
    }
}

/// A symbolic icon for the kind of device, if the icon theme has one.
fn device_icon(dev: &Device) -> &'static str {
    let theme = gdk::Display::default().map(|d| gtk::IconTheme::for_display(&d));
    let candidates: &[&'static str] = match dev.icon.as_str() {
        "audio-headset" => &["audio-headset-symbolic", "audio-headphones-symbolic"],
        "audio-headphones" => &["audio-headphones-symbolic"],
        "audio-card" | "audio-speakers" => &["audio-speakers-symbolic"],
        "input-keyboard" => &["input-keyboard-symbolic"],
        "input-mouse" => &["input-mouse-symbolic"],
        "input-tablet" => &["input-tablet-symbolic"],
        "input-gaming" => &["input-gaming-symbolic"],
        "phone" => &["phone-symbolic"],
        "computer" => &["computer-symbolic"],
        "printer" => &["printer-symbolic"],
        "camera-photo" | "camera-video" => &["camera-photo-symbolic"],
        "multimedia-player" => &["multimedia-player-symbolic"],
        "video-display" => &["video-display-symbolic"],
        _ => &[],
    };
    candidates
        .iter()
        .chain(&["bluetooth-active-symbolic", "bluetooth-symbolic"])
        .copied()
        .find(|name| theme.as_ref().is_none_or(|t| t.has_icon(name)))
        .unwrap_or("bluetooth-active-symbolic")
}

fn signal_name(bars: u8) -> &'static str {
    match bars {
        0 | 1 => "Signal weak",
        2 => "Signal fair",
        3 => "Signal good",
        _ => "Signal excellent",
    }
}

/// Paired devices: by name, so rows don't move when a device connects or
/// disconnects (they would jump away from under the pointer). Nearby devices: strongest
/// first, then by name; ones that haven't said their name are left out, as
/// they are usually beacons and other gadgets that can't be paired.
fn split_devices<'a>(devices: &'a [Device], adapter: &str) -> (Vec<&'a Device>, Vec<&'a Device>) {
    let mut paired: Vec<&Device> = devices
        .iter()
        .filter(|d| d.adapter == adapter && d.paired)
        .collect();
    paired.sort_by_key(|d| d.alias.to_lowercase());
    let mut nearby: Vec<&Device> = devices
        .iter()
        .filter(|d| d.adapter == adapter && !d.paired && d.name.is_some())
        .collect();
    nearby.sort_by_key(|d| {
        (
            std::cmp::Reverse(d.signal.unwrap_or(0)),
            d.alias.to_lowercase(),
        )
    });
    (paired, nearby)
}

/// Runs a BlueZ change and shows any error.
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

/// What a prompt's text field accepts.
#[derive(Clone, Copy)]
enum Input {
    /// A legacy PIN: 1–16 characters.
    Pin,
    /// A passkey: up to 6 digits.
    Passkey,
}

impl Input {
    fn valid(self, text: &str) -> bool {
        match self {
            Input::Pin => (1..=16).contains(&text.chars().count()),
            Input::Passkey => {
                (1..=6).contains(&text.len()) && text.chars().all(|c| c.is_ascii_digit())
            }
        }
    }
}

/// A small modal window for pairing. `on_answer` gets `Some(text)` (empty
/// without an `input`) for the OK button and `None` for Cancel or closing.
fn prompt_window(
    parent: &impl IsA<gtk::Widget>,
    message: &str,
    detail: &str,
    input: Option<Input>,
    ok_label: Option<&str>,
    answered: Rc<Cell<bool>>,
    on_answer: impl Fn(Option<String>) + 'static,
) -> (gtk::Window, gtk::Label) {
    let window = gtk::Window::builder()
        .title("Bluetooth Pairing")
        .modal(true)
        .resizable(false)
        .default_width(400)
        .build();
    if let Some(root) = parent.root().and_downcast::<gtk::Window>() {
        window.set_transient_for(Some(&root));
    }
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    vbox.append(
        &gtk::Label::builder()
            .label(message)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build(),
    );
    let detail_label = sys::dim_label(detail);
    detail_label.set_visible(!detail.is_empty());
    vbox.append(&detail_label);

    let entry = input.map(|_| gtk::Entry::builder().activates_default(true).build());
    if let Some(e) = &entry {
        if matches!(input, Some(Input::Passkey)) {
            e.set_input_purpose(gtk::InputPurpose::Digits);
            e.set_max_length(6);
        } else {
            e.set_max_length(16);
        }
        vbox.append(e);
    }

    let on_answer = Rc::new(on_answer);
    let answer = {
        let answered = answered.clone();
        let on_answer = on_answer.clone();
        move |a: Option<String>| {
            if !answered.replace(true) {
                on_answer(a);
            }
        }
    };

    let buttons = gtk::Box::builder()
        .spacing(6)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    buttons.append(&cancel);
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    if let Some(label) = ok_label {
        let ok = gtk::Button::builder()
            .label(label)
            .css_classes(["suggested-action"])
            .sensitive(entry.is_none())
            .build();
        buttons.append(&ok);
        window.set_default_widget(Some(&ok));
        if let (Some(e), Some(input)) = (&entry, input) {
            let ok = ok.clone();
            e.connect_changed(move |e| ok.set_sensitive(input.valid(&e.text())));
        }
        let (window, entry, answer) = (window.clone(), entry.clone(), answer.clone());
        ok.connect_clicked(move |_| {
            answer(Some(
                entry
                    .as_ref()
                    .map(|e| e.text().to_string())
                    .unwrap_or_default(),
            ));
            window.close();
        });
    }
    vbox.append(&buttons);
    window.set_child(Some(&vbox));

    // Escape and the close button count as Cancel.
    let keys = gtk::ShortcutController::new();
    keys.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::NamedAction::new("window.close")),
    ));
    window.add_controller(keys);
    window.connect_close_request(move |_| {
        answer(None);
        glib::Propagation::Proceed
    });

    window.present();
    match &entry {
        Some(e) => {
            e.grab_focus();
        }
        None => {
            cancel.grab_focus();
        }
    }
    (window, detail_label)
}

impl BluetoothPage {
    fn refresh(self: &Rc<Self>) {
        let objects = bluez::objects();
        let adapters = objects.as_ref().map(|o| o.adapters.clone());
        let devices = objects.map(|o| o.devices).unwrap_or_default();

        // Adapter controls.
        let masked = adapters.clone().map(|list| {
            list.into_iter()
                .map(|a| Adapter {
                    discovering: false,
                    ..a
                })
                .collect::<Vec<_>>()
        });
        if self.shown_adapters.borrow().as_ref() != Some(&masked) {
            self.render_adapters(masked.as_ref());
            *self.shown_adapters.borrow_mut() = Some(masked);
        }

        let adapters = adapters.unwrap_or_default();
        self.update_discovery(&adapters);

        // A "type this code" prompt goes away once pairing has finished.
        let finished = self.prompt.borrow().as_ref().is_some_and(|p| {
            p.display_only && devices.iter().any(|d| d.path == p.device && d.paired)
        });
        if finished && let Some(p) = self.prompt.take() {
            p.dismiss();
        }

        let state = State {
            adapters,
            devices: devices.clone(),
            busy: self.busy.borrow().clone(),
        };
        *self.devices.borrow_mut() = devices;
        if self.shown.borrow().as_ref() != Some(&state) {
            self.render_devices(&state);
            *self.shown.borrow_mut() = Some(state);
        }
    }

    /// Searches for devices on every powered adapter while the page is
    /// visible. BlueZ stops a search by itself when the app exits.
    fn update_discovery(self: &Rc<Self>, adapters: &[Adapter]) {
        let mut ours = self.discovering.borrow_mut();
        ours.retain(|p| adapters.iter().any(|a| &a.path == p && a.powered));
        if !self.visible.get() {
            for path in ours.drain() {
                glib::spawn_future_local(async move {
                    let _ = bluez::stop_discovery(&path).await;
                });
            }
            return;
        }
        for a in adapters.iter().filter(|a| a.powered) {
            if !ours.insert(a.path.clone()) {
                continue;
            }
            let path = a.path.clone();
            let this = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                if let Err(e) = bluez::start_discovery(&path).await {
                    eprintln!("settingsgoo: Bluetooth search: {e}");
                    // Try again on the next change.
                    if let Some(this) = this.upgrade() {
                        this.discovering.borrow_mut().remove(&path);
                    }
                }
            });
        }
    }

    fn set_visible(self: &Rc<Self>, visible: bool) {
        self.visible.set(visible);
        self.refresh();
    }

    fn render_adapters(self: &Rc<Self>, adapters: Option<&Vec<Adapter>>) {
        let container = &self.adapter_box;
        sys::clear(container);
        let Some(adapters) = adapters else {
            container.append(&sys::dim_label(
                "The Bluetooth service (bluetoothd, part of BlueZ) is not running.",
            ));
            return;
        };
        if adapters.is_empty() {
            container.append(&sys::dim_label("No Bluetooth adapter was found."));
            return;
        }
        for a in adapters {
            // One adapter needs no heading; several are told apart by name.
            let title = if adapters.len() > 1 {
                format!("Adapter{}", suffix(adapters, a))
            } else {
                String::new()
            };
            let list = sys::group(container, &title);

            let toggle = Toggle::new(a.powered);
            let path = a.path.clone();
            toggle.on_change(move |on| {
                let path = path.clone();
                async move { bluez::set_powered(&path, on).await }
            });
            toggle.switch.set_sensitive(!a.blocked());
            let subtitle = if a.blocked() {
                "Blocked by a hardware or software switch (rfkill)"
            } else {
                "Use Bluetooth devices"
            };
            sys::row(&list, "Bluetooth", Some(subtitle), &toggle.switch);
            if !a.powered {
                continue;
            }

            sys::row(
                &list,
                "Name",
                Some("How this computer appears to other devices"),
                &self.name_entry(a),
            );

            let toggle = Toggle::new(a.discoverable);
            let path = a.path.clone();
            toggle.on_change(move |on| {
                let path = path.clone();
                async move { bluez::set_discoverable(&path, on).await }
            });
            let mut subtitle = format!("Other devices can find this computer as “{}”", a.alias);
            if a.discoverable_timeout > 0 {
                let minutes = a.discoverable_timeout.div_ceil(60);
                let unit = if minutes == 1 { "minute" } else { "minutes" };
                subtitle.push_str(&format!(" for {minutes} {unit}"));
            }
            sys::row(
                &list,
                "Visible to other devices",
                Some(&subtitle),
                &toggle.switch,
            );

            sys::row(&list, "Address", None, &sys::value_label(&a.address));
        }
    }

    /// An entry for the adapter's name, applied with Enter or on leaving it.
    fn name_entry(self: &Rc<Self>, a: &Adapter) -> gtk::Entry {
        let entry = gtk::Entry::builder()
            .text(&a.alias)
            .max_length(248)
            .width_chars(20)
            .build();
        // The name last applied, so pressing Enter and then leaving the
        // entry doesn't rename twice.
        let current = Rc::new(RefCell::new(a.alias.clone()));
        let apply = {
            let path = a.path.clone();
            move |e: &gtk::Entry| {
                let new = e.text().trim().to_string();
                let old = current.borrow().clone();
                if new.is_empty() || new == old {
                    e.set_text(&old);
                    return;
                }
                *current.borrow_mut() = new.clone();
                let (e, path, current) = (e.clone(), path.clone(), current.clone());
                glib::spawn_future_local(async move {
                    if let Err(err) = bluez::set_alias(&path, &new).await {
                        *current.borrow_mut() = old.clone();
                        e.set_text(&old);
                        sys::show_error(&e, "Could not rename the adapter", &err);
                    }
                });
            }
        };
        let apply = Rc::new(apply);
        {
            let apply = apply.clone();
            entry.connect_activate(move |e| apply(e));
        }
        let focus = gtk::EventControllerFocus::new();
        {
            let entry = entry.clone();
            focus.connect_leave(move |_| apply(&entry));
        }
        entry.add_controller(focus);
        entry
    }

    fn render_devices(self: &Rc<Self>, state: &State) {
        let container = &self.device_box;
        sys::clear(container);
        for a in state.adapters.iter().filter(|a| a.powered) {
            let suffix = suffix(&state.adapters, a);
            let (paired, nearby) = split_devices(&state.devices, &a.path);

            let list = sys::group(container, &format!("My devices{suffix}"));
            if paired.is_empty() {
                sys::row(&list, "No paired devices", None, &empty());
            }
            for d in paired {
                self.device_row(&list, a, d, state.busy.get(&d.path).copied());
            }

            let list = sys::group(container, &format!("Nearby devices{suffix}"));
            let spinner = gtk::Spinner::new();
            spinner.set_spinning(a.discovering);
            let (title, subtitle) = if a.discovering {
                (
                    "Searching…",
                    "To pair a device, put it in pairing mode. The search runs while this page is open.",
                )
            } else {
                ("Nearby devices", "Not searching right now")
            };
            sys::row(&list, title, Some(subtitle), &spinner);
            if nearby.is_empty() {
                sys::row(&list, "No devices found yet", None, &empty());
            }
            for d in nearby {
                self.device_row(&list, a, d, state.busy.get(&d.path).copied());
            }
        }
    }

    fn device_row(
        self: &Rc<Self>,
        list: &gtk::ListBox,
        a: &Adapter,
        d: &Device,
        busy: Option<Busy>,
    ) {
        let mut subtitle: Vec<String> = vec![];
        if let Some(b) = busy {
            subtitle.push(b.label().into());
        } else if d.paired {
            subtitle.push(
                if d.connected {
                    "Connected"
                } else {
                    "Not connected"
                }
                .into(),
            );
        }
        if d.blocked {
            subtitle.push("Blocked".into());
        }
        if let Some(k) = d.kind_name() {
            subtitle.push(k.into());
        }
        if let Some(b) = d.battery {
            subtitle.push(format!("Battery {b}%"));
        }
        if !d.paired
            && let Some(s) = d.signal
        {
            subtitle.push(signal_name(s).into());
        }
        if subtitle.is_empty() {
            subtitle.push(d.address.clone());
        }
        let subtitle = subtitle.join(" · ");

        let buttons = gtk::Box::builder().spacing(6).build();
        if busy.is_some() {
            let spinner = gtk::Spinner::new();
            spinner.set_spinning(true);
            buttons.append(&spinner);
        }
        if busy == Some(Busy::Pairing) {
            let b = gtk::Button::with_label("Cancel");
            let path = d.path.clone();
            b.connect_clicked(move |_| {
                let path = path.clone();
                glib::spawn_future_local(async move {
                    let _ = bluez::cancel_pairing(&path).await;
                });
            });
            buttons.append(&b);
        } else if d.paired {
            let forget = gtk::Button::with_label("Forget");
            forget.set_sensitive(busy.is_none());
            let (adapter, dev) = (a.path.clone(), d.clone());
            forget.connect_clicked(move |b| forget_device(b, &adapter, &dev));
            buttons.append(&forget);

            let connected = d.connected;
            let b = gtk::Button::with_label(if connected { "Disconnect" } else { "Connect" });
            b.set_sensitive(busy.is_none() && !d.blocked);
            let path = d.path.clone();
            let this = Rc::downgrade(self);
            b.connect_clicked(move |b| {
                let Some(this) = this.upgrade() else { return };
                if connected {
                    let p = path.clone();
                    let fut = async move { bluez::disconnect(&p).await };
                    this.run_busy(b, &path, Busy::Disconnecting, "Could not disconnect", fut);
                } else {
                    let p = path.clone();
                    let fut = async move { bluez::connect(&p).await };
                    this.run_busy(b, &path, Busy::Connecting, "Could not connect", fut);
                }
            });
            buttons.append(&b);
        } else {
            let b = gtk::Button::with_label("Pair");
            b.set_sensitive(busy.is_none() && !d.blocked);
            let path = d.path.clone();
            let this = Rc::downgrade(self);
            b.connect_clicked(move |b| {
                if let Some(this) = this.upgrade() {
                    this.pair(b, &path);
                }
            });
            buttons.append(&b);
        }
        sys::row_with_icon(
            list,
            Some(device_icon(d)),
            &d.alias,
            Some(&subtitle),
            &buttons,
        );
    }

    fn set_busy(self: &Rc<Self>, path: &str, busy: Option<Busy>) {
        match busy {
            Some(b) => self.busy.borrow_mut().insert(path.to_string(), b),
            None => self.busy.borrow_mut().remove(path),
        };
        self.refresh();
    }

    /// Runs a change, showing the device as busy meanwhile.
    fn run_busy(
        self: &Rc<Self>,
        widget: &gtk::Button,
        path: &str,
        busy: Busy,
        what: &'static str,
        fut: impl Future<Output = Result<(), String>> + 'static,
    ) {
        self.set_busy(path, Some(busy));
        let (this, path) = (self.clone(), path.to_string());
        spawn(widget, what, async move {
            let result = fut.await;
            this.set_busy(&path, None);
            result
        });
    }

    /// Pairs, then trusts the device (so it can reconnect by itself) and
    /// connects to it, as other desktops do.
    fn pair(self: &Rc<Self>, widget: &gtk::Button, path: &str) {
        self.refused.borrow_mut().remove(path);
        self.set_busy(path, Some(Busy::Pairing));
        let (this, path, widget) = (self.clone(), path.to_string(), widget.clone());
        glib::spawn_future_local(async move {
            let result = bluez::pair(&path).await;
            let open = this
                .prompt
                .borrow()
                .as_ref()
                .is_some_and(|p| p.device == path);
            if open && let Some(p) = this.prompt.take() {
                p.dismiss();
            }
            if let Err(e) = result {
                this.set_busy(&path, None);
                // Canceled or refused by the user here: no need to say so.
                let refused = this.refused.borrow_mut().remove(&path);
                if !refused && !e.contains("canceled") && !e.contains("Canceled") {
                    sys::show_error(&widget, "Could not pair", &e);
                }
                return;
            }
            if let Err(e) = bluez::set_trusted(&path, true).await {
                eprintln!("settingsgoo: Bluetooth trust: {e}");
            }
            this.set_busy(&path, Some(Busy::Connecting));
            // Some paired devices, such as phones, have nothing to connect
            // to; they stay paired, so this isn't worth an error message.
            if let Err(e) = bluez::connect(&path).await {
                eprintln!("settingsgoo: Bluetooth connect after pairing: {e}");
            }
            this.set_busy(&path, None);
        });
    }

    fn device_name(&self, path: &str) -> String {
        self.devices
            .borrow()
            .iter()
            .find(|d| d.path == path)
            .map(|d| format!("“{}”", d.alias))
            .unwrap_or_else(|| "the device".into())
    }

    /// Answers a request from BlueZ's pairing agent.
    fn handle_agent(self: &Rc<Self>, req: Request, reply: Reply) {
        // A new request replaces any open prompt.
        let current = self.prompt.borrow().as_ref().map(|p| p.device.clone());
        if let Request::DisplayPasskey {
            device, entered, ..
        } = &req
            && current.as_ref() == Some(device)
            && let Some(p) = self.prompt.borrow().as_ref()
        {
            p.detail.set_label(&typed_digits(*entered));
            p.detail.set_visible(true);
            return;
        }
        if let Some(p) = self.prompt.take() {
            p.dismiss();
        }

        let reply = Rc::new(RefCell::new(Some(reply)));
        let take = move || reply.borrow_mut().take();
        let (device, message, detail, input, ok, display_only) = match &req {
            Request::Cancel => return,
            Request::PinCode { device } => (
                device,
                format!("Enter a PIN for {}", self.device_name(device)),
                "Type the same PIN on the device. Many devices without a keypad use 0000 or 1234."
                    .to_string(),
                Some(Input::Pin),
                Some("Pair"),
                false,
            ),
            Request::Passkey { device } => (
                device,
                format!("Enter the code shown on {}", self.device_name(device)),
                String::new(),
                Some(Input::Passkey),
                Some("Pair"),
                false,
            ),
            Request::DisplayPinCode { device, pin } => (
                device,
                format!(
                    "Type {pin} on {}, then press Enter",
                    self.device_name(device)
                ),
                String::new(),
                None,
                None,
                true,
            ),
            Request::DisplayPasskey {
                device,
                passkey,
                entered,
            } => (
                device,
                format!(
                    "Type {passkey:06} on {}, then press Enter",
                    self.device_name(device)
                ),
                typed_digits(*entered),
                None,
                None,
                true,
            ),
            Request::Confirm { device, passkey } => (
                device,
                format!(
                    "Does {} show the code {passkey:06}?",
                    self.device_name(device)
                ),
                "Only confirm if the codes match.".to_string(),
                None,
                Some("Confirm"),
                false,
            ),
            Request::Authorize { device } => (
                device,
                format!(
                    "Allow {} to pair with this computer?",
                    self.device_name(device)
                ),
                String::new(),
                None,
                Some("Allow"),
                false,
            ),
            Request::AuthorizeService { device, uuid } => {
                let known = self
                    .devices
                    .borrow()
                    .iter()
                    .any(|d| &d.path == device && d.paired && d.trusted);
                if known {
                    if let Some(r) = take() {
                        r.accept();
                    }
                    return;
                }
                let service = bluez::service_name(uuid)
                    .map(String::from)
                    .unwrap_or_else(|| format!("a service ({uuid})"));
                (
                    device,
                    format!("Allow {} to use {service}?", self.device_name(device)),
                    String::new(),
                    None,
                    Some("Allow"),
                    false,
                )
            }
        };

        let device = device.clone();
        let answered = Rc::new(Cell::new(false));
        let dev = device.clone();
        let this = Rc::downgrade(self);
        let on_answer = move |answer: Option<String>| {
            forget_prompt(&this, &dev);
            if answer.is_none()
                && let Some(this) = this.upgrade()
            {
                this.refused.borrow_mut().insert(dev.clone());
            }
            let reply = take();
            match (&req, answer) {
                // Closing a "type this code" prompt cancels the pairing.
                (Request::DisplayPinCode { .. } | Request::DisplayPasskey { .. }, _) => {
                    let dev = dev.clone();
                    glib::spawn_future_local(async move {
                        let _ = bluez::cancel_pairing(&dev).await;
                    });
                }
                (_, None) => drop(reply), // refuses
                (Request::PinCode { .. }, Some(pin)) => {
                    if let Some(r) = reply {
                        r.pin(&pin);
                    }
                }
                (Request::Passkey { .. }, Some(key)) => {
                    if let (Some(r), Ok(k)) = (reply, key.parse::<u32>()) {
                        r.passkey(k);
                    }
                }
                (_, Some(_)) => {
                    if let Some(r) = reply {
                        r.accept();
                    }
                }
            }
        };
        let (window, detail) = prompt_window(
            &self.device_box,
            &message,
            &detail,
            input,
            ok,
            answered.clone(),
            on_answer,
        );
        *self.prompt.borrow_mut() = Some(Prompt {
            window,
            detail,
            device,
            display_only,
            answered,
        });
    }
}

fn forget_prompt(this: &Weak<BluetoothPage>, device: &str) {
    if let Some(this) = this.upgrade() {
        let mine = this
            .prompt
            .borrow()
            .as_ref()
            .is_some_and(|p| p.device == device);
        if mine {
            this.prompt.take();
        }
    }
}

fn typed_digits(entered: u16) -> String {
    if entered == 0 {
        String::new()
    } else {
        format!("{} of 6 digits typed", entered.min(6))
    }
}

fn empty() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, 0)
}

fn forget_device(widget: &gtk::Button, adapter: &str, dev: &Device) {
    let parent = widget.root().and_downcast::<gtk::Window>();
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("Forget “{}”?", dev.alias))
        .detail("It will be unpaired. To use it again, pair it again.")
        .buttons(["Cancel", "Forget"])
        .cancel_button(0)
        .default_button(0)
        .build();
    let (widget, adapter, path) = (widget.clone(), adapter.to_string(), dev.path.clone());
    glib::spawn_future_local(async move {
        if dialog.choose_future(parent.as_ref()).await == Ok(1)
            && let Err(e) = bluez::remove(&adapter, &path).await
        {
            sys::show_error(&widget, "Could not forget the device", &e);
        }
    });
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Bluetooth",
        "Bluetooth adapters and devices, managed by BlueZ.",
    );
    let adapter_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let device_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.append(&adapter_box);
    page.append(&device_box);

    let this = Rc::new(BluetoothPage {
        adapter_box,
        device_box,
        shown_adapters: RefCell::default(),
        shown: RefCell::default(),
        devices: RefCell::default(),
        busy: RefCell::default(),
        visible: Cell::new(false),
        discovering: RefCell::default(),
        prompt: RefCell::default(),
        refused: RefCell::default(),
    });
    this.refresh();
    {
        let this = this.clone();
        bluez::watch(move || this.refresh());
    }
    {
        let this = Rc::downgrade(&this);
        bluez::start_agent(move |req, reply| {
            if let Some(this) = this.upgrade() {
                this.handle_agent(req, reply);
            }
        });
    }
    // A page that isn't showing is unmapped, so search only while it's visible.
    {
        let this = this.clone();
        page.connect_map(move |_| this.set_visible(true));
    }
    page.connect_unmap(move |_| this.set_visible(false));
    page.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, paired: bool, connected: bool, signal: Option<u8>) -> Device {
        Device {
            path: format!("/org/bluez/hci0/{name}"),
            adapter: "/org/bluez/hci0".into(),
            address: "00:00:00:00:00:00".into(),
            name: (!name.starts_with('-')).then(|| name.to_string()),
            alias: name.into(),
            icon: String::new(),
            paired,
            trusted: paired,
            blocked: false,
            connected,
            signal,
            battery: None,
        }
    }

    #[test]
    fn devices_split_and_sorted() {
        let mut devices = vec![
            dev("mouse", true, true, None),
            dev("Keyboard", true, false, None),
            dev("far", false, false, Some(1)),
            dev("near", false, false, Some(4)),
            dev("-nameless", false, false, Some(4)),
            dev("also near", false, false, Some(4)),
        ];
        let mut other = dev("elsewhere", true, true, None);
        other.adapter = "/org/bluez/hci1".into();
        devices.push(other);
        let (paired, nearby) = split_devices(&devices, "/org/bluez/hci0");
        let names = |v: Vec<&Device>| v.iter().map(|d| d.alias.clone()).collect::<Vec<_>>();
        assert_eq!(names(paired), ["Keyboard", "mouse"]); // by name, not connection
        assert_eq!(names(nearby), ["also near", "near", "far"]);
    }

    #[test]
    fn code_input() {
        assert!(Input::Pin.valid("0000"));
        assert!(!Input::Pin.valid(""));
        assert!(!Input::Pin.valid(&"1".repeat(17)));
        assert!(Input::Passkey.valid("123456"));
        assert!(!Input::Passkey.valid("1234567"));
        assert!(!Input::Passkey.valid("12a"));
    }
}
