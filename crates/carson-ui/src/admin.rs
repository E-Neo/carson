use crate::api;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::HtmlInputElement;

fn read_file_b64(file: web_sys::File) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, reject| {
        let reader = web_sys::FileReader::new().expect("FileReader");
        let reader2 = reader.clone();
        let closure =
            Closure::<dyn FnMut(web_sys::ProgressEvent)>::wrap(Box::new(move |_ev| match reader2
                .result()
            {
                Ok(val) => {
                    let text = val.as_string().unwrap_or_default();
                    let b64 = text.split(',').nth(1).unwrap_or("").to_string();
                    let _ = resolve.call1(&JsValue::UNDEFINED, &JsValue::from_str(&b64));
                }
                Err(e) => {
                    let _ = reject.call1(&JsValue::UNDEFINED, &e);
                }
            }));
        reader.set_onload(Some(closure.as_ref().unchecked_ref()));
        let _ = reader.read_as_data_url(&file);
        closure.forget();
    })
}

fn err_of(v: &Value) -> String {
    v.get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("request failed")
        .to_string()
}

async fn fetch_providers(providers: &RwSignal<Vec<Value>>) {
    if let Ok((_, v)) = api::get("/api/providers").await
        && let Some(list) = v.get("providers").and_then(|x| x.as_array())
    {
        providers.set(list.clone());
    }
}

async fn fetch_agents(agents: &RwSignal<Vec<Value>>) {
    if let Ok((_, v)) = api::get("/api/agents").await
        && let Some(list) = v.get("agents").and_then(|x| x.as_array())
    {
        agents.set(list.clone());
    }
}

async fn fetch_tools(tools: &RwSignal<Vec<Value>>) {
    if let Ok((_, v)) = api::get("/api/tools").await
        && let Some(list) = v.get("tools").and_then(|x| x.as_array())
    {
        tools.set(list.clone());
    }
}

fn show_notice(notice: &RwSignal<Option<String>>, ok: bool, msg: String) {
    notice.set(Some(format!("{}: {msg}", if ok { "ok" } else { "error" })));
}

#[component]
pub fn AdminPage() -> impl IntoView {
    let tab = RwSignal::new("status".to_string());
    let tab_class = move |name: &'static str| {
        move || {
            if tab.get() == name {
                "tab active"
            } else {
                "tab"
            }
        }
    };

    view! {
        <div class="app">
            <aside class="sidebar">
                <div class="brand-row">
                    <h1>"carson"</h1>
                    <div class="sub">"admin"</div>
                </div>
                <button class=tab_class("status") on:click=move |_| tab.set("status".to_string())>"Status"</button>
                <button class=tab_class("providers") on:click=move |_| tab.set("providers".to_string())>"Providers"</button>
                <button class=tab_class("agents") on:click=move |_| tab.set("agents".to_string())>"Agents"</button>
                <button class=tab_class("tools") on:click=move |_| tab.set("tools".to_string())>"Tools"</button>
                <a class="admin-link" href="/chat">"Back to chat"</a>
            </aside>
            <main class="main">
                {move || match tab.get().as_str() {
                    "providers" => view! { <ProvidersPanel/> }.into_any(),
                    "agents" => view! { <AgentsPanel/> }.into_any(),
                    "tools" => view! { <ToolsPanel/> }.into_any(),
                    _ => view! { <StatusPanel/> }.into_any(),
                }}
            </main>
        </div>
    }
}

