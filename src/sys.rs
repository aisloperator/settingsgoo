//! Small helpers: system D-Bus access, running commands, and reusable widgets.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::process::Command;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};

// ---------------------------------------------------------------------------
// D-Bus

/// A proxy for a systemd/freedesktop service on the system bus, e.g.
/// `org.freedesktop.timedate1` at `/org/freedesktop/timedate1`.
pub fn system_proxy(name: &str) -> Option<gio::DBusProxy> {
    let path = format!("/{}", name.replace('.', "/"));
    let proxy = gio::DBusProxy::for_bus_sync(
        gio::BusType::System,
        gio::DBusProxyFlags::NONE,
        None,
        name,
        &path,
        name,
        None::<&gio::Cancellable>,
    )
    .ok()?;
    // With an activatable service the proxy is created even if nothing answers;
    // treat "no properties at all" as unavailable.
    if proxy.cached_property_names().is_empty() {
        return None;
    }
    Some(proxy)
}

pub fn prop_str(proxy: &gio::DBusProxy, name: &str) -> Option<String> {
    proxy.cached_property(name)?.get::<String>()
}

pub fn prop_bool(proxy: &gio::DBusProxy, name: &str) -> Option<bool> {
    proxy.cached_property(name)?.get::<bool>()
}

pub fn prop_strv(proxy: &gio::DBusProxy, name: &str) -> Option<Vec<String>> {
    proxy.cached_property(name)?.get::<Vec<String>>()
}

/// Calls a method, allowing polkit to ask the user for authorization.
pub async fn call(
    proxy: &gio::DBusProxy,
    method: &str,
    args: glib::Variant,
) -> Result<glib::Variant, String> {
    proxy
        .call_future(
            method,
            Some(&args),
            gio::DBusCallFlags::ALLOW_INTERACTIVE_AUTHORIZATION,
            -1,
        )
        .await
        .map_err(|mut e| {
            gio::DBusError::strip_remote_error(&mut e);
            e.message().to_string()
        })
}

/// A string-keyed dictionary of D-Bus values (`a{sv}`).
pub struct Dict(glib::VariantDict);

impl Dict {
    /// Wraps an `a{sv}` value. Anything else gives an empty dictionary, so
    /// unexpected data from a service can't crash the app.
    pub fn from(v: &glib::Variant) -> Self {
        let ok = v.type_() == glib::VariantTy::VARDICT;
        Dict(glib::VariantDict::new(ok.then_some(v)))
    }
    pub fn value(&self, key: &str) -> Option<glib::Variant> {
        self.0.lookup_value(key, None)
    }
    /// A value of type `T`, if present and of that type.
    pub fn get<T: glib::variant::FromVariant>(&self, key: &str) -> Option<T> {
        self.value(key)?.get()
    }
    /// A string or object path.
    pub fn str(&self, key: &str) -> String {
        self.value(key)
            .and_then(|v| v.str().map(String::from))
            .unwrap_or_default()
    }
    pub fn u32(&self, key: &str) -> u32 {
        self.get(key).unwrap_or_default()
    }
    pub fn bool(&self, key: &str) -> bool {
        self.get(key).unwrap_or_default()
    }
    pub fn bytes(&self, key: &str) -> Vec<u8> {
        self.get(key).unwrap_or_default()
    }
    /// An array of strings or object paths.
    pub fn paths(&self, key: &str) -> Vec<String> {
        self.value(key)
            .filter(|v| v.is_container())
            .map(|v| v.iter().filter_map(|c| c.str().map(String::from)).collect())
            .unwrap_or_default()
    }
    /// An array of dictionaries (`aa{sv}`).
    pub fn dicts(&self, key: &str) -> Vec<Dict> {
        self.value(key)
            .filter(|v| v.is_container())
            .map(|v| v.iter().map(|c| Dict::from(&c)).collect())
            .unwrap_or_default()
    }
}

