//! Shared page-shell helpers: resizable sidebar width and mobile drawer bits.
//!
//! The sidebar machinery lives entirely at the app root: the width signal is
//! created in `init_shell()` (called from the root component, whose scope
//! never disposes) and the window listeners are installed exactly once.
//! Per-page closures would outlive their signals after route navigation and
//! trap on every mousemove.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::api::window;

const MIN_WIDTH: f64 = 180.0;
const MAX_WIDTH: f64 = 480.0;
const DEFAULT_WIDTH: f64 = 260.0;
const STORAGE_KEY: &str = "carson.sidebar.width";

static SIDEBAR_WIDTH: OnceLock<RwSignal<f64>> = OnceLock::new();
static DRAGGING: AtomicBool = AtomicBool::new(false);

/// Initialise the root-owned shell state. Call once from `App`.
pub fn init_shell() {
    let _ = SIDEBAR_WIDTH.set(RwSignal::new(load_sidebar_width()));
    install_sidebar_resize();
}

/// The sidebar's persisted width signal.
pub fn sidebar_width() -> RwSignal<f64> {
    *SIDEBAR_WIDTH.get().expect("shell not initialised")
}

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

fn clamp_width(px: f64) -> f64 {
    px.clamp(MIN_WIDTH, MAX_WIDTH)
}

fn install_sidebar_resize() {
    let on_move = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
        if !DRAGGING.load(Ordering::SeqCst) {
            return;
        }
        if let Some(width) = SIDEBAR_WIDTH.get() {
            let next = clamp_width(ev.client_x() as f64);
            width.set(next);
            save_sidebar_width(next);
        }
    });
    let on_up = Closure::<dyn Fn(web_sys::MouseEvent)>::new(|_: web_sys::MouseEvent| {
        DRAGGING.store(false, Ordering::SeqCst);
    });
    // Root-owned listeners: the app scope lives for as long as the page does,
    // so forgetting them is safe.
    let win = window();
    let _ = win.add_event_listener_with_callback("mousemove", on_move.as_ref().unchecked_ref());
    let _ = win.add_event_listener_with_callback("mouseup", on_up.as_ref().unchecked_ref());
    on_move.forget();
    on_up.forget();
}

/// Start a sidebar drag; the root listeners take it from here.
pub fn begin_drag() {
    DRAGGING.store(true, Ordering::SeqCst);
}

/// A drag rail rendered between the sidebar and main content. Dragging
/// resizes the global sidebar width within clamped bounds and persists it.
#[component]
pub fn DragRail() -> impl IntoView {
    view! { <div class="drag-rail" on:mousedown=move |_| begin_drag()></div> }
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
