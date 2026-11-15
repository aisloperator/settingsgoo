//! Region & Keyboard, via systemd-localed (`org.freedesktop.locale1`).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::sys::{self, Choice};

const SERVICE: &str = "org.freedesktop.locale1";
const XKB_RULES: [&str; 2] = [
    "/usr/share/X11/xkb/rules/evdev.lst",
    "/usr/share/X11/xkb/rules/base.lst",
];

/// The keyboard layouts and variants known to XKB.
#[derive(Default)]
struct Xkb {
    layouts: Vec<(String, String)>,
    /// (layout, variant, description)
    variants: Vec<(String, String, String)>,
}

impl Xkb {
    fn load() -> Self {
        let Some(text) = XKB_RULES
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())
        else {
            return Xkb::default();
        };
        let mut xkb = Xkb::default();
        let mut section = "";
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("! ") {
                section = if name.trim() == "layout" {
                    "layout"
                } else if name.trim() == "variant" {
                    "variant"
                } else {
                    ""
                };
                continue;
            }
            let line = line.trim();
            let Some((name, desc)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let desc = desc.trim();
            match section {
                "layout" => xkb.layouts.push((name.to_string(), desc.to_string())),
                "variant" => {
                    if let Some((layout, desc)) = desc.split_once(": ") {
                        xkb.variants
                            .push((layout.to_string(), name.to_string(), desc.to_string()));
                    }
                }
                _ => {}
            }
        }
        xkb.layouts.sort_by(|a, b| a.1.cmp(&b.1));
        xkb
    }

    fn variants_of(&self, layout: &str) -> Vec<(String, String)> {
        let mut v = vec![(String::new(), "Default".to_string())];
        v.extend(
            self.variants
                .iter()
                .filter(|(l, _, _)| l == layout)
                .map(|(_, name, desc)| (name.clone(), desc.clone())),
        );
        v
    }
}

/// The system keyboard configuration. Layouts and variants are
/// comma-separated lists; this page edits the first (primary) entry and keeps
/// the rest, along with the model and options.
#[derive(Clone, Default)]
struct Keyboard {
    layouts: Vec<String>,
    variants: Vec<String>,
    model: String,
    options: String,
}

impl Keyboard {
    fn with_primary(&self, layout: &str, variant: &str) -> Keyboard {
        let mut k = self.clone();
        if k.layouts.is_empty() {
            k.layouts.push(String::new());
        }
        k.variants.resize(k.layouts.len(), String::new());
        k.layouts[0] = layout.to_string();
        k.variants[0] = variant.to_string();
        k
    }

    fn primary(&self) -> (String, String) {
        let first = |v: &[String]| v.first().cloned().unwrap_or_default();
        (first(&self.layouts), first(&self.variants))
    }
}

fn split(s: Option<String>) -> Vec<String> {
    s.filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default()
}

async fn set_keyboard(proxy: &gtk::gio::DBusProxy, k: &Keyboard) -> Result<(), String> {
    let args = (
        k.layouts.join(","),
        k.model.as_str(),
        k.variants.join(","),
        k.options.as_str(),
        false, // don't convert to a console keymap
        true,  // interactive
    );
    sys::call(proxy, "SetX11Keyboard", args.to_variant())
        .await
        .map(drop)
}

/// Compares locale names loosely: "en_US.UTF-8" matches "en_US.utf8".
fn normalize_locale(s: &str) -> String {
    s.to_lowercase().replace("utf-8", "utf8")
}

