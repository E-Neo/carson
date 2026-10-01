//! Theme state: light (the default) or dark, persisted in localStorage and
//! applied as a `data-theme` attribute on `<html>` so the CSS variables swap.
//! The palette lives entirely in `style.css` under `:root` / `[data-theme]`.

use leptos::prelude::*;
use std::sync::OnceLock;

use crate::api::window;

const THEME_KEY: &str = "carson.theme";
const DEFAULT_THEME: &str = "light";
const DARK_THEME: &str = "dark";

static THEME: OnceLock<RwSignal<String>> = OnceLock::new();

fn theme() -> RwSignal<String> {
    *THEME.get().expect("theme not initialised")
}

/// Read the persisted theme (defaulting to light) and apply it to `<html>`.
/// Called before the app mounts so the first paint uses the saved theme.
pub fn init_theme() {
    let stored = window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(THEME_KEY).ok())
        .flatten();
    let theme = if stored.as_deref() == Some(DARK_THEME) {
        DARK_THEME
    } else {
        DEFAULT_THEME
    };
    let _ = THEME.set(RwSignal::new(theme.to_string()));
    apply(theme);
}

/// Apply `target` to the document root and persist the choice.
pub fn apply(target: &str) {
    if let Some(element) = window().document().and_then(|d| d.document_element()) {
        let _ = element.set_attribute("data-theme", target);
    }
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = storage.set_item(THEME_KEY, target);
    }
    theme().set(target.to_string());
}

fn toggle() {
    let next = if theme().get() == DARK_THEME {
        DEFAULT_THEME
    } else {
        DARK_THEME
    };
    apply(next);
}

/// A compact sun/moon button that flips between light and dark. The icon
/// shows the *target* theme (moon while light, sun while dark).
#[component]
pub fn ThemeToggle() -> impl IntoView {
    let icon = move || {
        if theme().get() == DARK_THEME {
            "☀"
        } else {
            "☾"
        }
    };
    let hint = move || {
        if theme().get() == DARK_THEME {
            "Switch to light mode"
        } else {
            "Switch to dark mode"
        }
    };
    view! {
        <button class="theme-toggle" title=hint on:click=move |_| toggle()>
            {icon}
        </button>
    }
}
