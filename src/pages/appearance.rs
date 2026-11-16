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
        if matches!(&value, Value::Str(v) if v.is_empty()) {
            return self.unset(key);
        }
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

    /// The value written for a setting by the user (in GSettings, the GTK
    /// `settings.ini` files or, for the cursor, `~/.icons/default`), or empty
    /// if none is. Unlike the effective value in `GtkSettings`, this tells
    /// "(not set)" apart from a theme that happens to be the default.
    fn configured(&self, key: &Key) -> String {
        let mut found = Vec::new();
        if let Some(s) = self.gsettings_key(key.gsettings)
            && s.user_value(key.gsettings).is_some()
        {
            found.push(s.string(key.gsettings).to_string());
        }
        for dir in ["gtk-4.0", "gtk-3.0"] {
            let path = glib::user_config_dir().join(dir).join("settings.ini");
            found.extend(read_key_file(&path, "Settings", key.ini));
        }
        if key.ini == CURSOR_THEME.ini {
            let path = glib::home_dir().join(".icons/default/index.theme");
            found.extend(read_key_file(&path, "Icon Theme", "Inherits"));
        }
        found
            .into_iter()
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    }

    /// Undoes a setting ("(not set)" in a drop-down): resets the GSettings key
    /// to its default and removes the key from the files, instead of writing
    /// an empty value. An empty cursor theme in particular hides the system's
    /// default theme, so named cursors such as the link pointer go missing.
    fn unset(&self, key: &Key) -> Result<(), String> {
        if let Some(s) = self.gsettings_key(key.gsettings) {
            s.reset(key.gsettings);
        }
        for dir in ["gtk-3.0", "gtk-4.0"] {
            remove_key_file(
                &glib::user_config_dir().join(dir).join("settings.ini"),
                "Settings",
                key.ini,
            )?;
        }
        if key.ini == CURSOR_THEME.ini {
            let dir = glib::home_dir().join(".icons/default");
            remove_key_file(&dir.join("index.theme"), "Icon Theme", "Inherits")?;
            // Only goes if empty, i.e. it held nothing but that file.
            let _ = std::fs::remove_dir(&dir);
        }
        if let Some(gs) = gtk::Settings::default() {
            gs.reset_property(key.ini);
        }
        Ok(())
    }
}

fn read_key_file(path: &PathBuf, group: &str, key: &str) -> Option<String> {
    let kf = glib::KeyFile::new();
    kf.load_from_file(path, glib::KeyFileFlags::NONE).ok()?;
    kf.string(group, key).ok().map(|s| s.to_string())
}

/// Removes a key from a key file, and the group if that leaves it empty. A
/// file left with nothing in it is deleted; a missing file is left alone.
fn remove_key_file(path: &PathBuf, group: &str, key: &str) -> Result<(), String> {
    let kf = glib::KeyFile::new();
    if kf
        .load_from_file(path, glib::KeyFileFlags::KEEP_COMMENTS)
        .is_err()
        || !kf.has_key(group, key).unwrap_or(false)
    {
        return Ok(());
    }
    let _ = kf.remove_key(group, key);
    if kf.keys(group).map(|k| k.is_empty()).unwrap_or(false) {
        let _ = kf.remove_group(group);
    }
    if kf.groups().is_empty() && kf.to_data().trim().is_empty() {
        return std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()));
    }
    kf.save_to_file(path)
        .map_err(|e| format!("{}: {e}", path.display()))
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
        // "(not set)" is always offered, so a theme can be undone.
        let mut items = sys::same(&items);
        items.insert(0, (String::new(), "(not set)".to_string()));
        let choice = Choice::new(&items, &store.configured(key));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("settingsgoo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn unset_after_set_removes_created_file() {
        let dir = temp_dir("created");
        let path = dir.join("index.theme");
        write_key_file(
            &path,
            "Icon Theme",
            "Inherits",
            &Value::Str("Adwaita".into()),
        )
        .unwrap();
        assert!(path.exists());
        remove_key_file(&path, "Icon Theme", "Inherits").unwrap();
        assert!(!path.exists());
        // Missing file: nothing to do.
        remove_key_file(&path, "Icon Theme", "Inherits").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unset_keeps_other_keys() {
        let dir = temp_dir("kept");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.ini");
        std::fs::write(&path, "# mine\n[Settings]\ngtk-theme-name=Foo\n").unwrap();
        write_key_file(
            &path,
            "Settings",
            "gtk-cursor-theme-name",
            &Value::Str("Adwaita".into()),
        )
        .unwrap();
        remove_key_file(&path, "Settings", "gtk-cursor-theme-name").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# mine"));
        assert!(text.contains("gtk-theme-name=Foo"));
        assert!(!text.contains("cursor"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