pub fn build() -> gtk::Widget {
    let page = sys::page(
        "Region & Keyboard",
        "System-wide language and keyboard layout. Changing them may ask for an administrator password; a new language takes effect at the next login.",
    );
    let Some(proxy) = sys::system_proxy(SERVICE) else {
        page.append(&sys::dim_label(
            "The locale service (systemd-localed) is not available.",
        ));
        return page.upcast();
    };

    // --- Language
    let list = sys::group(&page, "Language");
    let locale = sys::prop_strv(&proxy, "Locale").unwrap_or_default();
    let lang = locale
        .iter()
        .find_map(|l| l.strip_prefix("LANG="))
        .unwrap_or("")
        .to_string();
    let mut available: Vec<String> = sys::run("locale", &["-a"])
        .map(|out| {
            out.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    available.sort();
    // Show the current value using its system spelling.
    let selected = available
        .iter()
        .find(|a| normalize_locale(a) == normalize_locale(&lang))
        .cloned()
        .unwrap_or(lang);
    let lang_choice = Choice::new(&sys::same(&available), &selected);
    {
        let (proxy, locale) = (proxy.clone(), Rc::new(RefCell::new(locale)));
        lang_choice.on_change(move |new| {
            let (proxy, locale) = (proxy.clone(), locale.clone());
            async move {
                // Replace LANG and keep any other LC_* settings.
                let mut next: Vec<String> = locale
                    .borrow()
                    .iter()
                    .filter(|l| !l.starts_with("LANG="))
                    .cloned()
                    .collect();
                next.insert(0, format!("LANG={new}"));
                sys::call(&proxy, "SetLocale", (next.clone(), true).to_variant()).await?;
                *locale.borrow_mut() = next;
                Ok(())
            }
        });
    }
    sys::row(
        &list,
        "Language",
        Some("System locale (LANG)"),
        &lang_choice.dropdown,
    );

    // --- Keyboard
    let list = sys::group(&page, "Keyboard");
    let xkb = Rc::new(Xkb::load());
    let keyboard = Rc::new(RefCell::new(Keyboard {
        layouts: split(sys::prop_str(&proxy, "X11Layout")),
        variants: split(sys::prop_str(&proxy, "X11Variant")),
        model: sys::prop_str(&proxy, "X11Model").unwrap_or_default(),
        options: sys::prop_str(&proxy, "X11Options").unwrap_or_default(),
    }));
    let (layout, variant) = keyboard.borrow().primary();

    let layout_choice = Choice::new(&xkb.layouts, &layout);
    let variant_choice = Choice::new(&xkb.variants_of(&layout), &variant);
    {
        let (proxy, keyboard, xkb, variant_choice) = (
            proxy.clone(),
            keyboard.clone(),
            xkb.clone(),
            variant_choice.clone(),
        );
        layout_choice.on_change(move |layout| {
            let (proxy, keyboard, xkb, variant_choice) = (
                proxy.clone(),
                keyboard.clone(),
                xkb.clone(),
                variant_choice.clone(),
            );
            async move {
                let next = keyboard.borrow().with_primary(&layout, "");
                set_keyboard(&proxy, &next).await?;
                *keyboard.borrow_mut() = next;
                variant_choice.set_items(&xkb.variants_of(&layout), "");
                Ok(())
            }
        });
    }
    {
        let keyboard = keyboard.clone();
        variant_choice.on_change(move |variant| {
            let (proxy, keyboard) = (proxy.clone(), keyboard.clone());
            async move {
                let (layout, _) = keyboard.borrow().primary();
                let next = keyboard.borrow().with_primary(&layout, &variant);
                set_keyboard(&proxy, &next).await?;
                *keyboard.borrow_mut() = next;
                Ok(())
            }
        });
    }
    sys::row(
        &list,
        "Layout",
        Some("Primary keyboard layout"),
        &layout_choice.dropdown,
    );
    sys::row(&list, "Variant", None, &variant_choice.dropdown);

    let extra = keyboard.borrow().layouts.len().saturating_sub(1);
    let mut note = String::from(
        "This is the system default XKB configuration. It is used by X11, and by Wayland compositors that read the system keyboard settings; some compositors have their own keyboard configuration instead.",
    );
    if extra > 0 {
        note.push_str(&format!(
            " {extra} additional layout(s) are configured and are kept unchanged."
        ));
    }
    page.append(&sys::dim_label(&note));

    page.upcast()
}
