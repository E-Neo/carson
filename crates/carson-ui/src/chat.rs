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
    id: Option<String>,
}

/// One rendered conversation block, in chronological order. Streaming appends
/// new blocks as they arrive so thinking, text and tool cards interleave
/// exactly like the persisted log.
#[derive(Clone)]
enum UiBlock {
    User { content: String },
    Thinking { text: RwSignal<String> },
    Text { text: RwSignal<String> },
    ToolUse { card: RwSignal<ToolCard> },
}

#[derive(Clone)]
struct MsgEntry {
    block: UiBlock,
}

#[derive(Clone, Default)]
struct ToolCard {
    id: String,
    name: String,
    arguments: String,
    result: Option<String>,
    is_error: bool,
}

/// True while the last block in the log is still being streamed into.
fn last_is_kind(messages: &RwSignal<Vec<MsgEntry>>, kind_is_text: bool) -> bool {
    messages.with(|m| match m.last().map(|e| &e.block) {
        Some(UiBlock::Text { .. }) => kind_is_text,
        Some(UiBlock::Thinking { .. }) => !kind_is_text,
        _ => false,
    })
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
        format!("{}…", &s[..s.floor_char_boundary(limit)])
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Rebuild the block list from the session API's ordered block log.
fn build_history(v: &Value) -> Vec<MsgEntry> {
    let mut out: Vec<MsgEntry> = Vec::new();
    let Some(arr) = v.get("messages").and_then(|x| x.as_array()) else {
        return out;
    };
    for m in arr {
        let kind = m.get("kind").and_then(|k| k.as_str()).unwrap_or("");
        match kind {
            "user" => out.push(MsgEntry {
                block: UiBlock::User {
                    content: m
                        .get("text")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
            }),
            "thinking" => out.push(MsgEntry {
                block: UiBlock::Thinking {
                    text: RwSignal::new(
                        m.get("text")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                },
            }),
            "text" => out.push(MsgEntry {
                block: UiBlock::Text {
                    text: RwSignal::new(
                        m.get("text")
                            .and_then(|c| c.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                },
            }),
            "tool-use" => out.push(MsgEntry {
                block: UiBlock::ToolUse {
                    card: RwSignal::new(ToolCard {
                        id: m
                            .get("tool_call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: m
                            .get("tool_name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        arguments: m
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        result: None,
                        is_error: false,
                    }),
                },
            }),
            "tool-result" => {
                let tid = m
                    .get("tool_call_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let result = m.get("text").and_then(|c| c.as_str()).unwrap_or("");
                let preview = truncate(result, 500);
                let is_error = m.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                for e in out.iter_mut().rev() {
                    if let UiBlock::ToolUse { card } = &e.block
                        && card.get_untracked().id == tid
                    {
                        card.update(|c| {
                            c.result = Some(preview.clone());
                            c.is_error = is_error;
                        });
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

async fn load_history(
    id: &str,
    messages: &RwSignal<Vec<MsgEntry>>,
    error: &RwSignal<Option<String>>,
    scroll_tick: &RwSignal<u64>,
    at_bottom: &RwSignal<bool>,
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
    // Fresh content lands at the bottom; resume auto-scroll there.
    at_bottom.set(true);
    scroll_tick.update(|t| *t += 1);
}

#[allow(clippy::too_many_arguments)]
fn send(
    session_id: String,
    input: RwSignal<String>,
    messages: RwSignal<Vec<MsgEntry>>,
    running: RwSignal<bool>,
    usage: RwSignal<Option<String>>,
    error: RwSignal<Option<String>>,
    status_line: RwSignal<Option<String>>,
    scroll_tick: RwSignal<u64>,
) {
    let text = input.get().trim().to_string();
    if text.is_empty() || running.get() {
        return;
    }
    let content = RwSignal::new(String::new());
    messages.update(|m| {
        m.push(MsgEntry {
            block: UiBlock::User {
                content: text.clone(),
            },
        });
        m.push(MsgEntry {
            block: UiBlock::Text { text: content },
        });
    });
    input.set(String::new());
    running.set(true);
    usage.set(None);
    error.set(None);
    status_line.set(None);
    scroll_tick.update(|t| *t += 1);

    let path = format!("/api/sessions/{session_id}/stream");
    let body = json!({ "content": text });
    spawn_local(async move {
        let result = sse::stream_post(&path, &body, move |ev| {
            scroll_tick.update(|t| *t += 1);
            match ev.event.as_str() {
                "chunk" => {
                    let text = serde_json::from_str::<String>(&ev.data)
                        .unwrap_or_else(|_| ev.data.clone());
                    ensure_text_block(&messages);
                    append_last_text(&messages, &text);
                }
                "thinking" => {
                    let text = serde_json::from_str::<String>(&ev.data)
                        .unwrap_or_else(|_| ev.data.clone());
                    ensure_thinking_block(&messages);
                    append_last_thinking(&messages, &text);
                }
                "tool_use" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                        messages.update(|m| {
                            m.push(MsgEntry {
                                block: UiBlock::ToolUse {
                                    card: RwSignal::new(ToolCard {
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
                                        ..Default::default()
                                    }),
                                },
                            });
                        });
                    }
                }
                "tool_args" => {
                    if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                        let tid = v
                            .get("id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args = v
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        for_each_card(&messages, &tid, &|card| {
                            card.arguments = args.clone();
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
                        let preview = truncate(
                            v.get("result_preview")
                                .and_then(|x| x.as_str())
                                .unwrap_or(""),
                            500,
                        );
                        let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                        for_each_card(&messages, &tid, &|card| {
                            card.result = Some(preview.clone());
                            card.is_error = is_error;
                        });
                    }
                }
                "status" => {
                    let text = serde_json::from_str::<String>(&ev.data)
                        .unwrap_or_else(|_| ev.data.clone());
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
            }
        })
        .await;
        running.set(false);
        if let Err(e) = result {
            error.set(Some(e));
        }
    });
}

fn ensure_text_block(messages: &RwSignal<Vec<MsgEntry>>) {
    if !last_is_kind(messages, true) {
        messages.update(|m| {
            m.push(MsgEntry {
                block: UiBlock::Text {
                    text: RwSignal::new(String::new()),
                },
            });
        });
    }
}

fn ensure_thinking_block(messages: &RwSignal<Vec<MsgEntry>>) {
    let empty = messages.get_untracked().last().map(|e| &e.block).is_none();
    if last_is_kind(messages, true) || empty {
        messages.update(|m| {
            m.push(MsgEntry {
                block: UiBlock::Thinking {
                    text: RwSignal::new(String::new()),
                },
            });
        });
    }
}

fn append_last_text(messages: &RwSignal<Vec<MsgEntry>>, text: &str) {
    messages.update(|m| {
        if let Some(UiBlock::Text { text: buf }) = m.last_mut().map(|e| &mut e.block) {
            buf.update(|s| s.push_str(text));
        }
    });
}

fn append_last_thinking(messages: &RwSignal<Vec<MsgEntry>>, text: &str) {
    messages.update(|m| {
        if let Some(UiBlock::Thinking { text: buf }) = m.last_mut().map(|e| &mut e.block) {
            buf.update(|s| s.push_str(text));
        }
    });
}

fn for_each_card(messages: &RwSignal<Vec<MsgEntry>>, id: &str, f: &impl Fn(&mut ToolCard)) {
    messages.update(|m| {
        for e in m.iter().rev() {
            if let UiBlock::ToolUse { card } = &e.block {
                let mut hit = false;
                card.update(|c| {
                    if c.id == id {
                        f(c);
                        hit = true;
                    }
                });
                if hit {
                    break;
                }
            }
        }
    });
}

fn block_child(block: &UiBlock) -> AnyView {
    match block {
        UiBlock::Thinking { text } => {
            let t = *text;
            view! { <div class="thinking">{move || (!t.get().is_empty()).then(|| t.get())}</div> }
                .into_any()
        }
        UiBlock::Text { text } => view! { <MarkdownText text=*text/> }.into_any(),
        UiBlock::ToolUse { card } => {
            let c = *card;
            let name = move || c.get().name.clone();
            let args = move || {
                let a = c.get().arguments.clone();
                (!a.is_empty()).then_some(a)
            };
            let result = move || {
                let state = c.get();
                state.result.map(|r| (r, state.is_error))
            };
            view! {
                <div class="tool-card-inline">
                    <div class="tc-name">{name}</div>
                    {move || args().map(|a| view! { <div class="tc-args">{a}</div> })}
                    {move || {
                        result().map(|(r, is_err)| {
                            let cls = if is_err { "tc-result tc-error" } else { "tc-result" };
                            view! { <div class=cls>{r}</div> }
                        })
                    }}
                </div>
            }
            .into_any()
        }
        UiBlock::User { content } => {
            view! { <div class="msg user">{content.clone()}</div> }.into_any()
        }
    }
}

/// Consecutive assistant-side blocks share one chat bubble, keeping the
/// original order (thinking → text → tool cards → more text …).
fn assistant_group(run: Vec<MsgEntry>) -> AnyView {
    let children: Vec<AnyView> = run.iter().map(|e| block_child(&e.block)).collect();
    view! { <div class="msg assistant">{children}</div> }.into_any()
}

#[component]
fn MarkdownText(text: RwSignal<String>) -> impl IntoView {
    let node = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if let Some(el) = node.get() {
            el.set_inner_html(&render_md(&text.get()));
        }
    });
    view! { <div class="assistant-text" node_ref=node></div> }
}

fn render_md(text: &str) -> String {
    use pulldown_cmark::{Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, Parser::new_ext(text, opts));
    sanitize_hrefs(&html)
}

fn sanitize_hrefs(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(rel) = rest.find("href=\"") {
        out.push_str(&rest[..rel]);
        rest = &rest[rel + "href=\"".len()..];
        let end = rest.find('"').map(|i| i + 1).unwrap_or(rest.len());
        let (url, after) = rest.split_at(end);
        let inner = url.strip_prefix('"').unwrap_or(url);
        let blocked = inner.to_ascii_lowercase().starts_with("javascript:")
            || inner.to_ascii_lowercase().starts_with("data:");
        out.push_str(if blocked { "#\"" } else { url });
        rest = after;
    }
    out.push_str(rest);
    out
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
    let active = RwSignal::new(None::<String>);
    let selected_agent = RwSignal::new(String::new());

    // Auto-scroll bookkeeping: follow the stream only while the user is at
    // (or near) the bottom of the buffer.
    let messages_el = NodeRef::<leptos::html::Div>::new();
    let at_bottom = RwSignal::new(true);
    let scroll_tick = RwSignal::new(0u64);

    Effect::new(move |_| {
        scroll_tick.get();
        messages.get();
        if !at_bottom.get() {
            return;
        }
        if let Some(el) = messages_el.get() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    spawn_local(refresh_sessions_async(sessions));
    spawn_local(async move {
        if let Ok((_, v)) = api::get("/api/agents").await
            && let Some(list) = v.get("agents").and_then(|x| x.as_array())
        {
            agents.set(list.clone());
        }
    });

    Effect::new(move |_| {
        if let Some(id) = session_id.get() {
            active.set(Some(id.clone()));
            running.set(false);
            usage.set(None);
            error.set(None);
            status_line.set(None);
            messages.set(Vec::new());
            let messages = messages;
            let error = error;
            let scroll_tick = scroll_tick;
            let at_bottom = at_bottom;
            spawn_local(async move {
                load_history(&id, &messages, &error, &scroll_tick, &at_bottom).await;
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
            && let Some(name) = agents.first().and_then(|a| a["name"].as_str())
        {
            selected_agent.set(name.to_string());
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
            send(
                id,
                input,
                messages,
                running,
                usage,
                error,
                status_line,
                scroll_tick,
            );
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
                && let Some(id) = v.get("session_id").and_then(|x| x.as_str())
            {
                go_to.set(Some(format!("/chat/{id}")));
            }
        });
    };

    let delete_session = move |id: String| {
        let active = active.get();
        spawn_local(async move {
            let _ = api::delete(&format!("/api/sessions/{id}")).await;
            refresh_sessions_async(sessions).await;
            if Some(&id) == active.as_ref() {
                go_to.set(Some("/chat".to_string()));
            }
        });
    };

    let select_session = move |id: String| go_to.set(Some(format!("/chat/{id}")));

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
            let scroll_tick = scroll_tick;
            let at_bottom = at_bottom;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/reset"), &json!({})).await;
                load_history(&id, &messages, &error, &scroll_tick, &at_bottom).await;
            });
        }
    };

    let compact_session = move || {
        if let Some(id) = active.get() {
            let messages = messages;
            let error = error;
            let scroll_tick = scroll_tick;
            let at_bottom = at_bottom;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/compact"), &json!({})).await;
                load_history(&id, &messages, &error, &scroll_tick, &at_bottom).await;
            });
        }
    };

    let migrate_session = move || {
        if let Some(id) = active.get() {
            let messages = messages;
            let error = error;
            let scroll_tick = scroll_tick;
            let at_bottom = at_bottom;
            spawn_local(async move {
                let (_, v) = api::post(&format!("/api/sessions/{id}/migrate"), &json!({}))
                    .await
                    .unwrap_or((0, Value::Null));
                if v.get("error").is_some() {
                    error.set(v.get("error").and_then(|e| e.as_str()).map(String::from));
                }
                refresh_sessions_async(sessions).await;
                load_history(&id, &messages, &error, &scroll_tick, &at_bottom).await;
            });
        }
    };

    let new_chat = move || go_to.set(Some("/chat".to_string()));

    let active_agent = Memo::new(move |_| {
        sessions
            .get()
            .iter()
            .find(|s| Some(&s.id) == active.get().as_ref())
            .map(|s| s.agent.clone())
            .unwrap_or_default()
    });
    let _ = &session_id;

    view! {
        <div class="app">
            <aside class="sidebar">
                <div class="brand-row">
                    <h1>"Carson"</h1>
                    <div class="sub">"Chat"</div>
                </div>
                <button class="btn primary" on:click=move |_| new_chat()>"+ New chat"</button>
                <div class="session-list">
                    <For
                        each=move || sessions.get()
                        key=|s: &SessionSummary| s.id.clone()
                        children=move |session: SessionSummary| {
                            let sid = session.id.clone();
                            let active_class = move || {
                                if active.get().as_ref() == Some(&sid) {
                                    "session-item active"
                                } else {
                                    "session-item"
                                }
                            };
                            let label = format!("{} · {}", short_id(&session.id), session.agent);
                            let id_del = session.id.clone();
                            let id_sel = session.id.clone();
                            view! {
                                <div class=active_class>
                                    <button class="name" on:click=move |_| select_session(id_sel.clone())>
                                        {label}
                                    </button>
                                    <button
                                        class="del"
                                        title="Delete session"
                                        on:click=move |_| delete_session(id_del.clone())
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
                                        let id = active.get().map(|i| short_id(&i)).unwrap_or_default();
                                        if active_agent.get().is_empty() {
                                            format!("Session {id}")
                                        } else {
                                            format!("Session {id} · {}", active_agent.get())
                                        }
                                    }}
                                </div>
                                <button class="btn" disabled=move || !running.get() on:click=move |_| stop_session()>
                                    "Stop"
                                </button>
                                <button class="btn" on:click=move |_| reset_session()>"Reset"</button>
                                <button class="btn" on:click=move |_| compact_session()>"Compact"</button>
                                <button class="btn" title="Move this session onto the agent's current version"
                                    on:click=move |_| migrate_session()>"Migrate"</button>
                                <button class="btn danger" on:click=move |_| {
                                    if let Some(id) = active.get() {
                                        delete_session(id);
                                    }
                                }>"Delete"</button>
                            </div>
                            <div
                                class="messages"
                                node_ref=messages_el
                                on:scroll=move |_| {
                                    if let Some(el) = messages_el.get() {
                                        let near = el.scroll_top() + el.client_height()
                                            >= el.scroll_height() - 48;
                                        at_bottom.set(near);
                                    }
                                }
                            >
                                {move || {
                                    let entries = messages.get();
                                    let mut out: Vec<AnyView> = Vec::new();
                                    let mut run: Vec<MsgEntry> = Vec::new();
                                    for e in entries {
                                        match e.block {
                                            UiBlock::User { .. } => {
                                                if !run.is_empty() {
                                                    out.push(assistant_group(std::mem::take(&mut run)));
                                                }
                                                out.push(block_child(&e.block));
                                            }
                                            _ => run.push(e),
                                        }
                                    }
                                    if !run.is_empty() {
                                        out.push(assistant_group(run));
                                    }
                                    out
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
                            <div class="statusbar">
                                {move || status_line.get().map(|s| view! { <span class="status-line">{s}</span> })}
                                {move || error.get().map(|e| view! { <span class="error-line">{e}</span> })}
                                {move || usage.get().map(|u| view! { <span class="usage-line">{u}</span> })}
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
                                                        let names: Vec<String> = agents
                                                            .get()
                                                            .iter()
                                                            .filter_map(|a| {
                                                                a.get("name")
                                                                    .and_then(|k| k.as_str())
                                                                    .map(|k| k.to_string())
                                                            })
                                                            .collect();
                                                        names
                                                            .into_iter()
                                                            .map(|name| {
                                                                let v = name.clone();
                                                                view! { <option value=v>{name}</option> }
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

async fn refresh_sessions_async(sessions: RwSignal<Vec<SessionSummary>>) {
    if let Ok((_, v)) = api::get("/api/sessions").await
        && let Some(list) = v.get("sessions").and_then(|x| x.as_array())
    {
        sessions.set(
            list.iter()
                .filter_map(|s| serde_json::from_value::<SessionSummary>(s.clone()).ok())
                .collect(),
        );
    }
}
