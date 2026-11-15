//! Appearance: color scheme, themes, cursor and font.
//!
//! Settings are written to the places that toolkits and portals read, none of
//! which belong to a particular window manager or compositor:
//!
//! * the `org.gnome.desktop.interface` GSettings schema, if it is installed
//!   (read by GTK, `xdg-desktop-portal` and many other programs);
//! * `~/.config/gtk-3.0/settings.ini` and `~/.config/gtk-4.0/settings.ini`;
//! * `~/.icons/default/index.theme` for the X cursor theme fallback.

use std::collections::BTreeSet;
use std::path::PathBuf;

use gtk::prelude::*;
use gtk::{gio, glib, pango};

use crate::sys::{self, Choice};

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";

enum Value {
    Str(String),
    Int(i32),
    Bool(bool),
}

/// Where a setting lives: a GSettings key and a GTK `settings.ini` key (which
/// is also the name of the matching `GtkSettings` property).
struct Key {
    gsettings: &'static str,
    ini: &'static str,
}

const GTK_THEME: Key = Key {
    gsettings: "gtk-theme",
    ini: "gtk-theme-name",
};
const ICON_THEME: Key = Key {
    gsettings: "icon-theme",
    ini: "gtk-icon-theme-name",
};
const CURSOR_THEME: Key = Key {
    gsettings: "cursor-theme",
    ini: "gtk-cursor-theme-name",
};
const CURSOR_SIZE: Key = Key {
    gsettings: "cursor-size",
    ini: "gtk-cursor-theme-size",
};
const FONT: Key = Key {
    gsettings: "font-name",
    ini: "gtk-font-name",
};

#[derive(Clone)]
struct Store {
    gsettings: Option<(gio::Settings, gio::SettingsSchema)>,
}

impl Store {
    fn new() -> Self {
        let gsettings = gio::SettingsSchemaSource::default()
            .and_then(|src| src.lookup(INTERFACE_SCHEMA, true))
            .map(|schema| (gio::Settings::new(INTERFACE_SCHEMA), schema));
        Store { gsettings }
    }

    fn gsettings_key(&self, key: &str) -> Option<&gio::Settings> {
        let (settings, schema) = self.gsettings.as_ref()?;
        schema.has_key(key).then_some(settings)
    }

    fn color_scheme(&self) -> String {
        if let Some(s) = self.gsettings_key("color-scheme") {
            return s.string("color-scheme").to_string();
        }
        let dark = gtk::Settings::default()
            .map(|s| s.property::<bool>("gtk-application-prefer-dark-theme"))
            .unwrap_or(false);
        if dark { "prefer-dark" } else { "default" }.to_string()
    }

    fn set_color_scheme(&self, scheme: &str) -> Result<(), String> {
        if let Some(s) = self.gsettings_key("color-scheme") {
            s.set_string("color-scheme", scheme)
                .map_err(|e| e.to_string())?;
        }
        let dark = scheme == "prefer-dark";
        write_gtk_ini("gtk-application-prefer-dark-theme", &Value::Bool(dark))?;
        if let Some(gs) = gtk::Settings::default() {
            gs.set_property("gtk-application-prefer-dark-theme", dark);
        }
        Ok(())
    }

    fn set(&self, key: &Key, value: Value) -> Result<(), String> {
        if let Some(s) = self.gsettings_key(key.gsettings) {
            match &value {
                Value::Str(v) => s.set_string(key.gsettings, v),
                Value::Int(v) => s.set_int(key.gsettings, *v),
                Value::Bool(v) => s.set_boolean(key.gsettings, *v),
            }
            .map_err(|e| e.to_string())?;
        }
        write_gtk_ini(key.ini, &value)?;
        if let (true, Value::Str(theme)) = (key.ini == CURSOR_THEME.ini, &value) {
            // The XCursor fallback used by X11 and by many Wayland compositors.
            let path = glib::home_dir().join(".icons/default/index.theme");
            write_key_file(&path, "Icon Theme", "Inherits", &Value::Str(theme.clone()))?;
        }
        // Apply to this app immediately as a preview.
        if let Some(gs) = gtk::Settings::default() {
            match &value {
                Value::Str(v) => gs.set_property(key.ini, v),
                Value::Int(v) => gs.set_property(key.ini, v),
                Value::Bool(v) => gs.set_property(key.ini, v),
            }
        }
        Ok(())
    }
}

fn write_gtk_ini(key: &str, value: &Value) -> Result<(), String> {
    for dir in ["gtk-3.0", "gtk-4.0"] {
        write_key_file(
            &glib::user_config_dir().join(dir).join("settings.ini"),
            "Settings",
            key,
            value,
        )?;
    }
    Ok(())
}

