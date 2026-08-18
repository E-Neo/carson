use leptos::prelude::*;

/// Admin screen (providers / agents / tools). Full implementation lands in Phase 2.
#[component]
pub fn AdminPage() -> impl IntoView {
    view! {
        <div class="admin">
            <h2>"Admin"</h2>
            <div class="hint">"Phase 2: manage providers, agents and custom tools."</div>
            <div>
                <a href="/chat">"Back to chat"</a>
            </div>
        </div>
    }
}