/// One entry of a string-keyed dictionary of dictionaries (`a{sa{sv}}`), such
/// as a section of NetworkManager connection settings or one interface of a
/// D-Bus object in `GetManagedObjects`.
pub fn section(dicts: &glib::Variant, name: &str) -> Option<Dict> {
    if !dicts.is_container() {
        return None;
    }
    dicts
        .iter()
        .find(|entry| entry.n_children() == 2 && entry.child_value(0).str() == Some(name))
        .map(|entry| Dict::from(&entry.child_value(1)))
}

/// Calls `f` (at most about once a second) whenever the system-bus service
/// `name` sends any signal, such as a property change. The subscription lasts
/// for the life of the program.
pub fn watch_system_service(name: &str, f: impl Fn() + 'static) {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>) else {
        return;
    };
    let pending = Rc::new(Cell::new(false));
    let f = Rc::new(f);
    let sub = bus.subscribe_to_signal(
        Some(name),
        None,
        None,
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |_| {
            if pending.replace(true) {
                return;
            }
            let (pending, f) = (pending.clone(), f.clone());
            glib::timeout_add_local_once(std::time::Duration::from_millis(800), move || {
                pending.set(false);
                f();
            });
        },
    );
    // Pages live as long as the app, so keep the subscription forever.
    std::mem::forget(sub);
}

// ---------------------------------------------------------------------------
// Commands and files

/// Runs a program in the C locale and returns its stdout.
pub fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            format!("{program} failed ({})", out.status)
        } else {
            err
        })
    }
}

/// Runs a program without blocking the user interface and returns its stdout.
pub async fn run_async(argv: &[&str]) -> Result<String, String> {
    let program = argv.first().copied().unwrap_or_default();
    let os: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let proc = gio::Subprocess::newv(
        &os,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|e| format!("{program}: {e}"))?;
    let (out, err) = proc
        .communicate_utf8_future(None)
        .await
        .map_err(|e| format!("{program}: {e}"))?;
    if proc.is_successful() {
        return Ok(out.map(String::from).unwrap_or_default());
    }
    let err = err.map(|e| e.trim().to_string()).unwrap_or_default();
    Err(if !err.is_empty() {
        err
    } else {
        format!("{program} failed (exit status {})", proc.exit_status())
    })
}

/// Runs a program as root through `pkexec`, which asks for an administrator
/// password (through the session's polkit authentication agent) if needed.
pub async fn run_as_admin(argv: &[&str]) -> Result<String, String> {
    if !have_program("pkexec") {
        return Err("pkexec (part of polkit) is not installed.".into());
    }
    let mut full = vec!["pkexec"];
    full.extend_from_slice(argv);
    run_async(&full).await.map_err(|e| {
        // pkexec exits with 126 if the password dialog was dismissed and 127
        // if authorization failed, sometimes with nothing on stderr.
        if e.contains("exit status 126") || e.contains("dismissed") {
            "Authorization was cancelled.".into()
        } else if e.contains("exit status 127") || e.contains("Not authorized") {
            "Not authorized. This needs an administrator password, and a polkit \
             authentication agent running in the session."
                .into()
        } else {
            e
        }
    })
}

pub fn have_program(name: &str) -> bool {
    glib::find_program_in_path(name).is_some()
}

/// Reads `KEY=value` from an os-release style file, removing quotes.
pub fn read_env_file(path: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
    })
}

// ---------------------------------------------------------------------------
// Layout helpers

/// A page: a vertical box with a title and an optional introduction.
pub fn page(title: &str, intro: &str) -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(32)
        .margin_end(32)
        .build();
    b.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .css_classes(["page-title"])
            .build(),
    );
    if !intro.is_empty() {
        b.append(&dim_label(intro));
    }
    b
}

pub fn dim_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build()
}

/// A titled, framed list of rows appended to `parent`.
pub fn group(parent: &gtk::Box, title: &str) -> gtk::ListBox {
    if !title.is_empty() {
        parent.append(
            &gtk::Label::builder()
                .label(title)
                .xalign(0.0)
                .css_classes(["group-title"])
                .build(),
        );
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["settings-list"])
        .build();
    parent.append(&gtk::Frame::builder().child(&list).build());
    list
}

