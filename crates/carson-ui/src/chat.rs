use crate::api;
use crate::sse;
use crate::types::SessionSummary;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_navigate, use_params};
use leptos_router::params::Params;
use serde_json::{Value, json};

#[derive(Params, PartialEq, Debug, Clone)]
struct ChatParams {
    id: Option<u64>,
}

#[derive(Clone)]
enum UiMsg {
    User {
        content: String,
    },
    Assistant {
        content: RwSignal<String>,
        thinking: RwSignal<String>,
        tools: RwSignal<Vec<ToolCard>>,
    },
}

#[derive(Clone)]
struct MsgEntry {
    msg: UiMsg,
}

#[derive(Clone)]
struct ToolCard {
    id: String,
    name: String,
    arguments: String,
    result: Option<String>,
    is_error: bool,
}

fn fmt_usage(u: &Value) -> String {
    let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let cache_read = u
        .get("cache_read_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_creation = u
        .get("cache_creation_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    format!("in {input} | cache read {cache_read} + write {cache_creation} | out {output}")
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        s.to_string()
    } else {
        format!("{}...(truncated {})", &s[..limit], s.len())
    }
}

fn build_history(v: &Value) -> Vec<MsgEntry> {
    let mut out = Vec::new();
    let mut last_assistant: Option<(RwSignal<String>, RwSignal<Vec<ToolCard>>)> = None;
    if let Some(arr) = v.get("messages").and_then(|x| x.as_array()) {
        for m in arr {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
            match role {
                "user" => {
                    out.push(MsgEntry {
                        msg: UiMsg::User {
                            content: m
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string(),
                        },
                    });
                    last_assistant = None;
                }
                "assistant" => {
                    let content = RwSignal::new(
                        m.get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    let thinking = RwSignal::new(String::new());
                    let mut cards = Vec::new();
                    if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
                        for tc in calls {
                            cards.push(ToolCard {
                                id: tc
                                    .get("id")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                name: tc
                                    .get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: tc
                                    .get("arguments")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                result: None,
                                is_error: false,
                            });
                        }
                    }
                    let tools = RwSignal::new(cards);
                    out.push(MsgEntry {
                        msg: UiMsg::Assistant {
                            content,
                            thinking,
                            tools,
                        },
                    });
                    last_assistant = Some((content, tools));
                }
                "tool" => {
                    if let Some((_, tools)) = &last_assistant {
                        let tid = m
                            .get("tool_call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let result = m
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string();
                        let preview = truncate(&result, 500);
                        tools.update(|t| {
                            for card in t.iter_mut() {
                                if card.id == tid {
                                    card.result = Some(preview.clone());
                                }
                            }
                        });
                    }
                }
                _ => {}
            }
        }
    }
    out
}

async fn load_history(
    id: u64,
    messages: &RwSignal<Vec<MsgEntry>>,
    error: &RwSignal<Option<String>>,
) {
    if let Ok((status, v)) = api::get(&format!("/api/sessions/{id}")).await {
        if status == 200 {
            messages.set(build_history(&v));
        } else {
            error.set(Some(
                v.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("failed to load history")
                    .to_string(),
            ));
        }
    }
}

fn send(
    session_id: u64,
    input: RwSignal<String>,
    messages: RwSignal<Vec<MsgEntry>>,
    running: RwSignal<bool>,
    usage: RwSignal<Option<String>>,
    error: RwSignal<Option<String>>,
    status_line: RwSignal<Option<String>>,
) {
    let text = input.get().trim().to_string();
    if text.is_empty() || running.get() {
        return;
    }
    let content = RwSignal::new(String::new());
    let thinking = RwSignal::new(String::new());
    let tools = RwSignal::new(Vec::new());
    messages.update(|m| {
        m.push(MsgEntry {
            msg: UiMsg::User {
                content: text.clone(),
            },
        });
        m.push(MsgEntry {
            msg: UiMsg::Assistant {
                content,
                thinking,
                tools,
            },
        });
    });
    input.set(String::new());
    running.set(true);
    usage.set(None);
    error.set(None);
    status_line.set(None);

    let path = format!("/api/sessions/{session_id}/stream");
    let body = json!({ "content": text });
    spawn_local(async move {
        let result = sse::stream_post(&path, &body, move |ev| match ev.event.as_str() {
            "chunk" => {
                let text =
                    serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
                content.update(|s| s.push_str(&text));
            }
            "thinking" => {
                let text =
                    serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
                thinking.update(|s| s.push_str(&text));
            }
            "tool_use" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    tools.update(|t| {
                        t.push(ToolCard {
                            id: v
                                .get("id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            name: v
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: v
                                .get("arguments")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string(),
                            result: None,
                            is_error: false,
                        });
                    });
                }
            }
            "tool_result" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                    let tid = v
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let preview = v
                        .get("result_preview")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                    tools.update(|t| {
                        for card in t.iter_mut() {
                            if card.id == tid {
                                card.result = Some(preview.clone());
                                card.is_error = is_error;
                            }
                        }
                    });
                }
            }
            "status" => {
                let text =
                    serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
                status_line.set(Some(text));
            }
            "error" => {
                let msg = serde_json::from_str::<Value>(&ev.data)
                    .ok()
                    .and_then(|v| {
                        v.get("message")
                            .and_then(|m| m.as_str())
                            .map(|m| m.to_string())
                    })
                    .unwrap_or_else(|| ev.data.clone());
                error.set(Some(msg));
            }
            "done" => {
                if let Ok(v) = serde_json::from_str::<Value>(&ev.data)
                    && let Some(u) = v.get("usage")
                {
                    usage.set(Some(fmt_usage(u)));
                }
                running.set(false);
            }
            _ => {}
        })
        .await;
        running.set(false);
        if let Err(e) = result {
            error.set(Some(e));
        }
    });
}
fn render_msg(msg: UiMsg) -> AnyView {
    match msg {
        UiMsg::User { content } => view! { <div class="msg user">{content}</div> }.into_any(),
        UiMsg::Assistant {
            content,
            thinking,
            tools,
        } => view! {
                <div class="msg assistant">
                    {move || {
                        let t = thinking.get();
                        (!t.is_empty()).then(|| view! { <div class="thinking">{t}</div> })
                    }}
                    <div class="assistant-text">{move || content.get()}</div>
                    <For
                        each=move || tools.get()
                        key=|card: &ToolCard| card.id.clone()
                        children=move |card: ToolCard| {
                            view! {
                                <div class="tool-card-inline">
                                    <div class="tc-name">{card.name}</div>
                                    <div class="tc-args">{card.arguments}</div>
                                    {move || card.result.as_ref().map(|r| {
                                        let cls = if card.is_error { "tc-result tc-error" } else { "tc-result" };
                                        view! { <div class=cls>{r.clone()}</div> }
                                    })}
                                </div>
                            }
                        }
                    />
                </div>
            }
            .into_any(),
    }
}

