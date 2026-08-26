use crate::api;
use crate::shell::{DragRail, DrawerBackdrop, MenuButton, sidebar_width};
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
#[derive(Clone, Debug)]
enum UiBlock {
    User { content: String },
    Thinking { text: RwSignal<String> },
    Text { text: RwSignal<String> },
    ToolUse { card: RwSignal<ToolCard> },
}

#[derive(Clone)]
struct MsgEntry {
    id: u64,
    /// (created_ms, finished_ms); finished == 0 while the turn streams.
    times: RwSignal<(u64, u64)>,
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

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

fn alloc_id(next_id: &RwSignal<u64>) -> u64 {
    let mut id = 0;
    next_id.update(|n| {
        *n += 1;
        id = *n;
    });
    id
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
fn build_history(v: &Value, next_id: &RwSignal<u64>) -> Vec<MsgEntry> {
    let mut out: Vec<MsgEntry> = Vec::new();
    let Some(arr) = v.get("messages").and_then(|x| x.as_array()) else {
        return out;
    };
    for m in arr {
        match m.get("kind").and_then(|k| k.as_str()).unwrap_or("") {
            "user" => out.push(MsgEntry {
                id: alloc_id(next_id),
                times: RwSignal::new((
                    m["created_at_ms"].as_u64().unwrap_or(0),
                    m["finished_at_ms"].as_u64().unwrap_or(0),
                )),
                block: UiBlock::User {
                    content: text_of(m),
                },
            }),
            "thinking" => out.push(MsgEntry {
                id: alloc_id(next_id),
                times: RwSignal::new((
                    m["created_at_ms"].as_u64().unwrap_or(0),
                    m["finished_at_ms"].as_u64().unwrap_or(0),
                )),
                block: UiBlock::Thinking {
                    text: RwSignal::new(text_of(m)),
                },
            }),
            "text" => out.push(MsgEntry {
                id: alloc_id(next_id),
                times: RwSignal::new((
                    m["created_at_ms"].as_u64().unwrap_or(0),
                    m["finished_at_ms"].as_u64().unwrap_or(0),
                )),
                block: UiBlock::Text {
                    text: RwSignal::new(text_of(m)),
                },
            }),
            "tool-use" => out.push(MsgEntry {
                id: alloc_id(next_id),
                times: RwSignal::new((
                    m["created_at_ms"].as_u64().unwrap_or(0),
                    m["finished_at_ms"].as_u64().unwrap_or(0),
                )),
                block: UiBlock::ToolUse {
                    card: RwSignal::new(ToolCard {
                        id: str_of(m, "tool_call_id"),
                        name: str_of(m, "tool_name"),
                        arguments: str_of(m, "arguments"),
                        ..Default::default()
                    }),
                },
            }),
            "tool-result" => {
                let tid = str_of(m, "tool_call_id");
                let preview = truncate(&str_of(m, "text"), 500);
                let is_error = m.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                for e in out.iter().rev() {
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

fn text_of(v: &Value) -> String {
    v.get("text")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

async fn load_history(
    id: &str,
    messages: &RwSignal<Vec<MsgEntry>>,
    error: &RwSignal<Option<String>>,
    scroll_tick: &RwSignal<u64>,
    follow: &RwSignal<bool>,
    at_latest: &RwSignal<bool>,
    next_id: &RwSignal<u64>,
) {
    if let Ok((status, v)) = api::get(&format!("/api/sessions/{id}")).await {
        if status == 200 {
            messages.set(build_history(&v, next_id));
        } else {
            error.set(Some(str_of(&v, "error")));
        }
    }
    // Fresh content lands at the bottom; resume following there.
    follow.set(true);
    at_latest.set(true);
    scroll_tick.update(|t| *t += 1);
}

/// Append stream text to the trailing block of this kind, opening a new block
/// when the kind changes (thinking -> tool-use -> text ...). The display
/// order therefore matches the stream exactly, and thinking after a tool call
/// is no longer dropped.
fn stream_text(
    messages: &RwSignal<Vec<MsgEntry>>,
    next_id: &RwSignal<u64>,
    now: u64,
    matches_kind: impl Fn(&UiBlock) -> bool,
    make: impl FnOnce() -> UiBlock,
    extract: fn(&UiBlock) -> Option<RwSignal<String>>,
    text: &str,
) {
    // Signals are created OUTSIDE any update closure and ids are allocated
    // before entering the borrow: nested signal writes were trapping wasm
    // mid-stream.
    let last = messages.with_untracked(|m| {
        m.last()
            .map(|e| (matches_kind(&e.block), extract(&e.block)))
            .unwrap_or((false, None))
    });
    if !last.0 {
        let id = alloc_id(next_id);
        let times = RwSignal::new((now, 0));
        let block = make();
        messages.update(|m| m.push(MsgEntry { id, times, block }));
    }
    // Re-resolve AFTER the possible push so a freshly created block receives
    // its first chunk too; the handle keeps every write outside any borrow.
    let buf = messages.with_untracked(|m| m.last().and_then(|e| extract(&e.block)));
    if let Some(buf) = buf {
        buf.update(|s| s.push_str(text));
    }
}

/// The signals a chat page owns, bundled so streaming logic and the view
/// share one handle instead of an ever-growing parameter list.
#[derive(Clone, Copy)]
struct ChatSignals {
    messages: RwSignal<Vec<MsgEntry>>,
    running: RwSignal<bool>,
    usage: RwSignal<Option<String>>,
    error: RwSignal<Option<String>>,
    status_line: RwSignal<Option<String>>,
    scroll_tick: RwSignal<u64>,
    follow: RwSignal<bool>,
    at_latest: RwSignal<bool>,
    next_id: RwSignal<u64>,
}

/// What a single SSE frame asked the page to do beyond mutating blocks.
#[derive(Debug, PartialEq)]
enum EventOutcome {
    /// chunk / thinking / tool events mutated blocks in place.
    Silent,
    Status(String),
    Error(String),
    /// `finished_ms` stamps any still-open blocks; `usage_text` is the
    /// formatted usage summary when the payload carried one.
    Done {
        finished_ms: u64,
        usage_text: Option<String>,
    },
}

/// Apply one SSE frame to the streaming state. Returns the outcome for the
/// caller to surface (status/error/usage/done), keeping every signal write
/// inside a single `batch()` so no borrow outlives its guard.
fn apply_stream_event(st: &ChatSignals, now: u64, ev: &sse::SseEvent) -> EventOutcome {
    let messages = st.messages;
    let next_id = st.next_id;
    match ev.event.as_str() {
        "chunk" => {
            let text = serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
            stream_text(
                &messages,
                &next_id,
                now,
                last_is_text,
                || UiBlock::Text {
                    text: RwSignal::new(String::new()),
                },
                |block| match block {
                    UiBlock::Text { text } => Some(*text),
                    _ => None,
                },
                &text,
            );
            st.scroll_tick.update(|t| *t += 1);
            EventOutcome::Silent
        }
        "thinking" => {
            let text = serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
            stream_text(
                &messages,
                &next_id,
                now,
                last_is_thinking,
                || UiBlock::Thinking {
                    text: RwSignal::new(String::new()),
                },
                |block| match block {
                    UiBlock::Thinking { text } => Some(*text),
                    _ => None,
                },
                &text,
            );
            st.scroll_tick.update(|t| *t += 1);
            EventOutcome::Silent
        }
        "tool_use" => {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                let id = alloc_id(&next_id);
                let times = RwSignal::new((now, 0));
                let card = RwSignal::new(ToolCard {
                    id: str_of(&v, "id"),
                    name: str_of(&v, "name"),
                    ..Default::default()
                });
                messages.update(|m| {
                    m.push(MsgEntry {
                        id,
                        times,
                        block: UiBlock::ToolUse { card },
                    });
                });
            }
            st.scroll_tick.update(|t| *t += 1);
            EventOutcome::Silent
        }
        "tool_args" => {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                let tid = str_of(&v, "id");
                let args = str_of(&v, "arguments");
                for_each_card(&messages, &tid, &|card| {
                    card.arguments = args.clone();
                });
            }
            EventOutcome::Silent
        }
        "tool_result" => {
            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                let tid = str_of(&v, "id");
                let preview = truncate(&str_of(&v, "result_preview"), 500);
                let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                for_each_card(&messages, &tid, &|card| {
                    card.result = Some(preview.clone());
                    card.is_error = is_error;
                });
            }
            EventOutcome::Silent
        }
        "status" => {
            let text = serde_json::from_str::<String>(&ev.data).unwrap_or_else(|_| ev.data.clone());
            EventOutcome::Status(text)
        }
        "error" => {
            let msg = serde_json::from_str::<Value>(&ev.data)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or_else(|| ev.data.clone());
            EventOutcome::Error(msg)
        }
        "done" => {
            let usage_text = serde_json::from_str::<Value>(&ev.data)
                .ok()
                .and_then(|v| v.get("usage").cloned())
                .map(|usage| fmt_usage(&usage));
            EventOutcome::Done {
                finished_ms: now,
                usage_text,
            }
        }
        _ => EventOutcome::Silent,
    }
}

/// Stream a user message: push the user entry, then apply frames as they
/// arrive and surface their outcomes.
fn send(session_id: String, input: RwSignal<String>, st: ChatSignals) {
    let text = input.get().trim().to_string();
    if text.is_empty() || st.running.get() {
        return;
    }
    let id = alloc_id(&st.next_id);
    let times = RwSignal::new((now_ms(), 0));
    st.messages.update(|m| {
        m.push(MsgEntry {
            id,
            times,
            block: UiBlock::User {
                content: text.clone(),
            },
        });
    });
    input.set(String::new());
    st.running.set(true);
    st.usage.set(None);
    st.error.set(None);
    st.status_line.set(None);
    // The user asked for this reply; follow it regardless of prior scroll.
    st.follow.set(true);
    st.at_latest.set(true);
    st.scroll_tick.update(|t| *t += 1);

    let path = format!("/api/sessions/{session_id}/stream");
    let body = json!({ "content": text });
    spawn_local(async move {
        let result = sse::stream_post(&path, &body, move |ev| {
            match apply_stream_event(&st, now_ms(), &ev) {
                EventOutcome::Status(text) => st.status_line.set(Some(text)),
                EventOutcome::Error(msg) => st.error.set(Some(msg)),
                EventOutcome::Done {
                    finished_ms,
                    usage_text,
                } => {
                    if let Some(text) = usage_text {
                        st.usage.set(Some(text));
                    }
                    finish_open_blocks(&st.messages, finished_ms);
                    st.scroll_tick.update(|t| *t += 1);
                    st.running.set(false);
                }
                EventOutcome::Silent => {}
            }
        })
        .await;
        st.running.set(false);
        if let Err(e) = result {
            st.error.set(Some(e));
        }
    });
}

/// Stamp `finished` on every block still open (`finished == 0`).
fn finish_open_blocks(messages: &RwSignal<Vec<MsgEntry>>, finished: u64) {
    let open: Vec<RwSignal<(u64, u64)>> = messages.with_untracked(|m| {
        m.iter()
            .filter(|e| e.times.get_untracked().1 == 0)
            .map(|e| e.times)
            .collect()
    });
    for t in open {
        t.update(|(_created, fin)| *fin = finished);
    }
}

fn for_each_card(messages: &RwSignal<Vec<MsgEntry>>, id: &str, f: &impl Fn(&mut ToolCard)) {
    // Phase 1 (untracked read): collect the matching card signals.
    let targets: Vec<RwSignal<ToolCard>> = messages.with_untracked(|m| {
        m.iter()
            .rev()
            .filter_map(|e| match &e.block {
                UiBlock::ToolUse { card } if card.get_untracked().id == id => Some(*card),
                _ => None,
            })
            .take(1)
            .collect()
    });
    // Phase 2: mutate outside the messages borrow.
    for card in targets {
        card.update(f);
    }
}

fn last_is_text(block: &UiBlock) -> bool {
    matches!(block, UiBlock::Text { .. })
}

fn last_is_thinking(block: &UiBlock) -> bool {
    matches!(block, UiBlock::Thinking { .. })
}

fn block_child(entry: &MsgEntry) -> AnyView {
    match &entry.block {
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

/// Pure transition for one scroll event: returns `(at_latest, follow)`.
/// Echoes of our own pins (inside the suppression window) are ignored.
fn scroll_update(
    now: f64,
    pinned_until: f64,
    near: bool,
    follow: bool,
    at_latest: bool,
) -> (bool, bool) {
    if now < pinned_until {
        return (at_latest, follow);
    }
    if near { (true, true) } else { (false, follow) }
}

fn fmt_clock(ms: u64) -> String {
    // u64 converts to a JS BigInt, which `new Date` rejects; go through f64.
    let d = js_sys::Date::new(&(ms as f64).into());
    format!(
        "{:02}:{:02}:{:02}",
        d.get_hours(),
        d.get_minutes(),
        d.get_seconds()
    )
}

/// Muted per-card timestamp: local clock on creation plus a duration when
/// finishing took a measurable amount of time. Hover shows the full ISO.
/// Duration-aware label: injected clock formatter keeps this testable off-wasm.
fn time_label(created: u64, finished: u64, fmt_clock: impl Fn(u64) -> String) -> Option<String> {
    if created == 0 {
        return None;
    }
    let mut label = fmt_clock(created);
    if finished > created + 1000 {
        let dur = (finished - created) as f64 / 1000.0;
        label.push_str(&format!(" · {dur:.1}s"));
    }
    Some(label)
}

fn time_footer(times: RwSignal<(u64, u64)>) -> AnyView {
    view! {
        <div class="msg-time" title=move || {
            let (created, _) = times.get_untracked();
            if created == 0 { String::new() } else { js_sys::Date::new(&(created as f64).into()).to_iso_string().into() }
        }>
            {move || {
                let (created, finished) = times.get();
                time_label(created, finished, fmt_clock)
            }}
        </div>
    }
    .into_any()
}

/// Render one log entry as its own card. The list is flat and keyed by entry
/// id: streaming mounts new cards while already-mounted ones keep their
/// signals, so streamed text grows in place and tool cards update live.
fn entry_view(entry: &MsgEntry) -> AnyView {
    match &entry.block {
        UiBlock::User { .. } => {
            let child = block_child(entry);
            let footer = time_footer(entry.times);
            view! { <div class="msg user">{child} {footer}</div> }.into_any()
        }
        UiBlock::Thinking { .. } | UiBlock::Text { .. } | UiBlock::ToolUse { .. } => {
            let child = block_child(entry);
            let footer = time_footer(entry.times);
            view! { <div class="msg assistant">{child} {footer}</div> }.into_any()
        }
    }
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

/// Replace non-`http(s)` link targets with `#`, preserving attribute
/// structure for every other attribute and tag.
fn sanitize_hrefs(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        out.push_str(&rest[..start + "href=\"".len()]);
        rest = &rest[start + "href=\"".len()..];
        let end = rest.find('"').unwrap_or(rest.len());
        let url = &rest[..end];
        let blocked = url.starts_with("javascript:") || url.starts_with("data:");
        out.push_str(if blocked { "#" } else { url });
        out.push('"');
        rest = &rest[end + 1..];
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
    let next_id = RwSignal::new(1u64);
    let drawer_open = RwSignal::new(false);

    // Auto-scroll: follow the stream only while `follow` is engaged. Any
    // user intent (wheel / touch / scrollbar grab) disengages it immediately
    // and synchronously, so streaming never fights the user. Scrolling back
    // to the bottom re-engages; sending a message or switching sessions
    // resets it.
    let messages_el = NodeRef::<leptos::html::Div>::new();
    let follow = RwSignal::new(true);
    let at_latest = RwSignal::new(true);
    // Programmatic pins stamp this timestamp; scroll events within the window
    // are our own echo and are ignored, so they can never re-engage or
    // mislabel visibility.
    let pinned_until = RwSignal::new(0f64);
    let scroll_tick = RwSignal::new(0u64);

    let stream_signals = ChatSignals {
        messages,
        running,
        usage,
        error,
        status_line,
        scroll_tick,
        follow,
        at_latest,
        next_id,
    };

    Effect::new(move |_| {
        scroll_tick.get();
        messages.track();
        if !follow.get() {
            return;
        }
        if let Some(el) = messages_el.get() {
            pinned_until.set(js_sys::Date::now() + 80.0);
            el.set_scroll_top(el.scroll_height());
        }
    });

    let pin_to_latest = move || {
        follow.set(true);
        at_latest.set(true);
        scroll_tick.update(|t| *t += 1);
    };

    spawn_local(async move { refresh_sessions_async(sessions).await });
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
            let follow = follow;
            let at_latest = at_latest;
            let next_id = next_id;
            spawn_local(async move {
                load_history(
                    &id,
                    &messages,
                    &error,
                    &scroll_tick,
                    &follow,
                    &at_latest,
                    &next_id,
                )
                .await;
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
            drawer_open.set(false);
            navigate(&target, Default::default());
            untrack(|| go_to.set(None));
        }
    });

    let do_send = move || {
        if let Some(id) = active.get() {
            send(id, input, stream_signals);
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
                refresh_sessions_async(sessions).await;
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
            let follow = follow;
            let at_latest = at_latest;
            let next_id = next_id;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/reset"), &json!({})).await;
                load_history(
                    &id,
                    &messages,
                    &error,
                    &scroll_tick,
                    &follow,
                    &at_latest,
                    &next_id,
                )
                .await;
            });
        }
    };

    let compact_session = move || {
        if let Some(id) = active.get() {
            let messages = messages;
            let error = error;
            let scroll_tick = scroll_tick;
            let follow = follow;
            let at_latest = at_latest;
            let next_id = next_id;
            spawn_local(async move {
                let _ = api::post(&format!("/api/sessions/{id}/compact"), &json!({})).await;
                load_history(
                    &id,
                    &messages,
                    &error,
                    &scroll_tick,
                    &follow,
                    &at_latest,
                    &next_id,
                )
                .await;
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

    view! {
            <div class="app">
                <aside
                    class="sidebar"
                    class:open=drawer_open
                    style:width=move || format!("{}px", sidebar_width().get())
                >
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

                <DragRail/>
                <MenuButton open=drawer_open/>

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
                                    <button class="btn danger" on:click=move |_| {
                                        if let Some(id) = active.get() {
                                            delete_session(id);
                                        }
                                    }>"Delete"</button>
                                </div>
                                <div
                                    class="messages"
                                    node_ref=messages_el
                                    on:wheel=move |_| follow.set(false)
                                    on:touchstart=move |_| follow.set(false)
                                    on:mousedown=move |_| follow.set(false)
                                    on:scroll=move |_| {
                                        let Some(el) = messages_el.get() else { return };
                                        let near = el.scroll_top() + el.client_height()
                                            >= el.scroll_height() - 48;
                                        let (a, f) = scroll_update(
                                            js_sys::Date::now(),
                                            pinned_until.get_untracked(),
                                            near,
                                            follow.get_untracked(),
                                            at_latest.get_untracked(),
                                        );
                                        at_latest.set(a);
                                        follow.set(f);
                                    }
                                >
                                    <For each=move || messages.get() key=|e: &MsgEntry| e.id children=move |e| entry_view(&e)/>
                                </div>
                                {move || {
                                    (!follow.get() && !at_latest.get()).then(|| {
                                        view! {
                                            <button
                                                class="jump-pill"
                                                on:click=move |_| pin_to_latest()
                                            >
                                                "↓ latest"
                                            </button>
                                        }
                                    })
    }}
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
                                                                .map(|a| str_of(a, "name"))
                                                                .collect::<Vec<_>>();
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
                <DrawerBackdrop open=drawer_open/>
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block_signal() -> UiBlock {
        UiBlock::Text {
            text: RwSignal::new(String::new()),
        }
    }

    fn text_extract(block: &UiBlock) -> Option<RwSignal<String>> {
        match block {
            UiBlock::Text { text } => Some(*text),
            _ => None,
        }
    }

    #[test]
    fn stream_text_opens_blocks_on_kind_switch_and_appends_within_kind() {
        let messages = RwSignal::new(Vec::<MsgEntry>::new());
        let next_id = RwSignal::new(0u64); // first allocated id is 1

        stream_text(
            &messages,
            &next_id,
            1000,
            last_is_thinking,
            || UiBlock::Thinking {
                text: RwSignal::new(String::new()),
            },
            |block| match block {
                UiBlock::Thinking { text } => Some(*text),
                _ => None,
            },
            "let me ",
        );
        stream_text(
            &messages,
            &next_id,
            1050,
            last_is_thinking,
            || UiBlock::Thinking {
                text: RwSignal::new(String::new()),
            },
            |block| match block {
                UiBlock::Thinking { text } => Some(*text),
                _ => None,
            },
            "think",
        );
        stream_text(
            &messages,
            &next_id,
            1100,
            last_is_text,
            || UiBlock::Text {
                text: RwSignal::new(String::new()),
            },
            text_extract,
            "Hello",
        );

        let entries = messages.get_untracked();
        assert_eq!(entries.len(), 2, "kind switch opens a new block");
        assert_eq!(entries[0].id, 1);
        match &entries[0].block {
            UiBlock::Thinking { text } => assert_eq!(text.get_untracked(), "let me think"),
            other => panic!("expected thinking, got {other:?}"),
        }
        assert_eq!(entries[0].times.get_untracked(), (1000, 0));
        assert_eq!(entries[1].id, 2);
        match &entries[1].block {
            UiBlock::Text { text } => assert_eq!(text.get_untracked(), "Hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn finish_open_blocks_stamps_only_open_entries() {
        let messages = RwSignal::new(Vec::<MsgEntry>::new());
        messages.set(vec![
            MsgEntry {
                id: 1,
                times: RwSignal::new((10, 0)),
                block: text_block_signal(),
            },
            MsgEntry {
                id: 2,
                times: RwSignal::new((20, 25)),
                block: text_block_signal(),
            },
        ]);

        finish_open_blocks(&messages, 99);

        let entries = messages.get_untracked();
        assert_eq!(entries[0].times.get_untracked(), (10, 99));
        assert_eq!(
            entries[1].times.get_untracked(),
            (20, 25),
            "already finished untouched"
        );
    }

    fn history_fixture() -> Value {
        json!({
            "messages": [
                {"kind": "user", "text": "hi", "created_at_ms": 1000, "finished_at_ms": 1000},
                {"kind": "thinking", "text": "reasoning", "created_at_ms": 2000, "finished_at_ms": 2500},
                {"kind": "tool-use", "tool_call_id": "c1", "tool_name": "time",
                 "arguments": "{}", "created_at_ms": 3000, "finished_at_ms": 3000},
                {"kind": "tool-result", "text": "12:00:00", "tool_call_id": "c1",
                 "created_at_ms": 4000, "finished_at_ms": 4500},
                {"kind": "text", "text": "done", "created_at_ms": 5000, "finished_at_ms": 6000}
            ]
        })
    }

    #[test]
    fn build_history_parses_blocks_payloads_and_links_results() {
        let next_id = RwSignal::new(1u64);
        let entries = build_history(&history_fixture(), &next_id);

        // tool-result attaches to the existing card instead of adding one.
        assert_eq!(entries.len(), 4);
        assert!(
            entries
                .iter()
                .zip(entries.iter().skip(1))
                .all(|(a, b)| a.id < b.id)
        );

        assert!(matches!(entries[0].block, UiBlock::User { ref content } if content == "hi"));
        assert_eq!(entries[0].times.get_untracked(), (1000, 1000));

        match &entries[2].block {
            UiBlock::ToolUse { card } => {
                let c = card.get_untracked();
                assert_eq!(c.id, "c1");
                assert_eq!(c.name, "time");
                assert_eq!(c.arguments, "{}");
                assert_eq!(c.result.as_deref(), Some("12:00:00"));
                assert!(!c.is_error);
            }
            other => panic!("expected tool-use, got {other:?}"),
        }
    }

    #[test]
    fn build_history_marks_error_results() {
        let mut v = history_fixture();
        v["messages"][3]["text"] = json!("boom");
        v["messages"][3]["is_error"] = json!(true);
        let next_id = RwSignal::new(1u64);
        let entries = build_history(&v, &next_id);
        match &entries[2].block {
            UiBlock::ToolUse { card } => {
                let c = card.get_untracked();
                assert_eq!(c.result.as_deref(), Some("boom"));
                assert!(c.is_error);
            }
            other => panic!("expected tool-use, got {other:?}"),
        }
    }

    #[test]
    fn scroll_update_ignores_suppressed_echoes() {
        // Our own pin echoing back inside the suppression window changes
        // nothing — this is the exact race that used to yank the user down.
        assert_eq!(scroll_update(500.0, 600.0, true, true, true), (true, true));
        assert_eq!(
            scroll_update(500.0, 600.0, true, false, false),
            (false, false)
        );
    }

    #[test]
    fn scroll_update_disengages_when_scrolled_away() {
        assert_eq!(scroll_update(1000.0, 0.0, false, true, true), (false, true));
    }

    #[test]
    fn scroll_update_resumes_follow_on_genuine_bottom_hit() {
        assert_eq!(scroll_update(1000.0, 0.0, true, false, false), (true, true));
    }

    #[test]
    fn time_label_composition() {
        let fmt = |ms: u64| format!("c{ms}");
        assert_eq!(time_label(0, 0, fmt), None);
        assert_eq!(time_label(1000, 1500, fmt), Some("c1000".to_string()));
        let long = time_label(1000, 3000, fmt).unwrap();
        assert!(
            long.starts_with("c1000") && long.ends_with("2.0s"),
            "{long}"
        );
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 4), "hell…");
        let multibyte = "héllo wörld";
        assert_eq!(truncate(multibyte, 6), "héllo…");
    }

    #[test]
    fn sanitize_hrefs_blocks_script_and_data() {
        let html = r#"<a href="javascript:alert(1)">x</a><a href="https://ok">y</a>"#;
        let out = sanitize_hrefs(html);
        assert!(out.contains(r##"<a href="#">x</a>"##), "{out}");
        assert!(out.contains(r##"<a href="https://ok">y</a>"##), "{out}");
    }

    fn chat_signals() -> ChatSignals {
        ChatSignals {
            messages: RwSignal::new(Vec::<MsgEntry>::new()),
            running: RwSignal::new(false),
            usage: RwSignal::new(None),
            error: RwSignal::new(None),
            status_line: RwSignal::new(None),
            scroll_tick: RwSignal::new(0),
            follow: RwSignal::new(true),
            at_latest: RwSignal::new(true),
            next_id: RwSignal::new(0u64),
        }
    }

    fn ev(event: &str, data: &str) -> sse::SseEvent {
        sse::SseEvent {
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    #[test]
    fn apply_stream_event_creates_and_fills_tool_cards() {
        let st = chat_signals();
        assert!(matches!(
            apply_stream_event(&st, 1000, &ev("tool_use", r#"{"id":"c1","name":"time"}"#)),
            EventOutcome::Silent
        ));

        // First card's identity lands immediately.
        let entries = st.messages.get_untracked();
        assert_eq!(entries.len(), 1);
        match &entries[0].block {
            UiBlock::ToolUse { card } => {
                assert_eq!(card.get_untracked().id, "c1");
                assert_eq!(card.get_untracked().name, "time");
            }
            other => panic!("expected tool-use, got {other:?}"),
        }

        // Arguments stream in, then the result (error-flagged) attaches.
        apply_stream_event(
            &st,
            1100,
            &ev("tool_args", r#"{"id":"c1","arguments":"{}"}"#),
        );
        apply_stream_event(
            &st,
            1200,
            &ev(
                "tool_result",
                r#"{"id":"c1","result_preview":"12:00","is_error":true}"#,
            ),
        );
        let entries = st.messages.get_untracked();
        match &entries[0].block {
            UiBlock::ToolUse { card } => {
                let c = card.get_untracked();
                assert_eq!(c.arguments, "{}");
                assert_eq!(c.result.as_deref(), Some("12:00"));
                assert!(c.is_error);
            }
            other => panic!("expected tool-use, got {other:?}"),
        }
    }

    #[test]
    fn apply_stream_event_surfaces_status_error_and_done() {
        let st = chat_signals();
        assert!(matches!(
            apply_stream_event(&st, 0, &ev("status", "\"compacted: 3 summarized\"")),
            EventOutcome::Status(text) if text == "compacted: 3 summarized"
        ));
        assert!(matches!(
            apply_stream_event(&st, 0, &ev("error", "{\"message\":\"boom\"}")),
            EventOutcome::Error(msg) if msg == "boom"
        ));
        // done stamps open blocks and returns usage text.
        apply_stream_event(&st, 900, &ev("chunk", "\"Hel\""));
        let out = apply_stream_event(
            &st,
            2000,
            &ev(
                "done",
                r#"{"done":true,"usage":{"input_tokens":3,"cache_read_tokens":2,"cache_creation_tokens":1,"output_tokens":4}}"#,
            ),
        );
        let finished_ms = match out {
            EventOutcome::Done {
                finished_ms,
                usage_text,
            } => {
                assert!(finished_ms > 0);
                assert_eq!(
                    usage_text.as_deref(),
                    Some("in 3 | cache read 2 + write 1 | out 4")
                );
                finished_ms
            }
            other => panic!("expected Done, got {other:?}"),
        };
        // send() performs the finishing pass after receiving Done.
        finish_open_blocks(&st.messages, finished_ms);
        let entries = st.messages.get_untracked();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].times.get_untracked().1 > 0,
            "open block stamped finished"
        );
    }

    #[test]
    fn fmt_usage_renders_all_fields() {
        let u = json!({"input_tokens": 3, "cache_read_tokens": 2,
                       "cache_creation_tokens": 1, "output_tokens": 4});
        assert_eq!(fmt_usage(&u), "in 3 | cache read 2 + write 1 | out 4");
    }
}