/// Adds a row with a title, optional subtitle and a control on the right.
pub fn row(
    list: &gtk::ListBox,
    title: &str,
    subtitle: Option<&str>,
    control: &impl IsA<gtk::Widget>,
) {
    row_with_icon(list, None, title, subtitle, control);
}

/// Like [`row`], with an optional icon (by icon name) at the start.
pub fn row_with_icon(
    list: &gtk::ListBox,
    icon: Option<&str>,
    title: &str,
    subtitle: Option<&str>,
    control: &impl IsA<gtk::Widget>,
) {
    let labels = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    labels.append(&gtk::Label::builder().label(title).xalign(0.0).build());
    if let Some(s) = subtitle {
        let sub = dim_label(s);
        sub.add_css_class("caption");
        labels.append(&sub);
    }
    let hbox = gtk::Box::builder()
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    if let Some(icon) = icon {
        hbox.append(&gtk::Image::from_icon_name(icon));
    }
    hbox.append(&labels);
    control.set_valign(gtk::Align::Center);
    hbox.append(control);
    list.append(
        &gtk::ListBoxRow::builder()
            .activatable(false)
            .child(&hbox)
            .build(),
    );
}

/// Removes all children of a box.
pub fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

/// A read-only, selectable value label for information rows.
pub fn value_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .selectable(true)
        .xalign(1.0)
        .wrap(true)
        .css_classes(["dim-label"])
        .build()
}

/// Asks the user to confirm an action. Returns true if they chose `action`.
pub async fn confirm(
    widget: &impl IsA<gtk::Widget>,
    message: &str,
    detail: &str,
    action: &str,
) -> bool {
    let parent = widget.root().and_downcast::<gtk::Window>();
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail(detail)
        .buttons(["Cancel", action])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.choose_future(parent.as_ref()).await == Ok(1)
}

pub fn show_error(widget: &impl IsA<gtk::Widget>, message: &str, detail: &str) {
    let parent = widget.root().and_downcast::<gtk::Window>();
    gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail(detail)
        .build()
        .show(parent.as_ref());
}

// ---------------------------------------------------------------------------
// Choice: a searchable drop-down of (value, label) pairs that reverts on failure

#[derive(Clone)]
pub struct Choice {
    pub dropdown: gtk::DropDown,
    values: Rc<RefCell<Vec<String>>>,
    current: Rc<RefCell<String>>,
    quiet: Rc<Cell<bool>>,
    /// Labels of items that are shown grayed out and can't be chosen, with
    /// the reason (shown as a tooltip).
    disabled: Rc<RefCell<std::collections::HashMap<String, String>>>,
}

/// Turns a list of strings into (value, label) pairs with identical parts.
pub fn same(items: &[String]) -> Vec<(String, String)> {
    items.iter().map(|s| (s.clone(), s.clone())).collect()
}

