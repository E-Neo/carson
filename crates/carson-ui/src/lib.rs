use leptos::prelude::*;

pub mod api;
pub mod sse;

/// Entry point invoked from `index.html` after the wasm module initializes.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(|| {
        view! {
            <div class="app">
                <div class="sidebar">
                    <h1>"carson"</h1>
                    <div class="sub">"wasm agent host"</div>
                </div>
                <div class="main">
                    <div class="empty">"web UI bootstrap complete"</div>
                </div>
            </div>
        }
    });
}
