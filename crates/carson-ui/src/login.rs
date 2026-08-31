//! Full-screen login gate. The token is POSTed to `/api/auth/login`; on
//! success the server sets an HttpOnly session cookie, so the raw token never
//! needs to live in page JS or localStorage.

use crate::api;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

#[component]
pub fn LoginPage() -> impl IntoView {
    let token = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);

    let submit = move || {
        let value = token.get().trim().to_string();
        if value.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post("/api/auth/login", &json!({"token": value})).await {
                Ok((200, _)) => crate::auth().set(Some(true)),
                Ok((429, _)) => error.set(Some("Too many attempts; try again later".into())),
                Ok((_, v)) => {
                    let message = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("Invalid token")
                        .to_string();
                    error.set(Some(message));
                }
                Err(e) => error.set(Some(api::err_text(e))),
            }
            busy.set(false);
        });
    };

    view! {
        <div class="login-page">
            <form
                class="login-card"
                on:submit=move |ev| {
                    ev.prevent_default();
                    submit();
                }
            >
                <h1>"Carson"</h1>
                <p class="login-sub">"Sign in with the API token from config.toml"</p>
                <input
                    name="token"
                    placeholder="API token"
                    type="password"
                    autocomplete="current-password"
                    prop:value=move || token.get()
                    on:input=move |ev| token.set(event_target_value(&ev))
                />
                <button class="btn primary" type="submit" disabled=move || busy.get()>
                    "Sign in"
                </button>
                {move || error.get().map(|e| view! { <div class="login-error">{e}</div> })}
            </form>
        </div>
    }
}