impl Choice {
    pub fn new(items: &[(String, String)], selected: &str) -> Self {
        let dropdown = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
        dropdown.set_expression(Some(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            None::<gtk::Expression>,
            "string",
        )));
        let choice = Choice {
            dropdown,
            values: Rc::default(),
            current: Rc::default(),
            quiet: Rc::default(),
            disabled: Rc::default(),
        };
        choice
            .dropdown
            .set_list_factory(Some(&choice.list_factory()));
        always_show_scrollbars(&choice.dropdown);
        choice.set_items(items, selected);
        // A disabled item can't be clicked, but could still be reached by
        // typing in the search box; go back to the previous item then.
        let this = choice.clone();
        choice.dropdown.connect_selected_notify(move |_| {
            if !this.quiet.get() && this.selected_is_disabled() {
                this.select(&this.value());
            }
        });
        choice
    }

    /// The rows of the pop-up list: a check mark on the selected item, and
    /// the label, grayed out if disabled.
    fn list_factory(&self) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let dropdown = self.dropdown.downgrade();
        // Shows the check mark if the row holds the drop-down's selected item.
        let update_check = move |item: &gtk::ListItem| {
            let check = item.child().and_then(|c| c.first_child());
            if let (Some(dd), Some(check)) = (dropdown.upgrade(), check) {
                let on = item.item().is_some() && item.item() == dd.selected_item();
                check.set_opacity(if on { 1.0 } else { 0.0 });
            }
        };
        let dropdown = self.dropdown.downgrade();
        let update = update_check.clone();
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let hbox = gtk::Box::builder().spacing(6).build();
            // The check mark goes first: at the end of the row it could be
            // hidden under the scrollbar.
            hbox.append(&gtk::Image::from_icon_name("object-select-symbolic"));
            hbox.append(&gtk::Label::builder().xalign(0.0).hexpand(true).build());
            item.set_child(Some(&hbox));
            if let Some(dd) = dropdown.upgrade() {
                let (item, update) = (item.downgrade(), update.clone());
                dd.connect_selected_item_notify(move |_| {
                    if let Some(item) = item.upgrade() {
                        update(&item);
                    }
                });
            }
        });
        let disabled = self.disabled.clone();
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let (Some(text), Some(label)) = (
                item.item().and_downcast::<gtk::StringObject>(),
                item.child()
                    .and_then(|c| c.last_child())
                    .and_downcast::<gtk::Label>(),
            ) else {
                return;
            };
            let text = text.string();
            let reason = disabled.borrow().get(text.as_str()).cloned();
            label.set_label(&text);
            label.set_sensitive(reason.is_none());
            label.set_tooltip_text(reason.as_deref());
            item.set_selectable(reason.is_none());
            item.set_activatable(reason.is_none());
            update_check(item);
        });
        factory
    }

    /// Grays out the items with these values; each maps to the reason why it
    /// can't be chosen. Call after `set_items`.
    pub fn set_disabled(&self, reasons: &std::collections::HashMap<String, String>) {
        let labels = self.dropdown.model().and_downcast::<gtk::StringList>();
        let values = self.values.borrow();
        let mut disabled = self.disabled.borrow_mut();
        disabled.clear();
        for (i, value) in values.iter().enumerate() {
            if let (Some(reason), Some(label)) = (
                reasons.get(value),
                labels.as_ref().and_then(|l| l.string(i as u32)),
            ) {
                disabled.insert(label.to_string(), reason.clone());
            }
        }
    }

    fn selected_is_disabled(&self) -> bool {
        self.dropdown
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .is_some_and(|s| self.disabled.borrow().contains_key(s.string().as_str()))
    }

    /// Replaces the items without triggering the change handler. If `selected`
    /// is not among them it is added at the top.
    pub fn set_items(&self, items: &[(String, String)], selected: &str) {
        let mut items = items.to_vec();
        if !items.iter().any(|(v, _)| v == selected) {
            let label = if selected.is_empty() {
                "(not set)".to_string()
            } else {
                selected.to_string()
            };
            items.insert(0, (selected.to_string(), label));
        }
        let labels: Vec<&str> = items.iter().map(|(_, l)| l.as_str()).collect();
        // Searching is only worth it for long lists such as time zones.
        self.dropdown.set_enable_search(items.len() > 15);
        self.disabled.borrow_mut().clear();
        self.quiet.set(true);
        self.dropdown
            .set_model(Some(&gtk::StringList::new(&labels)));
        *self.values.borrow_mut() = items.iter().map(|(v, _)| v.clone()).collect();
        self.quiet.set(false);
        self.select(selected);
    }

    /// Selects a value without triggering the change handler.
    pub fn select(&self, value: &str) {
        let pos = self.values.borrow().iter().position(|v| v == value);
        if let Some(pos) = pos {
            self.quiet.set(true);
            self.dropdown.set_selected(pos as u32);
            self.quiet.set(false);
            *self.current.borrow_mut() = value.to_string();
        }
    }

    pub fn value(&self) -> String {
        self.current.borrow().clone()
    }

    /// Runs `apply(new_value)` when the user picks another item. If it fails,
    /// the previous item is selected again and the error is shown (an empty
    /// error means the user cancelled, so nothing is shown).
    pub fn on_change<F, Fut>(&self, apply: F)
    where
        F: Fn(String) -> Fut + 'static,
        Fut: Future<Output = Result<(), String>> + 'static,
    {
        let this = self.clone();
        let apply = Rc::new(apply);
        self.dropdown.connect_selected_notify(move |dd| {
            if this.quiet.get() || this.selected_is_disabled() {
                return;
            }
            let Some(new) = this.values.borrow().get(dd.selected() as usize).cloned() else {
                return;
            };
            let old = this.value();
            if new == old {
                return;
            }
            *this.current.borrow_mut() = new.clone();
            let (this, apply) = (this.clone(), apply.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = apply(new).await {
                    this.select(&old);
                    if !e.is_empty() {
                        show_error(&this.dropdown, "Could not change the setting", &e);
                    }
                }
            });
        });
    }
}

