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
        };
        choice.set_items(items, selected);
        choice
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
    /// the previous item is selected again and the error is shown.
    pub fn on_change<F, Fut>(&self, apply: F)
    where
        F: Fn(String) -> Fut + 'static,
        Fut: Future<Output = Result<(), String>> + 'static,
    {
        let this = self.clone();
        let apply = Rc::new(apply);
        self.dropdown.connect_selected_notify(move |dd| {
            if this.quiet.get() {
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
                    show_error(&this.dropdown, "Could not change the setting", &e);
                }
            });
        });
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