fn write_key_file(path: &PathBuf, group: &str, key: &str, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let kf = glib::KeyFile::new();
    // A missing file is fine: it will be created.
    let _ = kf.load_from_file(path, glib::KeyFileFlags::KEEP_COMMENTS);
    match value {
        Value::Str(v) => kf.set_string(group, key, v),
        Value::Int(v) => kf.set_integer(group, key, *v),
        Value::Bool(v) => kf.set_boolean(group, key, *v),
    }
    kf.save_to_file(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn data_dirs(sub: &str) -> Vec<PathBuf> {
    let mut dirs = vec![
        glib::home_dir().join(format!(".{sub}")),
        glib::user_data_dir().join(sub),
    ];
    dirs.extend(glib::system_data_dirs().into_iter().map(|d| d.join(sub)));
    dirs
}

fn subdirs(dirs: &[PathBuf]) -> impl Iterator<Item = (String, PathBuf)> + '_ {
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
}

fn gtk_themes() -> Vec<String> {
    let mut names: BTreeSet<String> = ["Adwaita", "HighContrast"].map(String::from).into();
    for (name, path) in subdirs(&data_dirs("themes")) {
        if path.join("gtk-4.0").is_dir() || path.join("gtk-3.0").is_dir() {
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

/// Icon themes have `Directories=` in their index; cursor themes have `cursors/`.
fn icon_themes(cursors: bool) -> Vec<String> {
    let mut names = BTreeSet::new();
    for (name, path) in subdirs(&data_dirs("icons")) {
        if matches!(name.as_str(), "default" | "hicolor" | "locolor") {
            continue;
        }
        let ok = if cursors {
            path.join("cursors").is_dir()
        } else {
            let kf = glib::KeyFile::new();
            kf.load_from_file(path.join("index.theme"), glib::KeyFileFlags::NONE)
                .is_ok()
                && kf.has_key("Icon Theme", "Directories").unwrap_or(false)
        };
        if ok {
            names.insert(name);
        }
    }
    names.into_iter().collect()
}

fn current_str(prop: &str) -> String {
    gtk::Settings::default()
        .and_then(|s| s.property::<Option<String>>(prop))
        .unwrap_or_default()
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Appearance",
        "Theme, icons, cursor and font for applications. Some applications only pick up changes after they are restarted.",
    );
    let store = Store::new();

    // --- Style
    let list = sys::group(&page, "Style");

    let schemes = [
        ("default", "Default"),
        ("prefer-dark", "Prefer dark"),
        ("prefer-light", "Prefer light"),
    ]
    .map(|(v, l)| (v.to_string(), l.to_string()));
    let scheme = Choice::new(&schemes, &store.color_scheme());
    let s = store.clone();
    scheme.on_change(move |v| {
        let s = s.clone();
        async move { s.set_color_scheme(&v) }
    });
    sys::row(
        &list,
        "Color scheme",
        Some("Whether applications should use a light or dark style"),
        &scheme.dropdown,
    );

    let add_theme_row = |list: &gtk::ListBox,
                         title: &str,
                         subtitle: &str,
                         items: Vec<String>,
                         key: &'static Key| {
        let choice = Choice::new(&sys::same(&items), &current_str(key.ini));
        let s = store.clone();
        choice.on_change(move |v| {
            let s = s.clone();
            async move { s.set(key, Value::Str(v)) }
        });
        sys::row(list, title, Some(subtitle), &choice.dropdown);
    };
    add_theme_row(
        &list,
        "Application theme",
        "GTK theme",
        gtk_themes(),
        &GTK_THEME,
    );
    add_theme_row(
        &list,
        "Icons",
        "Icon theme",
        icon_themes(false),
        &ICON_THEME,
    );

    // --- Cursor
    let list = sys::group(&page, "Pointer");
    add_theme_row(
        &list,
        "Cursor theme",
        "Pointer appearance",
        icon_themes(true),
        &CURSOR_THEME,
    );

    let size = gtk::SpinButton::with_range(8.0, 256.0, 8.0);
    let current_size = gtk::Settings::default()
        .map(|s| s.property::<i32>("gtk-cursor-theme-size"))
        .filter(|&n| n > 0)
        .unwrap_or(24);
    size.set_value(current_size as f64);
    let s = store.clone();
    size.connect_value_changed(move |spin| {
        if let Err(e) = s.set(&CURSOR_SIZE, Value::Int(spin.value_as_int())) {
            sys::show_error(spin, "Could not change the cursor size", &e);
        }
    });
    sys::row(&list, "Cursor size", Some("In pixels"), &size);

    // --- Fonts
    let list = sys::group(&page, "Fonts");
    let font = gtk::FontDialogButton::new(Some(gtk::FontDialog::new()));
    let current_font = current_str(FONT.ini);
    if !current_font.is_empty() {
        font.set_font_desc(&pango::FontDescription::from_string(&current_font));
    }
    let s = store.clone();
    font.connect_font_desc_notify(move |btn| {
        if let Some(desc) = btn.font_desc()
            && let Err(e) = s.set(&FONT, Value::Str(desc.to_string()))
        {
            sys::show_error(btn, "Could not change the font", &e);
        }
    });
    sys::row(
        &list,
        "Interface font",
        Some("Default font for application text"),
        &font,
    );

    if store.gsettings.is_none() {
        page.append(&sys::dim_label(&format!(
            "The {INTERFACE_SCHEMA} settings schema is not installed, so only GTK settings files are updated."
        )));
    }

    page.upcast()
}