/// Makes the scrollbars in a drop-down's pop-up list always visible when the
/// list is longer than the pop-up, instead of appearing only while the pointer
/// moves over it, so it's clear there are more items.
fn always_show_scrollbars(widget: &impl IsA<gtk::Widget>) {
    let mut child = widget.as_ref().first_child();
    while let Some(c) = child {
        if let Some(sw) = c.downcast_ref::<gtk::ScrolledWindow>() {
            sw.set_overlay_scrolling(false);
        }
        always_show_scrollbars(&c);
        child = c.next_sibling();
    }
}

// ---------------------------------------------------------------------------
// Toggle: a switch that reverts on failure

#[derive(Clone)]
pub struct Toggle {
    pub switch: gtk::Switch,
    quiet: Rc<Cell<bool>>,
}

impl Toggle {
    pub fn new(active: bool) -> Self {
        let t = Toggle {
            switch: gtk::Switch::new(),
            quiet: Rc::default(),
        };
        t.set(active);
        t
    }

    /// Sets the state without triggering the change handler.
    pub fn set(&self, active: bool) {
        self.quiet.set(true);
        self.switch.set_active(active);
        self.quiet.set(false);
    }

    pub fn on_change<F, Fut>(&self, apply: F)
    where
        F: Fn(bool) -> Fut + 'static,
        Fut: Future<Output = Result<(), String>> + 'static,
    {
        let this = self.clone();
        let apply = Rc::new(apply);
        self.switch.connect_active_notify(move |sw| {
            if this.quiet.get() {
                return;
            }
            let active = sw.is_active();
            let (this, apply) = (this.clone(), apply.clone());
            glib::spawn_future_local(async move {
                if let Err(e) = apply(active).await {
                    this.set(!active);
                    show_error(&this.switch, "Could not change the setting", &e);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn sections() {
        let mut wifi: HashMap<String, glib::Variant> = HashMap::new();
        wifi.insert("ssid".into(), b"Home".to_vec().to_variant());
        let mut all: HashMap<String, HashMap<String, glib::Variant>> = HashMap::new();
        all.insert("802-11-wireless".into(), wifi);
        let v = all.to_variant();
        let w = section(&v, "802-11-wireless").unwrap();
        assert_eq!(w.bytes("ssid"), b"Home");
        assert!(section(&v, "ipv4").is_none());
        // The wrong type gives an empty dictionary instead of a panic.
        assert_eq!(Dict::from(&v).str("anything"), "");
        assert!(section(&"text".to_variant(), "x").is_none());
        // A value of the wrong type reads as missing.
        assert_eq!(w.get::<u32>("ssid"), None);
        assert!(w.paths("ssid").is_empty());
    }
}
