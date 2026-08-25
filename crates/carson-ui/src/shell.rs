//! Shared page-shell helpers: resizable sidebar width and mobile drawer bits.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::api::window;

const MIN_WIDTH: f64 = 180.0;
const MAX_WIDTH: f64 = 480.0;
const DEFAULT_WIDTH: f64 = 260.0;
const STORAGE_KEY: &str = "carson.sidebar.width";

fn load_sidebar_width() -> f64 {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(STORAGE_KEY).ok())
        .flatten()
        .and_then(|value| value.parse::<f64>().ok())
        .map(|width| width.clamp(MIN_WIDTH, MAX_WIDTH))
        .unwrap_or(DEFAULT_WIDTH)
}

fn save_sidebar_width(width: f64) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = storage.set_item(STORAGE_KEY, &format!("{width}"));
    }
}

/// The sidebar's persisted width signal.
pub fn sidebar_width() -> RwSignal<f64> {
    RwSignal::new(load_sidebar_width())
}

/// A drag rail rendered between the sidebar and main content. Dragging resizes
/// the sidebar within clamped bounds and persists the result.
#[component]
pub fn DragRail(width: RwSignal<f64>) -> impl IntoView {
    let dragging = RwSignal::new(false);
    let dragging_for_move = dragging;
    let on_move = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        if !dragging_for_move.get_untracked() {
            return;
        }
        let next = (ev.client_x() as f64).clamp(MIN_WIDTH, MAX_WIDTH);
        width.set(next);
        save_sidebar_width(next);
    });
    let on_up = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |_: web_sys::MouseEvent| {
        dragging.set(false);
    });
    // Page-lifetime listeners: cheap no-ops unless a drag is in progress.
    let win = window();
    let _ = win.add_event_listener_with_callback("mousemove", on_move.as_ref().unchecked_ref());
    let _ = win.add_event_listener_with_callback("mouseup", on_up.as_ref().unchecked_ref());
    on_move.forget();
    on_up.forget();

    view! { <div class="drag-rail" on:mousedown=move |_| dragging.set(true)></div> }
}

/// Floating hamburger button that toggles the mobile drawer.
#[component]
pub fn MenuButton(open: RwSignal<bool>) -> impl IntoView {
    view! {
        <button class="menu-fab" aria-label="Toggle menu" on:click=move |_| open.update(|o| *o = !*o)>
            "☰"
        </button>
    }
}

/// Dimmed backdrop behind the open mobile drawer; tapping it closes.
#[component]
pub fn DrawerBackdrop(open: RwSignal<bool>) -> impl IntoView {
    view! {
        <Show when=move || open.get()>
            <div class="backdrop" on:click=move |_| open.set(false)></div>
        </Show>
    }
}
