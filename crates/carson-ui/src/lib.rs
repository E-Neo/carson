use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::{Redirect, Route, Router, Routes};
use leptos_router::path;
use std::sync::OnceLock;

pub mod admin;
pub mod api;
pub mod chat;
pub mod login;
pub mod shell;
pub mod sse;
pub mod types;

/// Root-owned authentication state shared by every page. `None` while probing
/// the server on first load, `Some(false)` logged out, `Some(true)` logged in.
static AUTH: OnceLock<RwSignal<Option<bool>>> = OnceLock::new();

/// The global authentication signal.
pub fn auth() -> RwSignal<Option<bool>> {
    *AUTH.get().expect("auth not initialised")
}

fn probe_auth() {
    let _ = AUTH.set(RwSignal::new(None));
    spawn_local(async move {
        // Same-origin fetch carries the HttpOnly session cookie automatically;
        // no token ever touches page JS.
        let ok = matches!(api::get("/api/auth/me").await, Ok((200, _)));
        auth().set(Some(ok));
    });
}

/// Sign out: revoke the session cookie and drop back to the login page.
pub async fn logout() {
    let _ = api::post("/api/auth/logout", &serde_json::json!({})).await;
    auth().set(Some(false));
}

/// Entry point invoked from `index.html` after the wasm module initializes.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}

#[component]
pub fn App() -> impl IntoView {
    shell::init_shell();
    probe_auth();
    view! {
        <Router>
            {move || match auth().get() {
                None => view! { <div class="boot"></div> }.into_any(),
                Some(false) => view! { <login::LoginPage/> }.into_any(),
                Some(true) => view! {
                    <Routes fallback=|| view! { <Redirect path="/chat"/> }>
                        <Route path=path!("/") view=|| view! { <Redirect path="/chat"/> }/>
                        <Route path=path!("/chat/:id?") view=chat::ChatPage/>
                        <Route path=path!("/admin") view=admin::AdminPage/>
                    </Routes>
                }
                    .into_any(),
            }}
        </Router>
    }
}
