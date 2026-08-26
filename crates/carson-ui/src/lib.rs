use leptos::prelude::*;
use leptos_router::components::{Redirect, Route, Router, Routes};
use leptos_router::path;

pub mod admin;
pub mod api;
pub mod chat;
pub mod shell;
pub mod sse;
pub mod types;

/// Entry point invoked from `index.html` after the wasm module initializes.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
pub fn App() -> impl IntoView {
    shell::init_shell();
    view! {
        <Router>
            <Routes fallback=|| view! { <Redirect path="/chat"/> }>
                <Route path=path!("/") view=|| view! { <Redirect path="/chat"/> }/>
                <Route path=path!("/chat/:id?") view=chat::ChatPage/>
                <Route path=path!("/admin") view=admin::AdminPage/>
            </Routes>
        </Router>
    }
}
