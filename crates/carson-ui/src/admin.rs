use crate::api;
use crate::shell::{DragRail, DrawerBackdrop, MenuButton, sidebar_width};
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

fn item_name(item: &Value) -> String {
    item.get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
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

fn tool_checks(tools: RwSignal<Vec<Value>>, caps: RwSignal<Vec<String>>) -> impl IntoView {
    view! {
        <div class="field">
            <label>"Tools"</label>
            {move || {
                tools
                    .get()
                    .iter()
                    .map(|t| {
                        let owned = t.clone();
                        let name = item_name(&owned);
                        let label = name.clone();
                        let checked_caps = caps;
                        let checked_name = name.clone();
                        let change_caps = caps;
                        let change_name = name.clone();
                        view! {
                            <label class="check">
                                <input type="checkbox"
                                    prop:checked=move || checked_caps.get().contains(&checked_name)
                                    on:change=move |ev| {
                                        let checked = event_target_checked(&ev);
                                        let mut v = change_caps.get();
                                        if checked {
                                            v.push(change_name.clone());
                                        } else {
                                            v.retain(|n| n != &change_name);
                                        }
                                        change_caps.set(v);
                                    }/>
                                {label}
                            </label>
                        }
                        .into_any()
                    })
                    .collect::<Vec<_>>()
            }}
        </div>
    }
}

fn show_notice(notice: &RwSignal<Option<String>>, ok: bool, msg: String) {
    notice.set(Some(format!("{}: {msg}", if ok { "ok" } else { "error" })));
}

#[component]
pub fn AdminPage() -> impl IntoView {
    let tab = RwSignal::new("status".to_string());
    let drawer_open = RwSignal::new(false);
    let sidebar_w = sidebar_width();
    let pick_tab = move |name: &'static str| {
        drawer_open.set(false);
        tab.set(name.to_string());
    };
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
            <aside
                class="sidebar"
                class:open=drawer_open
                style:width=move || format!("{}px", sidebar_w.get())
            >
                <div class="brand-row">
                    <h1>"Carson"</h1>
                    <div class="sub">"Admin"</div>
                </div>
                <button class=tab_class("status") on:click=move |_| pick_tab("status")>"Status"</button>
                <button class=tab_class("providers") on:click=move |_| pick_tab("providers")>"Providers"</button>
                <button class=tab_class("agents") on:click=move |_| pick_tab("agents")>"Agents"</button>
                <button class=tab_class("tools") on:click=move |_| pick_tab("tools")>"Tools"</button>
                <a class="admin-link" href="/chat">"Back to chat"</a>
            </aside>

            <DragRail width=sidebar_w/>
            <MenuButton open=drawer_open/>

            <main class="main">
                {move || match tab.get().as_str() {
                    "providers" => view! { <ProvidersPanel/> }.into_any(),
                    "agents" => view! { <AgentsPanel/> }.into_any(),
                    "tools" => view! { <ToolsPanel/> }.into_any(),
                    _ => view! { <StatusPanel/> }.into_any(),
                }}
            </main>
            <DrawerBackdrop open=drawer_open/>
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
    let api_key = RwSignal::new(String::new());
    let notice = RwSignal::new(None::<String>);
    let editing = RwSignal::new(None::<Value>);
    let edit_base_url = RwSignal::new(String::new());
    let edit_api_key = RwSignal::new(String::new());

    spawn_local(async move {
        fetch_providers(&providers).await;
    });

    let create = move || {
        let n = name.get();
        let b = base_url.get();
        let k = api_key.get();
        if n.is_empty() || b.is_empty() {
            show_notice(&notice, false, "name and base_url are required".to_string());
            return;
        }
        let key = if k.is_empty() {
            Value::Null
        } else {
            Value::String(k)
        };
        let providers = providers;
        let notice = notice;
        spawn_local(async move {
            let (status, v) = api::post(
                "/api/providers",
                &json!({ "name": n, "base_url": b, "api_key": key }),
            )
            .await
            .unwrap_or((0, Value::Null));
            if status == 201 {
                name.set(String::new());
                base_url.set(String::new());
                api_key.set(String::new());
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

    let start_edit = move |item: Value| {
        edit_base_url.set(
            item.get("base_url")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
        );
        // Round-trip the stored key so editing the base_url keeps it; blanking
        // the field and confirming clears it intentionally.
        edit_api_key.set(
            item.get("api_key")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
        );
        editing.set(Some(item));
    };

    let cancel_edit = move || editing.set(None);

    let save_edit = move || {
        let Some(item) = editing.get() else { return };
        let n = item_name(&item);
        let b = edit_base_url.get();
        let k = edit_api_key.get();
        let key = if k.is_empty() {
            Value::Null
        } else {
            Value::String(k)
        };
        let providers = providers;
        let notice = notice;
        let editing = editing;
        spawn_local(async move {
            let path = format!("/api/providers/{n}");
            let (status, v) = api::put(&path, &json!({ "name": n, "base_url": b, "api_key": key }))
                .await
                .unwrap_or((0, Value::Null));
            if status == 200 {
                editing.set(None);
                show_notice(&notice, true, "provider updated".to_string());
                fetch_providers(&providers).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
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
                    <label>"API key (optional)"</label>
                    <input type="password" prop:value=move || api_key.get() on:input=move |ev| api_key.set(event_target_value(&ev)) placeholder="sk-…"/>
                </div>
                <div><button class="btn primary" on:click=move |_| create()>"Create"</button></div>
            </div>
            <div class="panel-grid">
                {move || {
                    providers
                        .get()
                        .iter()
                        .map(|p| {
                            let owned = p.clone();
                            let name = item_name(&owned);
                            let base = owned
                                .get("base_url")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let n2 = name.clone();
                            let edit_item = owned.clone();
                            let is_editing = editing
                                .get()
                                .as_ref()
                                .map(item_name)
                                == Some(name.clone());
                            if is_editing {
                                view! {
                                    <div class="card">
                                        <h3>{name}</h3>
                                        <div class="field">
                                            <label>"Base URL"</label>
                                            <input prop:value=move || edit_base_url.get() on:input=move |ev| edit_base_url.set(event_target_value(&ev))/>
                                        </div>
                                        <div class="field">
                                            <label>"API key (blank clears it)"</label>
                                            <input type="password" prop:value=move || edit_api_key.get() on:input=move |ev| edit_api_key.set(event_target_value(&ev)) placeholder="sk-…"/>
                                        </div>
                                        <div class="row">
                                            <button class="btn primary" on:click=move |_| save_edit()>"Confirm"</button>
                                            <button class="btn" on:click=move |_| cancel_edit()>"Cancel"</button>
                                        </div>
                                    </div>
                                }
                                .into_any()
                            } else {
                                view! {
                                    <div class="card">
                                        <div class="row">
                                            <h3>{name}</h3>
                                            <div class="row">
                                                <button class="btn" on:click=move |_| start_edit(edit_item.clone())>"Edit"</button>
                                                <button class="btn danger" on:click=move |_| remove(n2.clone())>"Delete"</button>
                                            </div>
                                        </div>
                                        <div class="muted">{base}</div>
                                    </div>
                                }
                                .into_any()
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
    let editing = RwSignal::new(None::<Value>);
    let edit_system_prompt = RwSignal::new(String::new());
    let edit_model = RwSignal::new(String::new());
    let edit_instances = RwSignal::new(String::new());
    let edit_max_history = RwSignal::new(String::new());
    let edit_context_window = RwSignal::new(String::new());
    let edit_compaction_ratio = RwSignal::new(String::new());
    let edit_auto_compact = RwSignal::new(false);
    let tools = RwSignal::new(Vec::<Value>::new());
    let create_caps = RwSignal::new(Vec::<String>::new());
    let edit_caps = RwSignal::new(Vec::<String>::new());

    spawn_local(async move {
        fetch_agents(&agents).await;
        fetch_tools(&tools).await;
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
                "name": k,
                "system_prompt": sp,
                "model": m,
                "instances": 1,
                "max_history": 20,
                "context_window": 4000,
                "compaction_ratio": 0.8,
                "auto_compact": false,
                "capabilities": create_caps.get(),
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

    let start_edit = move |item: Value| {
        let num = |key: &str, d: i64| {
            item.get(key)
                .and_then(|v| v.as_i64())
                .unwrap_or(d)
                .to_string()
        };
        edit_system_prompt.set(
            item.get("system_prompt")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
        );
        edit_model.set(
            item.get("model")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
        );
        edit_instances.set(num("instances", 1));
        edit_max_history.set(num("max_history", 20));
        edit_context_window.set(num("context_window", 4000));
        edit_compaction_ratio.set(
            item.get("compaction_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.8)
                .to_string(),
        );
        edit_auto_compact.set(
            item.get("auto_compact")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        edit_caps.set(
            item.get("capabilities")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        );
        editing.set(Some(item));
    };

    let cancel_edit = move || editing.set(None);

    let save_edit = move || {
        let Some(item) = editing.get() else { return };
        let k = item
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let agents = agents;
        let notice = notice;
        let editing = editing;
        spawn_local(async move {
            let body = json!({
                "name": k,
                "system_prompt": edit_system_prompt.get(),
                "model": edit_model.get(),
                "instances": edit_instances.get().parse::<usize>().unwrap_or(1),
                "max_history": edit_max_history.get().parse::<usize>().unwrap_or(20),
                "context_window": edit_context_window.get().parse::<usize>().unwrap_or(4000),
                "compaction_ratio": edit_compaction_ratio.get().parse::<f32>().unwrap_or(0.8),
                "auto_compact": edit_auto_compact.get(),
                "capabilities": edit_caps.get(),
            });
            let path = format!("/api/agents/{k}");
            let (status, v) = api::put(&path, &body).await.unwrap_or((0, Value::Null));
            if status == 200 {
                editing.set(None);
                show_notice(&notice, true, "agent updated".to_string());
                fetch_agents(&agents).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
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
                {tool_checks(tools, create_caps)}
                <div><button class="btn primary" on:click=move |_| create()>"Create"</button></div>
            </div>
            <div class="panel-grid">
                {move || {
                    agents
                        .get()
                        .iter()
                        .map(|a| {
                            let owned = a.clone();
                            let kind = owned
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let version = owned
                                .get("id")
                                .and_then(|n| n.as_str())
                                .map(|v| v.chars().take(8).collect::<String>())
                                .unwrap_or_default();
                            let model = owned
                                .get("model")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let instances = owned.get("instances").and_then(|n| n.as_u64()).unwrap_or(0);
                            let history = owned.get("max_history").and_then(|n| n.as_u64()).unwrap_or(0);
                            let auto = owned.get("auto_compact").and_then(|n| n.as_bool()).unwrap_or(false);
                            let caps = owned
                                .get("capabilities")
                                .and_then(|c| c.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            let k2 = kind.clone();
                            let edit_item = owned.clone();
                            let is_editing = editing
                                .get()
                                .and_then(|e| e.get("name").and_then(|x| x.as_str()).map(|s| s.to_string()))
                                == Some(kind.clone());
                            if is_editing {
                                view! {
                                    <div class="card">
                                        <h3>{kind}</h3>
                                        <div class="field">
                                            <label>"Model (provider/model)"</label>
                                            <input prop:value=move || edit_model.get() on:input=move |ev| edit_model.set(event_target_value(&ev))/>
                                        </div>
                                        <div class="field">
                                            <label>"System prompt"</label>
                                            <textarea prop:value=move || edit_system_prompt.get() on:input=move |ev| edit_system_prompt.set(event_target_value(&ev))></textarea>
                                        </div>
                                        <div class="row">
                                            <div class="field">
                                                <label>"Instances"</label>
                                                <input prop:value=move || edit_instances.get() on:input=move |ev| edit_instances.set(event_target_value(&ev))/>
                                            </div>
                                            <div class="field">
                                                <label>"Max history"</label>
                                                <input prop:value=move || edit_max_history.get() on:input=move |ev| edit_max_history.set(event_target_value(&ev))/>
                                            </div>
                                        </div>
                                        <div class="row">
                                            <div class="field">
                                                <label>"Context window"</label>
                                                <input prop:value=move || edit_context_window.get() on:input=move |ev| edit_context_window.set(event_target_value(&ev))/>
                                            </div>
                                            <div class="field">
                                                <label>"Compaction ratio"</label>
                                                <input prop:value=move || edit_compaction_ratio.get() on:input=move |ev| edit_compaction_ratio.set(event_target_value(&ev))/>
                                            </div>
                                        </div>
                                        <label class="check">
                                            <input type="checkbox" prop:checked=move || edit_auto_compact.get() on:change=move |ev| edit_auto_compact.set(event_target_checked(&ev))/>
                                            "Auto compact"
                                        </label>
                                        {tool_checks(tools, edit_caps)}
                                        <div class="row">
                                            <button class="btn primary" on:click=move |_| save_edit()>"Confirm"</button>
                                            <button class="btn" on:click=move |_| cancel_edit()>"Cancel"</button>
                                        </div>
                                    </div>
                                }
                                .into_any()
                            } else {
                                view! {
                                    <div class="card">
                                        <div class="row">
                                            <h3>{kind}</h3>
                                            <div class="row">
                                                <button class="btn" on:click=move |_| start_edit(edit_item.clone())>"Edit"</button>
                                                <button class="btn danger" on:click=move |_| remove(k2.clone())>"Delete"</button>
                                            </div>
                                        </div>
                                        <div class="muted">{model}</div>
                                        <div class="muted">{format!("version {version}")}</div>
                                        <div class="muted">{format!("instances {instances} · max_history {history} · auto_compact {auto}")}</div>
                                        <div class="muted">{format!("tools: {caps}")}</div>
                                    </div>
                                }
                                .into_any()
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
    let editing = RwSignal::new(None::<Value>);
    let edit_description = RwSignal::new(String::new());
    let edit_parameters = RwSignal::new(String::new());
    let edit_env = RwSignal::new(String::new());
    let wasm_mode = RwSignal::new("keep".to_string());
    let edit_file = RwSignal::new(None::<web_sys::File>);

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

    let on_edit_file = move |ev: web_sys::Event| {
        let input = ev
            .target()
            .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
            .expect("file input");
        edit_file.set(input.files().and_then(|f| f.get(0)));
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

    let start_edit = move |item: Value| {
        let params = item.get("parameters").cloned().unwrap_or(Value::Null);
        let env = item.get("env").cloned().unwrap_or(Value::Null);
        edit_description.set(
            item.get("description")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string(),
        );
        edit_parameters.set(serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_string()));
        edit_env.set(serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string()));
        wasm_mode.set("keep".to_string());
        edit_file.set(None);
        editing.set(Some(item));
    };

    let cancel_edit = move || editing.set(None);

    let save_edit = move || {
        let Some(item) = editing.get() else { return };
        let n = item_name(&item);
        let tools = tools;
        let notice = notice;
        let editing = editing;
        spawn_local(async move {
            let params: Value = match serde_json::from_str(&edit_parameters.get()) {
                Ok(v) => v,
                Err(e) => {
                    show_notice(&notice, false, format!("invalid parameters JSON: {e}"));
                    return;
                }
            };
            let env: Value = match serde_json::from_str(&edit_env.get()) {
                Ok(v) => v,
                Err(e) => {
                    show_notice(&notice, false, format!("invalid env JSON: {e}"));
                    return;
                }
            };
            let b64 = if wasm_mode.get() == "replace" {
                let Some(f) = edit_file.get() else {
                    show_notice(
                        &notice,
                        false,
                        "select a .wasm file to replace with".to_string(),
                    );
                    return;
                };
                match JsFuture::from(read_file_b64(f)).await {
                    Ok(v) => v.as_string().unwrap_or_default(),
                    Err(_) => {
                        show_notice(&notice, false, "failed to read wasm file".to_string());
                        return;
                    }
                }
            } else {
                item.get("wasm_b64")
                    .and_then(|w| w.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            if b64.is_empty() {
                show_notice(
                    &notice,
                    false,
                    "wasm is required (keep the original or replace it with a file)".to_string(),
                );
                return;
            }
            let body = json!({
                "name": n,
                "description": edit_description.get(),
                "parameters": params,
                "env": env,
                "wasm_b64": b64,
            });
            let path = format!("/api/tools/{n}");
            let (status, v) = api::put(&path, &body).await.unwrap_or((0, Value::Null));
            if status == 200 {
                editing.set(None);
                show_notice(&notice, true, "tool updated".to_string());
                fetch_tools(&tools).await;
            } else {
                show_notice(&notice, false, err_of(&v));
            }
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
                            let owned = t.clone();
                            let name = item_name(&owned);
                            let desc = owned
                                .get("description")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string();
                            let params = owned
                                .get("parameters")
                                .cloned()
                                .unwrap_or(Value::Null)
                                .to_string();
                            let n2 = name.clone();
                            let edit_item = owned.clone();
                            let builtin = name.starts_with("core/");
                            let is_editing = !builtin
                                && editing
                                    .get()
                                    .as_ref()
                                    .map(item_name)
                                    == Some(name.clone());
                            if is_editing {
                                view! {
                                    <div class="card">
                                        <h3>{name}</h3>
                                        <div class="field">
                                            <label>"Description"</label>
                                            <input prop:value=move || edit_description.get() on:input=move |ev| edit_description.set(event_target_value(&ev))/>
                                        </div>
                                        <div class="field">
                                            <label>"Parameters (JSON schema)"</label>
                                            <textarea prop:value=move || edit_parameters.get() on:input=move |ev| edit_parameters.set(event_target_value(&ev))></textarea>
                                        </div>
                                        <div class="field">
                                            <label>"Env (JSON)"</label>
                                            <textarea prop:value=move || edit_env.get() on:input=move |ev| edit_env.set(event_target_value(&ev))></textarea>
                                        </div>
                                        <div class="field">
                                            <label>"Wasm module"</label>
                                            <label class="check">
                                                <input type="radio" name="wasm-mode" prop:checked=move || wasm_mode.get() == "keep" on:change=move |_| wasm_mode.set("keep".to_string())/>
                                                "Keep original"
                                            </label>
                                            <label class="check">
                                                <input type="radio" name="wasm-mode" prop:checked=move || wasm_mode.get() == "replace" on:change=move |_| wasm_mode.set("replace".to_string())/>
                                                "Replace"
                                            </label>
                                            {move || {
                                                if wasm_mode.get() == "replace" {
                                                    view! { <input type="file" accept=".wasm" on:change=on_edit_file/> }.into_any()
                                                } else {
                                                    ().into_any()
                                                }
                                            }}
                                        </div>
                                        <div class="row">
                                            <button class="btn primary" on:click=move |_| save_edit()>"Confirm"</button>
                                            <button class="btn" on:click=move |_| cancel_edit()>"Cancel"</button>
                                        </div>
                                    </div>
                                }
                                .into_any()
                            } else {
                                view! {
                                    <div class="card">
                                        <div class="row">
                                            <div class="row">
                                                <h3>{name}</h3>
                                                {move || {
                                                    if builtin {
                                                        view! { <span class="badge">"built-in"</span> }.into_any()
                                                    } else {
                                                        ().into_any()
                                                    }
                                                }}
                                            </div>
                                            {move || {
                                                if builtin {
                                                    ().into_any()
                                                } else {
                                                    let edit = edit_item.clone();
                                                    let del = n2.clone();
                                                    view! {
                                                        <div class="row">
                                                            <button class="btn" on:click=move |_| start_edit(edit.clone())>"Edit"</button>
                                                            <button class="btn danger" on:click=move |_| remove(del.clone())>"Delete"</button>
                                                        </div>
                                                    }
                                                    .into_any()
                                                }
                                            }}
                                        </div>
                                        <div class="muted">{desc}</div>
                                        <pre>{params}</pre>
                                    </div>
                                }
                                .into_any()
                            }
                        })
                        .collect::<Vec<_>>()
                }}
            </div>
        </div>
    }
}
