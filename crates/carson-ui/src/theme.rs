//! Theme state: light (the default) or dark, persisted server-side in the
//! DB via `/api/config` and applied as a `data-theme` attribute on `<html>`
//! so the CSS variables swap. The palette lives entirely in `style.css`
//! under `:root` / `[data-theme]`.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;
use std::sync::OnceLock;

use crate::api;
use crate::api::window;

const DEFAULT_THEME: &str = "light";
const DARK_THEME: &str = "dark";

static THEME: OnceLock<RwSignal<String>> = OnceLock::new();

fn theme() -> RwSignal<String> {
    *THEME.get().expect("theme not initialised")
}

/// Seed the theme with the light default and apply it before the app mounts.
/// The persisted value is fetched from the server once authenticated.
pub fn init_theme() {
    let _ = THEME.set(RwSignal::new(DEFAULT_THEME.to_string()));
    apply(DEFAULT_THEME);
}

/// Fetch the persisted theme from `/api/config` and apply it. Ignored when
/// the server has no theme or the request is rejected (not authenticated).
pub async fn load_from_server() {
    if let Ok((200, v)) = api::get("/api/config").await
        && let Some(theme) = v.get("theme").and_then(|t| t.as_str())
        && (theme == DEFAULT_THEME || theme == DARK_THEME)
    {
        apply(theme);
    }
}

/// Apply `target` to the document root and update the signal.
fn apply(target: &str) {
    if let Some(element) = window().document().and_then(|d| d.document_element()) {
        let _ = element.set_attribute("data-theme", target);
    }
    theme().set(target.to_string());
}

/// Persist `target` to the server (best effort; ignored if unauthenticated).
fn persist(target: &str) {
    let value = target.to_string();
    spawn_local(async move {
        let _ = api::put("/api/config", &json!({ "theme": value })).await;
    });
}

fn toggle() {
    let next = if theme().get() == DARK_THEME {
        DEFAULT_THEME
    } else {
        DARK_THEME
    };
    apply(next);
    persist(next);
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