#[component]
fn StatusPanel() -> impl IntoView {
    let status = RwSignal::new(None::<Value>);
    let health = RwSignal::new(String::new());
    Effect::new(move |_| {
        let status = status;
        let health = health;
        spawn_local(async move {
            if let Ok((_, v)) = api::get("/api/status").await {
                status.set(Some(v));
            }
            if let Ok((_, v)) = api::get("/api/health").await {
                health.set(
                    v.get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("?")
                        .to_string(),
                );
            }
        });
    });

    view! {
        <div class="admin">
            <h2>"Status"</h2>
            {move || {
                match status.get() {
                    Some(s) => view! {
                        <div class="card">
                            <div class="row"><span class="muted">"health"</span><span class="status-ok">{health.get()}</span></div>
                            <div class="row"><span class="muted">"bind"</span><span>{s.get("bind").and_then(|b| b.as_str()).unwrap_or("")}</span></div>
                            <div class="row"><span class="muted">"agents"</span><span>{s.get("agent_count").and_then(|c| c.as_u64()).unwrap_or(0)}</span></div>
                            <div class="row"><span class="muted">"sessions"</span><span>{s.get("session_count").and_then(|c| c.as_u64()).unwrap_or(0)}</span></div>
                            <div class="row"><span class="muted">"providers"</span><span>{s.get("provider_count").and_then(|c| c.as_u64()).unwrap_or(0)}</span></div>
                            <div class="row"><span class="muted">"tools"</span><span>{s.get("tool_count").and_then(|c| c.as_u64()).unwrap_or(0)}</span></div>
                        </div>
                    }
                        .into_any(),
                    None => view! { <div class="hint">"loading…"</div> }.into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn ProvidersPanel() -> impl IntoView {
    let providers = RwSignal::new(Vec::<Value>::new());
    let name = RwSignal::new(String::new());
    let base_url = RwSignal::new(String::new());
    let api_key_env = RwSignal::new(String::new());
    let notice = RwSignal::new(None::<String>);

    spawn_local(async move {
        fetch_providers(&providers).await;
    });

    let create = move || {
        let n = name.get();
        let b = base_url.get();
        let k = api_key_env.get();
        if n.is_empty() || b.is_empty() {
            show_notice(&notice, false, "name and base_url are required".to_string());
            return;
        }
        let key_env = if k.is_empty() {
            Value::Null
        } else {
            Value::String(k)
        };
        let providers = providers;
        let notice = notice;
        spawn_local(async move {
            let (status, v) = api::post(
                "/api/providers",
                &json!({ "name": n, "base_url": b, "api_key_env": key_env }),
            )
            .await
            .unwrap_or((0, Value::Null));
            if status == 201 {
                name.set(String::new());
                base_url.set(String::new());
                api_key_env.set(String::new());
                show_notice(&notice, true, "provider created".to_string());
                fetch_providers(&providers).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
        });
    };

    let remove = move |name: String| {
        let providers = providers;
        let notice = notice;
        spawn_local(async move {
            let _ = api::delete(&format!("/api/providers/{name}")).await;
            show_notice(&notice, true, format!("deleted {name}"));
            fetch_providers(&providers).await;
        });
    };

    view! {
        <div class="admin">
            <h2>"Providers"</h2>
            {move || {
                notice
                    .get()
                    .map(|m| view! { <div class="error">{m}</div> })
            }}
            <div class="card">
                <h3>"Add provider"</h3>
                <div class="field">
                    <label>"Name"</label>
                    <input prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) placeholder="groq"/>
                </div>
                <div class="field">
                    <label>"Base URL"</label>
                    <input prop:value=move || base_url.get() on:input=move |ev| base_url.set(event_target_value(&ev)) placeholder="https://api.groq.com/openai/v1"/>
                </div>
                <div class="field">
                    <label>"API key env var (optional)"</label>
                    <input prop:value=move || api_key_env.get() on:input=move |ev| api_key_env.set(event_target_value(&ev)) placeholder="GROQ_API_KEY"/>
                </div>
                <div><button class="btn primary" on:click=move |_| create()>"Create"</button></div>
            </div>
            <div class="panel-grid">
                {move || {
                    providers
                        .get()
                        .iter()
                        .map(|p| {
                            let name = p.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let base = p.get("base_url").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let env = p.get("api_key_env").and_then(|n| n.as_str()).unwrap_or("-").to_string();
                            let n2 = name.clone();
                            view! {
                                <div class="card">
                                    <div class="row">
                                        <h3>{name}</h3>
                                        <button class="btn danger" on:click=move |_| remove(n2.clone())>"Delete"</button>
                                    </div>
                                    <div class="muted">{base}</div>
                                    <div class="muted">{"env: "}{env}</div>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                }}
            </div>
        </div>
    }
}

#[component]
fn AgentsPanel() -> impl IntoView {
    let agents = RwSignal::new(Vec::<Value>::new());
    let kind = RwSignal::new(String::new());
    let system_prompt = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let notice = RwSignal::new(None::<String>);

    spawn_local(async move {
        fetch_agents(&agents).await;
    });

    let create = move || {
        let k = kind.get();
        let sp = system_prompt.get();
        let m = model.get();
        if k.is_empty() || m.is_empty() {
            show_notice(&notice, false, "kind and model are required".to_string());
            return;
        }
        let agents = agents;
        let notice = notice;
        spawn_local(async move {
            let body = json!({
                "kind": k,
                "system_prompt": sp,
                "model": m,
                "instances": 1,
                "max_history": 20,
                "context_window": 4000,
                "compaction_ratio": 0.8,
                "auto_compact": false,
                "capabilities": [],
            });
            let (status, v) = api::post("/api/agents", &body)
                .await
                .unwrap_or((0, Value::Null));
            if status == 201 {
                kind.set(String::new());
                system_prompt.set(String::new());
                model.set(String::new());
                show_notice(&notice, true, "agent created".to_string());
                fetch_agents(&agents).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
        });
    };

    let remove = move |kind: String| {
        let agents = agents;
        let notice = notice;
        spawn_local(async move {
            let _ = api::delete(&format!("/api/agents/{kind}")).await;
            show_notice(&notice, true, format!("deleted {kind}"));
            fetch_agents(&agents).await;
        });
    };

    view! {
        <div class="admin">
            <h2>"Agents"</h2>
            {move || {
                notice
                    .get()
                    .map(|m| view! { <div class="error">{m}</div> })
            }}
            <div class="card">
                <h3>"Add agent"</h3>
                <div class="field">
                    <label>"Kind"</label>
                    <input prop:value=move || kind.get() on:input=move |ev| kind.set(event_target_value(&ev)) placeholder="assistant"/>
                </div>
                <div class="field">
                    <label>"Model (provider/model)"</label>
                    <input prop:value=move || model.get() on:input=move |ev| model.set(event_target_value(&ev)) placeholder="groq/llama-3.3-70b-versatile"/>
                </div>
                <div class="field">
                    <label>"System prompt"</label>
                    <textarea prop:value=move || system_prompt.get() on:input=move |ev| system_prompt.set(event_target_value(&ev))></textarea>
                </div>
                <div><button class="btn primary" on:click=move |_| create()>"Create"</button></div>
            </div>
            <div class="panel-grid">
                {move || {
                    agents
                        .get()
                        .iter()
                        .map(|a| {
                            let kind = a.get("kind").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let model = a.get("model").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let instances = a.get("instances").and_then(|n| n.as_u64()).unwrap_or(0);
                            let history = a.get("max_history").and_then(|n| n.as_u64()).unwrap_or(0);
                            let auto = a.get("auto_compact").and_then(|n| n.as_bool()).unwrap_or(false);
                            let k2 = kind.clone();
                            view! {
                                <div class="card">
                                    <div class="row">
                                        <h3>{kind}</h3>
                                        <button class="btn danger" on:click=move |_| remove(k2.clone())>"Delete"</button>
                                    </div>
                                    <div class="muted">{model}</div>
                                    <div class="muted">{format!("instances {instances} · max_history {history} · auto_compact {auto}")}</div>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                }}
            </div>
        </div>
    }
}

#[component]
fn ToolsPanel() -> impl IntoView {
    let tools = RwSignal::new(Vec::<Value>::new());
    let name = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let file = RwSignal::new(None::<web_sys::File>);
    let notice = RwSignal::new(None::<String>);

    spawn_local(async move {
        fetch_tools(&tools).await;
    });

    let on_file = move |ev: web_sys::Event| {
        let input = ev
            .target()
            .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
            .expect("file input");
        file.set(input.files().and_then(|f| f.get(0)));
    };

    let create = move || {
        let n = name.get();
        let d = description.get();
        let picked = file.get();
        let tools = tools;
        let notice = notice;
        spawn_local(async move {
            let Some(f) = picked else {
                show_notice(&notice, false, "select a .wasm file".to_string());
                return;
            };
            let b64 = match JsFuture::from(read_file_b64(f)).await {
                Ok(v) => v.as_string().unwrap_or_default(),
                Err(_) => {
                    show_notice(&notice, false, "failed to read wasm file".to_string());
                    return;
                }
            };
            if b64.is_empty() {
                show_notice(&notice, false, "wasm file was empty".to_string());
                return;
            }
            let body = json!({
                "name": n,
                "description": d,
                "parameters": {},
                "env": {},
                "wasm_b64": b64,
            });
            let (status, v) = api::post("/api/tools", &body)
                .await
                .unwrap_or((0, Value::Null));
            if status == 201 {
                name.set(String::new());
                description.set(String::new());
                file.set(None);
                show_notice(&notice, true, "tool created".to_string());
                fetch_tools(&tools).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
        });
    };

    let remove = move |name: String| {
        let tools = tools;
        let notice = notice;
        spawn_local(async move {
            let _ = api::delete(&format!("/api/tools/{name}")).await;
            show_notice(&notice, true, format!("deleted {name}"));
            fetch_tools(&tools).await;
        });
    };

    view! {
        <div class="admin">
            <h2>"Tools"</h2>
            {move || {
                notice
                    .get()
                    .map(|m| view! { <div class="error">{m}</div> })
            }}
            <div class="card">
                <h3>"Add custom tool"</h3>
                <div class="field">
                    <label>"Name (must start with custom/)"</label>
                    <input prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) placeholder="custom/my-tool"/>
                </div>
                <div class="field">
                    <label>"Description"</label>
                    <input prop:value=move || description.get() on:input=move |ev| description.set(event_target_value(&ev)) placeholder="what the tool does"/>
                </div>
                <div class="field">
                    <label>"Wasm module"</label>
                    <input type="file" accept=".wasm" on:change=on_file/>
                </div>
                <div><button class="btn primary" on:click=move |_| create()>"Upload"</button></div>
            </div>
            <div class="panel-grid">
                {move || {
                    tools
                        .get()
                        .iter()
                        .map(|t| {
                            let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let desc = t.get("description").and_then(|n| n.as_str()).unwrap_or("").to_string();
                            let params = t.get("parameters").cloned().unwrap_or(Value::Null).to_string();
                            let n2 = name.clone();
                            view! {
                                <div class="card">
                                    <div class="row">
                                        <h3>{name}</h3>
                                        <button class="btn danger" on:click=move |_| remove(n2.clone())>"Delete"</button>
                                    </div>
                                    <div class="muted">{desc}</div>
                                    <pre>{params}</pre>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>()
                }}
            </div>
        </div>
    }
}