#[component]
pub fn ChatPage() -> impl IntoView {
    let navigate = use_navigate();
    let params = use_params::<ChatParams>();
    let session_id = Memo::new(move |_| params.get().ok().and_then(|p| p.id));

    let sessions = RwSignal::new(Vec::<SessionSummary>::new());
    let agents = RwSignal::new(Vec::<Value>::new());
    let messages = RwSignal::new(Vec::<MsgEntry>::new());
    let input = RwSignal::new(String::new());
    let running = RwSignal::new(false);
    let usage = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let status_line = RwSignal::new(None::<String>);
    let active = RwSignal::new(None::<u64>);
    let selected_agent = RwSignal::new(String::new());

    spawn_local(async move {
        if let Ok((_, v)) = api::get("/api/sessions").await
            && let Some(list) = v.get("sessions").and_then(|x| x.as_array())
        {
            sessions.set(
                list.iter()
                    .filter_map(|s| serde_json::from_value::<SessionSummary>(s.clone()).ok())
                    .collect(),
            );
        }
    });
    spawn_local(async move {
        if let Ok((_, v)) = api::get("/api/agents").await
            && let Some(list) = v.get("agents").and_then(|x| x.as_array())
        {
            agents.set(list.clone());
        }
    });

    Effect::new(move |_| {
        if let Some(id) = session_id.get() {
            active.set(Some(id));
            running.set(false);
            usage.set(None);
            error.set(None);
            status_line.set(None);
            messages.set(Vec::new());
            spawn_local(async move {
                load_history(id, &messages, &error).await;
            });
        } else {
            active.set(None);
            running.set(false);
            messages.set(Vec::new());
            usage.set(None);
            error.set(None);
        }
    });

    Effect::new(move |_| {
        let agents = agents.get();
        if !agents.is_empty()
            && selected_agent.get().is_empty()
            && let Some(kind) = agents.first().and_then(|a| a["kind"].as_str())
        {
            selected_agent.set(kind.to_string());
        }
    });

    let go_to = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if let Some(target) = go_to.get() {
            navigate(&target, Default::default());
            untrack(|| go_to.set(None));
        }
    });

    let do_send = move || {
        if let Some(id) = active.get() {
            send(id, input, messages, running, usage, error, status_line);
        }
    };

    let create_session = move || {
        let agent = selected_agent.get();
        if agent.is_empty() {
            return;
        }
        spawn_local(async move {
            if let Ok((status, v)) = api::post("/api/sessions", &json!({ "agent": agent })).await
                && status == 201
                && let Some(id) = v.get("session_id").and_then(|x| x.as_u64())
            {
                go_to.set(Some(format!("/chat/{id}")));
            }
        });
    };

    let delete_session = move |id: u64| {
        let active = active.get();
        spawn_local(async move {
            let _ = api::delete(&format!("/api/sessions/{id}")).await;
            if let Ok((_, v)) = api::get("/api/sessions").await
                && let Some(list) = v.get("sessions").and_then(|x| x.as_array())
            {
                sessions.set(
                    list.iter()
                        .filter_map(|s| serde_json::from_value::<SessionSummary>(s.clone()).ok())
                        .collect(),
                );
            }
            if Some(id) == active {
                go_to.set(Some("/chat".to_string()));
            }
        });
    };

    let select_session = move |id: u64| go_to.set(Some(format!("/chat/{id}")));

    let stop_session = move || {
        if let Some(id) = active.get() {
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/stop"), &json!({})).await;
            });
        }
    };

    let reset_session = move || {
        if let Some(id) = active.get() {
            let messages = messages;
            let error = error;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/reset"), &json!({})).await;
                load_history(id, &messages, &error).await;
            });
        }
    };

    let compact_session = move || {
        if let Some(id) = active.get() {
            let messages = messages;
            let error = error;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/compact"), &json!({})).await;
                load_history(id, &messages, &error).await;
            });
        }
    };

    let new_chat = move || go_to.set(Some("/chat".to_string()));

    let active_agent = Memo::new(move |_| {
        sessions
            .get()
            .iter()
            .find(|s| Some(s.id) == active.get())
            .map(|s| s.agent.clone())
            .unwrap_or_default()
    });

    view! {
        <div class="app">
            <aside class="sidebar">
                <div class="brand-row">
                    <h1>"carson"</h1>
                    <div class="sub">"wasm agent host"</div>
                </div>
                <button class="btn primary" on:click=move |_| new_chat()>"+ New chat"</button>
                <div class="session-list">
                    <For
                        each=move || sessions.get()
                        key=|s: &SessionSummary| s.id
                        children=move |session: SessionSummary| {
                            let active_class = move || {
                                if active.get() == Some(session.id) {
                                    "session-item active"
                                } else {
                                    "session-item"
                                }
                            };
                            view! {
                                <div class=active_class>
                                    <button class="name" on:click=move |_| select_session(session.id)>
                                        {format!("#{} · {}", session.id, session.agent)}
                                    </button>
                                    <button
                                        class="del"
                                        title="Delete session"
                                        on:click=move |_| delete_session(session.id)
                                    >
                                        "x"
                                    </button>
                                </div>
                            }
                        }
                    />
                </div>
                <a class="admin-link" href="/admin">"Admin"</a>
            </aside>

            <main class="main">
                {move || {
                    if active.get().is_some() {
                        view! {
                            <div class="toolbar">
                                <div class="title">
                                    {move || {
                                        if active_agent.get().is_empty() {
                                            format!("Session #{}", active.get().unwrap_or(0))
                                        } else {
                                            format!("Session #{} · {}", active.get().unwrap_or(0), active_agent.get())
                                        }
                                    }}
                                </div>
                                <button class="btn" disabled=move || !running.get() on:click=move |_| stop_session()>
                                    "Stop"
                                </button>
                                <button class="btn" on:click=move |_| reset_session()>"Reset"</button>
                                <button class="btn" on:click=move |_| compact_session()>"Compact"</button>
                                <button class="btn danger" on:click=move |_| {
                                    if let Some(id) = active.get() {
                                        delete_session(id);
                                    }
                                }>"Delete"</button>
                            </div>
                            <div class="messages">
                                {move || {
                                    let entries = messages.get();
                                    let mut views = Vec::new();
                                    for e in entries {
                                        views.push(render_msg(e.msg).into_any());
                                    }
                                    views
                                }}
                            </div>
                            <div class="composer">
                                <textarea
                                    prop:value=move || input.get()
                                    on:input=move |ev| input.set(event_target_value(&ev))
                                    placeholder="Type a message, Enter to send"
                                    on:keydown=move |ev| {
                                        let key = ev.key();
                                        if key == "Enter" && !ev.shift_key() {
                                            ev.prevent_default();
                                            do_send();
                                        }
                                    }
                                ></textarea>
                                <button class="btn primary" disabled=move || running.get() on:click=move |_| do_send()>
                                    "Send"
                                </button>
                            </div>
                        }
                            .into_any()
                    } else {
                        view! {
                            <div class="new-chat">
                                <h2>"Start a new chat"</h2>
                                {move || {
                                    if agents.get().is_empty() {
                                        view! { <div class="hint">"No agents configured. Add one in Admin."</div> }.into_any()
                                    } else {
                                        view! {
                                            <div class="field">
                                                <label>"Agent"</label>
                                                <select
                                                    prop:value=move || selected_agent.get()
                                                    on:change=move |ev| selected_agent.set(event_target_value(&ev))
                                                >
                                                    {move || {
                                                        let kinds: Vec<String> = agents
                                                            .get()
                                                            .iter()
                                                            .filter_map(|a| {
                                                                a.get("kind")
                                                                    .and_then(|k| k.as_str())
                                                                    .map(|k| k.to_string())
                                                            })
                                                            .collect();
                                                        kinds
                                                            .into_iter()
                                                            .map(|kind| {
                                                                let v = kind.clone();
                                                                view! { <option value=v>{kind}</option> }
                                                            })
                                                            .collect::<Vec<_>>()
                                                    }}
                                                </select>
                                            </div>
                                            <button class="btn primary" on:click=move |_| create_session()>
                                                "Start"
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                            </div>
                        }
                            .into_any()
                    }
                }}
            </main>
        </div>
    }
}
