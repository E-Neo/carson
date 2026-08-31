use crate::api;
use crate::shell::{DragRail, DrawerBackdrop, MenuButton, sidebar_width};
use crate::sse;
use crate::types::{SandboxSummary, SessionSummary};
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
    User {
        content: String,
    },
    Thinking {
        text: RwSignal<String>,
    },
    Text {
        text: RwSignal<String>,
    },
    /// A tool interaction as a plain text block. It is driven by the exact
    /// same `RwSignal<String>` + inline-closure mechanism as thinking/text
    /// (which update live), rather than the old card with per-field closures
    /// that never subscribed during streaming.
    Tool {
        text: RwSignal<String>,
    },
}

#[derive(Clone)]
struct MsgEntry {
    id: u64,
    /// (created_ms, finished_ms); finished == 0 while the turn streams.
    times: RwSignal<(u64, u64)>,
    block: UiBlock,
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
            "tool-use" => {
                let mut text = str_of(m, "tool_name");
                let args = str_of(m, "arguments");
                if !args.is_empty() {
                    text.push(' ');
                    text.push_str(&args);
                }
                out.push(MsgEntry {
                    id: alloc_id(next_id),
                    times: RwSignal::new((
                        m["created_at_ms"].as_u64().unwrap_or(0),
                        m["finished_at_ms"].as_u64().unwrap_or(0),
                    )),
                    block: UiBlock::Tool {
                        text: RwSignal::new(text),
                    },
                });
            }
            "tool-result" => {
                let preview = truncate(&str_of(m, "text"), 500);
                let marker = if m.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false) {
                    "→ error: "
                } else {
                    "→ "
                };
                out.push(MsgEntry {
                    id: alloc_id(next_id),
                    times: RwSignal::new((
                        m["created_at_ms"].as_u64().unwrap_or(0),
                        m["finished_at_ms"].as_u64().unwrap_or(0),
                    )),
                    block: UiBlock::Tool {
                        text: RwSignal::new(format!("{marker}{preview}")),
                    },
                });
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
/// when the kind changes (thinking -> text -> ...). The display order
/// therefore matches the stream exactly.
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
        // A new block of a different kind starts: close the previous open
        // non-user block so it spans only up to here.
        close_open_assistant(messages, now);
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
    /// Called after a turn completes so the session list reorders.
    on_turn_done: Option<Callback<()>>,
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
            let v = parse_event_object(&ev.data);
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
            if !name.is_empty() {
                push_tool(&messages, &next_id, now, name.to_string());
            }
            st.scroll_tick.update(|t| *t += 1);
            EventOutcome::Silent
        }
        "tool_args" => {
            let v = parse_event_object(&ev.data);
            let args = v.get("arguments").and_then(|x| x.as_str()).unwrap_or("");
            if !args.is_empty() {
                // Append into the tool_use block so tool use + arguments form
                // one block (the result gets its own block).
                append_last_tool(&messages, &format!(" {args}"));
            }
            // The tool call is fully yielded here: its duration is the time to
            // produce the call (streaming the arguments).
            close_open_assistant(&messages, now);
            EventOutcome::Silent
        }
        "tool_result" => {
            let v = parse_event_object(&ev.data);
            let preview = v
                .get("result_preview")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let marker = if v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false) {
                "→ error: "
            } else {
                "→ "
            };
            // The server measured the actual invocation; use its timestamps so
            // the tool-result duration is the execution time.
            let created = v
                .get("created_at_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(now);
            let finished = v
                .get("finished_at_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(now);
            push_tool_at(
                &messages,
                &next_id,
                created,
                finished,
                format!("{marker}{preview}"),
            );
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
                    // Close the turn's final open non-user block and stamp the
                    // user block so its duration spans the whole turn.
                    close_open_assistant(&st.messages, finished_ms);
                    finish_user(&st.messages, finished_ms);
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
        if let Some(cb) = st.on_turn_done {
            cb.run(());
        }
    });
}

/// Stamp `finished` on every block still open (`finished == 0`).
/// Close the most recent non-user block still open (finished == 0), stamping
/// its finish at `at` — so each block's duration spans until the next block
/// of the turn begins. The user block is handled separately by `finish_user`.
fn close_open_assistant(messages: &RwSignal<Vec<MsgEntry>>, at: u64) {
    let target = messages.with_untracked(|m| {
        m.iter()
            .rev()
            .find(|e| e.times.get_untracked().1 == 0 && !matches!(e.block, UiBlock::User { .. }))
            .map(|e| e.times)
    });
    if let Some(t) = target {
        t.update(|(_created, fin)| *fin = at);
    }
}

/// Stamp the current turn's user block finish at `at` (its duration is the
/// whole turn).
fn finish_user(messages: &RwSignal<Vec<MsgEntry>>, at: u64) {
    let target = messages.with_untracked(|m| {
        m.iter()
            .rev()
            .find(|e| e.times.get_untracked().1 == 0 && matches!(e.block, UiBlock::User { .. }))
            .map(|e| e.times)
    });
    if let Some(t) = target {
        t.update(|(_created, fin)| *fin = at);
    }
}

/// Push a new plain-text Tool block. Closes the previous open non-user block
/// so it spans only up to this point.
fn push_tool(messages: &RwSignal<Vec<MsgEntry>>, next_id: &RwSignal<u64>, now: u64, text: String) {
    close_open_assistant(messages, now);
    let id = alloc_id(next_id);
    let times = RwSignal::new((now, 0));
    messages.update(|m| {
        m.push(MsgEntry {
            id,
            times,
            block: UiBlock::Tool {
                text: RwSignal::new(text),
            },
        });
    });
}

/// Push a Tool block with explicit server-measured timestamps (used for the
/// tool-result, whose duration is the actual invocation time).
fn push_tool_at(
    messages: &RwSignal<Vec<MsgEntry>>,
    next_id: &RwSignal<u64>,
    created: u64,
    finished: u64,
    text: String,
) {
    let id = alloc_id(next_id);
    let times = RwSignal::new((created, finished));
    messages.update(|m| {
        m.push(MsgEntry {
            id,
            times,
            block: UiBlock::Tool {
                text: RwSignal::new(text),
            },
        });
    });
}

/// Append text to the most recent `Tool` block (the tool_use block the
/// current call just opened), so use + arguments stay together.
fn append_last_tool(messages: &RwSignal<Vec<MsgEntry>>, text: &str) {
    let target = messages.with_untracked(|m| {
        m.iter().rev().find_map(|e| match &e.block {
            UiBlock::Tool { text } => Some(*text),
            _ => None,
        })
    });
    if let Some(t) = target {
        t.update(|s| s.push_str(text));
    }
}

/// The SSE layer frames every payload as a JSON string, so an object payload
/// (tool events) arrives as a JSON string literal containing JSON text: e.g.
/// `data: "{\"id\":\"c1\",...}"`. Unwrap that outer layer so field access
/// sees the actual object.
fn parse_event_object(data: &str) -> Value {
    match serde_json::from_str::<Value>(data) {
        Ok(Value::String(inner)) => serde_json::from_str(&inner).unwrap_or_default(),
        Ok(v) => v,
        Err(_) => Value::Null,
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
        UiBlock::Tool { text } => {
            // Same style as thinking; the header's "Tool" label distinguishes it.
            let t = *text;
            view! { <div class="thinking">{move || t.get()}</div> }.into_any()
        }
        UiBlock::User { content } => {
            view! { <div class="msg user">{content.clone()}</div> }.into_any()
        }
    }
}

/// Pure transition for one scroll event: returns `(at_latest, follow)`.
///
/// Our own pins (inside the suppression window) are ignored entirely.
/// Following only resumes on a genuine away -> near transition: starting a
/// drag while already at the bottom must never resurrect auto-scroll, or
/// mobile/touch streams would yank the view back on the first tiny move.
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
    let new_at_latest = near;
    let new_follow = if !at_latest && near { true } else { follow };
    (new_at_latest, new_follow)
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

/// Milliseconds since creation to finish, formatted as `{:.3}s`.
fn fmt_duration(created: u64, finished: u64) -> String {
    format!("{:.3}s", finished.saturating_sub(created) as f64 / 1000.0)
}

/// Header line: the start time, then an optional kind label (Thinking / Tool).
fn time_head(kind: Option<&str>, times: RwSignal<(u64, u64)>) -> AnyView {
    let label = kind.map(str::to_owned);
    view! {
        <div class="msg-head" title=move || {
            let (created, _) = times.get_untracked();
            if created == 0 { String::new() } else { js_sys::Date::new(&(created as f64).into()).to_iso_string().into() }
        }>
            <span class="msg-start">{move || {
                let (created, _) = times.get();
                if created == 0 { String::new() } else { fmt_clock(created) }
            }}</span>
            {move || label.clone().map(|k| view! { <span class="msg-kind">{k}</span> })}
        </div>
    }
    .into_any()
}

/// Footer line: the duration, always visible once the block is stamped.
fn time_tail(times: RwSignal<(u64, u64)>) -> AnyView {
    view! {
        <div class="msg-dur">{move || {
            let (created, finished) = times.get();
            if created == 0 { String::new() } else { fmt_duration(created, finished) }
        }}</div>
    }
    .into_any()
}

/// Render one log entry as its own card. The list is flat and keyed by entry
/// id: streaming mounts new cards while already-mounted ones keep their
/// signals, so streamed text grows in place and tool cards update live.
/// Every card carries a header (start time, then a kind label) and a footer
/// (duration); the user's duration spans the whole turn.
fn entry_view(entry: &MsgEntry) -> AnyView {
    let kind = match &entry.block {
        UiBlock::Thinking { .. } => Some("Thinking"),
        UiBlock::Tool { .. } => Some("Tool"),
        _ => None,
    };
    let head = time_head(kind, entry.times);
    let tail = time_tail(entry.times);
    let child = block_child(entry);
    match &entry.block {
        UiBlock::User { .. } => {
            view! { <div class="msg user">{head} {child} {tail}</div> }.into_any()
        }
        _ => view! {
            <div class="msg assistant">{head} {child} {tail}</div>
        }
        .into_any(),
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

    // Per-session settings drawer.
    let settings_open = RwSignal::new(false);
    let settings_session = RwSignal::new(None::<String>);
    let sandboxes = RwSignal::new(Vec::<SandboxSummary>::new());
    let name_edit = RwSignal::new(String::new());
    let rename_alias = RwSignal::new(String::new());
    let new_sandbox_name = RwSignal::new(String::new());
    let selected_sandbox = RwSignal::new(None::<String>);
    // Which session-item (if any) has its action menu open, plus where to anchor it.
    let menu_popover = RwSignal::new(None::<(String, f64, f64)>);
    // Inline rename of a session-item row.
    let editing_session = RwSignal::new(None::<String>);
    let rename_draft = RwSignal::new(String::new());

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
        on_turn_done: Some(Callback::new(move |()| {
            let sessions = sessions;
            spawn_local(async move {
                refresh_sessions_async(sessions).await;
            });
        })),
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

    // The session the settings drawer edits (defaults to the active one).
    let settings_target = move || {
        settings_session
            .get()
            .or_else(|| active.get())
            .unwrap_or_default()
    };

    let open_settings_for = move |sid: String| {
        settings_session.set(Some(sid.clone()));
        name_edit.set(
            sessions
                .get()
                .iter()
                .find(|s| s.id == sid)
                .and_then(|s| s.name.clone())
                .unwrap_or_default(),
        );
        settings_open.set(true);
        let sandboxes = sandboxes;
        spawn_local(async move {
            load_sandboxes_async(sandboxes).await;
        });
    };

    // Reset/compact targeted at the session the settings drawer is editing.
    let reset_target = move || {
        let id = settings_target();
        if id.is_empty() {
            return;
        }
        let sessions = sessions;
        spawn_local(async move {
            let _ = api::post(&format!("/api/sessions/{id}/reset"), &json!({})).await;
            refresh_sessions_async(sessions).await;
        });
    };

    let compact_target = move || {
        let id = settings_target();
        if id.is_empty() {
            return;
        }
        let sessions = sessions;
        spawn_local(async move {
            let _ = api::post(&format!("/api/sessions/{id}/compact"), &json!({})).await;
            refresh_sessions_async(sessions).await;
        });
    };

    // The floating action menu reads its session from the popover signal, so
    // these handlers only capture Copy signals and sibling closures.
    let menu_rename = move || {
        if let Some((sid, _, _)) = menu_popover.get() {
            menu_popover.set(None);
            let current = sessions
                .get()
                .iter()
                .find(|s| s.id == sid)
                .and_then(|s| s.name.clone())
                .unwrap_or_default();
            rename_draft.set(current);
            editing_session.set(Some(sid));
        }
    };
    let menu_settings = move || {
        if let Some((sid, _, _)) = menu_popover.get() {
            menu_popover.set(None);
            open_settings_for(sid);
        }
    };
    let menu_delete = move || {
        if let Some((sid, _, _)) = menu_popover.get() {
            menu_popover.set(None);
            delete_session(sid);
        }
    };

    // Save/cancel the inline session rename in a session-item row.
    let rename_inline_save = move |sid: String| {
        editing_session.set(None);
        let name = rename_draft.get();
        let sessions = sessions;
        spawn_local(async move {
            let _ = api::put(&format!("/api/sessions/{sid}"), &json!({ "name": name })).await;
            refresh_sessions_async(sessions).await;
        });
    };
    let rename_inline_cancel = move || {
        editing_session.set(None);
    };

    // --- session settings ---------------------------------------------------

    let save_session_name = move || {
        let id = settings_target();
        if !id.is_empty() {
            let name = name_edit.get();
            let sessions = sessions;
            spawn_local(async move {
                let _ = api::put(&format!("/api/sessions/{id}"), &json!({ "name": name })).await;
                refresh_sessions_async(sessions).await;
            });
        }
    };

    let switch_sandbox = move |sandbox_id: String| {
        let id = settings_target();
        if !id.is_empty() {
            let sessions = sessions;
            spawn_local(async move {
                let _ = api::put(
                    &format!("/api/sessions/{id}"),
                    &json!({ "sandbox_id": sandbox_id }),
                )
                .await;
                refresh_sessions_async(sessions).await;
            });
        }
    };

    let create_new_sandbox = move || {
        let id = settings_target();
        if id.is_empty() {
            return;
        }
        let name = new_sandbox_name.get();
        if name.trim().is_empty() {
            return;
        }
        new_sandbox_name.set(String::new());
        let sessions = sessions;
        let sandboxes = sandboxes;
        spawn_local(async move {
            if let Ok((_, v)) = api::post("/api/sandboxes", &json!({ "name": name })).await
                && let Some(sid) = v.get("id").and_then(|x| x.as_str())
            {
                let _ = api::put(
                    &format!("/api/sessions/{id}"),
                    &json!({ "sandbox_id": sid }),
                )
                .await;
                load_sandboxes_async(sandboxes).await;
                refresh_sessions_async(sessions).await;
            }
        });
    };

    let rename_alias_save = move || {
        if let Some(sid) = selected_sandbox.get() {
            let name = rename_alias.get();
            let sandboxes = sandboxes;
            spawn_local(async move {
                let _ = api::put(&format!("/api/sandboxes/{sid}"), &json!({ "name": name })).await;
                load_sandboxes_async(sandboxes).await;
            });
        }
    };

    let current_sandbox_name = Memo::new(move |_| {
        let target = settings_target();
        let sid = sessions
            .get()
            .iter()
            .find(|s| s.id == target)
            .and_then(|s| s.sandbox_id.clone())
            .unwrap_or_default();
        if sid.is_empty() {
            return String::from("(none)");
        }
        sandboxes
            .get()
            .iter()
            .find(|b| b.id == sid)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| sid)
    });

    // The id of the sandbox the settings-target session currently uses.
    let current_sandbox_id = Memo::new(move |_| {
        let target = settings_target();
        sessions
            .get()
            .iter()
            .find(|s| s.id == target)
            .and_then(|s| s.sandbox_id.clone())
    });

    let new_chat = move || go_to.set(Some("/chat".to_string()));

    // Toolbar title: alias (when set) plus session id and agent.
    let active_title = Memo::new(move |_| {
        let list = sessions.get();
        let Some(sess) = list.iter().find(|s| Some(&s.id) == active.get().as_ref()) else {
            return String::new();
        };
        let id = short_id(&sess.id);
        let agent = &sess.agent;
        match sess
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            Some(name) if agent.is_empty() => format!("{name} · {id}"),
            Some(name) => format!("{name} · {id} · {agent}"),
            None if agent.is_empty() => format!("Session {id}"),
            None => format!("Session {id} · {agent}"),
        }
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
                                let active_sid = sid.clone();
                                let active_class = move || {
                                    if active.get().as_ref() == Some(&active_sid) {
                                        "session-item active"
                                    } else {
                                        "session-item"
                                    }
                                };
                                // Reactive: re-derives from the sessions signal
                                // so renames re-render without page switches.
                                let label_sid = sid.clone();
                                let label = move || {
                                    let list = sessions.get();
                                    let cur = list.iter().find(|s| &s.id == &label_sid);
                                    match cur.and_then(|s| s.name.clone()) {
                                        Some(name) if !name.is_empty() => name,
                                        _ => {
                                            let agent = cur
                                                .map(|s| s.agent.clone())
                                                .unwrap_or_default();
                                            format!("{} · {agent}", short_id(&label_sid))
                                        }
                                    }
                                };
                                let more_sid = sid.clone();
                                let edit_sid = sid.clone();
                                let edit_sid2 = sid.clone();
                                let edit_sid3 = sid.clone();
                                let edit_sid4 = sid.clone();
                                view! {
                                    <div class=active_class>
                                        <input
                                            class="session-rename"
                                            class:hidden=move || {
                                                editing_session.get().as_ref() != Some(&edit_sid)
                                            }
                                            name="session-name"
                                            aria-label="Session name"
                                            prop:value=move || rename_draft.get()
                                            on:input=move |ev| rename_draft.set(event_target_value(&ev))
                                            on:keydown=move |ev| {
                                                let key = ev.key();
                                                if key == "Enter" {
                                                    ev.prevent_default();
                                                    rename_inline_save(edit_sid2.clone());
                                                } else if key == "Escape" {
                                                    ev.prevent_default();
                                                    rename_inline_cancel();
                                                }
                                            }
                                            on:blur=move |_| {
                                                if editing_session.get().is_some() {
                                                    rename_inline_save(edit_sid3.clone());
                                                }
                                            }
                                        />
                                        <button
                                            class="name"
                                            class:hidden=move || {
                                                editing_session.get().as_ref() == Some(&edit_sid4)
                                            }
                                            on:click=move |_| {
                                                menu_popover.set(None);
                                                select_session(sid.clone());
                                            }
                                        >
                                            {label}
                                        </button>
                                        <button
                                            class="more"
                                            title="Session actions"
                                            on:click=move |ev| {
                                                let open = menu_popover
                                                    .get()
                                                    .as_ref()
                                                    .map(|(id, _, _)| id == &more_sid)
                                                    .unwrap_or(false);
                                                let x = ev.client_x() as f64;
                                                let y = ev.client_y() as f64;
                                                let next = if open {
                                                    None
                                                } else {
                                                    Some((more_sid.clone(), x, y))
                                                };
                                                menu_popover.set(next);
                                            }
                                        >
                                            "⋯"
                                        </button>
                                    </div>
                                }
                            }
                        />
                    </div>
                    {move || {
                        if menu_popover.get().is_some() {
                            Some(view! {
                                <>
                                    <div
                                        class="menu-backdrop"
                                        on:click=move |_| menu_popover.set(None)
                                    ></div>
                                    <div
                                        class="session-menu"
                                        style:left=move || {
                                            menu_popover
                                                .get()
                                                .map(|(_, x, _)| format!("{x}px"))
                                                .unwrap_or_default()
                                        }
                                        style:top=move || {
                                            menu_popover
                                                .get()
                                                .map(|(_, _, y)| format!("{y}px"))
                                                .unwrap_or_default()
                                        }
                                    >
                                        <button on:click=move |_| menu_rename()>"Rename"</button>
                                        <button on:click=move |_| menu_settings()>"Settings"</button>
                                        <button
                                            class="danger"
                                            on:click=move |_| menu_delete()
                                        >"Delete"</button>
                                    </div>
                                </>
                            })
                        } else {
                            None
                        }
                    }}
                    <a class="admin-link" href="/admin">"Admin"</a>
                    <button
                        class="logout-link"
                        on:click=move |_| {
                            spawn_local(async move {
                                crate::logout().await;
                            })
                        }
                    >
                        "Log out"
                    </button>
                </aside>

                <DragRail/>
                <MenuButton open=drawer_open/>

                <main class="main">
                    {move || {
                        if active.get().is_some() {
                            view! {
                                <div class="toolbar">
                                    <div class="title">
                                        {move || active_title.get()}
                                    </div>
                                    <button class="btn" disabled=move || !running.get() on:click=move |_| stop_session()>
                                        "Stop"
                                    </button>
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
                                    <div class="messages-column">
                                        <For each=move || messages.get() key=|e: &MsgEntry| e.id children=move |e| entry_view(&e)/>
                                    </div>
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
                                    <div class="composer-inner">
                                        <textarea
                                            name="message"
                                            aria-label="Message"
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
                                                    <label for="agent-select">"Agent"</label>
                                                    <select
                                                        id="agent-select"
                                                        name="agent"
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
                {move || {
                    settings_open.get().then(|| {
                        view! {
                            <>
                                <div
                                    class="settings-backdrop"
                                    on:click=move |_| settings_open.set(false)
                                ></div>
                                <aside class="settings-panel">
                                    <div class="settings-head">
                                        <h3>"Session settings"</h3>
                                        <button class="btn" on:click=move |_| settings_open.set(false)>
                                            "Close"
                                        </button>
                                    </div>
                                    <div class="settings-body">
                                        <label for="session-name">"Name"</label>
                                        <div class="settings-row">
                                            <input
                                                id="session-name"
                                                name="session-name"
                                                placeholder="Session name"
                                                prop:value=move || name_edit.get()
                                                on:input=move |ev| name_edit.set(event_target_value(&ev))
                                            />
                                            <button class="btn primary" on:click=move |_| save_session_name()>
                                                "Save"
                                            </button>
                                        </div>
                                        <label>"Sandbox"</label>
                                        <div class="settings-hint">
                                            "Current: "{move || current_sandbox_name.get()}
                                        </div>
                                        <div class="sandbox-list">
                                            <For
                                                each=move || sandboxes.get()
                                                key=|s: &SandboxSummary| s.id.clone()
                                                children=move |sb: SandboxSummary| {
                                                    let id = sb.id.clone();
                                                    // Reactive: re-derives the name from the
                                                    // sandboxes signal so renames re-render.
                                                    let name_sid = id.clone();
                                                    let name = move || {
                                                        sandboxes
                                                            .get()
                                                            .iter()
                                                            .find(|b| &b.id == &name_sid)
                                                            .map(|b| b.name.clone())
                                                            .unwrap_or_default()
                                                    };
                                                    let click_id = id.clone();
                                                    let current_id = id.clone();
                                                    let selected_id = id.clone();
                                                    let is_current = move || {
                                                        current_sandbox_id.get().as_ref() == Some(&current_id)
                                                    };
                                                    let is_selected = move || {
                                                        selected_sandbox.get().as_ref() == Some(&selected_id)
                                                    };
                                                    let cls = move || {
                                                        if is_current() {
                                                            "sandbox-item current"
                                                        } else if is_selected() {
                                                            "sandbox-item active"
                                                        } else {
                                                            "sandbox-item"
                                                        }
                                                    };
                                                    view! {
                                                        <div
                                                            class=cls
                                                            on:click=move |_| {
                                                                selected_sandbox.set(Some(click_id.clone()));
                                                                let nm = sandboxes
                                                                    .get()
                                                                    .iter()
                                                                    .find(|b| &b.id == &click_id)
                                                                    .map(|b| b.name.clone())
                                                                    .unwrap_or_default();
                                                                rename_alias.set(nm);
                                                                switch_sandbox(click_id.clone());
                                                            }
                                                        >
                                                            <span class="sandbox-name">{name}</span>
                                                            <span class="sandbox-id">{short_id(&id)}</span>
                                                        </div>
                                                    }
                                                }
                                            />
                                        </div>
                                        <div class="settings-row">
                                            <input
                                                id="sandbox-rename"
                                                name="sandbox-rename"
                                                placeholder="Rename selected sandbox"
                                                prop:value=move || rename_alias.get()
                                                on:input=move |ev| rename_alias.set(event_target_value(&ev))
                                            />
                                            <button
                                                class="btn"
                                                disabled=move || selected_sandbox.get().is_none()
                                                on:click=move |_| rename_alias_save()
                                            >
                                                "Rename"
                                            </button>
                                        </div>
                                        <label for="new-sandbox-name">"New sandbox"</label>
                                        <div class="settings-row">
                                            <input
                                                id="new-sandbox-name"
                                                name="new-sandbox-name"
                                                placeholder="Workspace name"
                                                prop:value=move || new_sandbox_name.get()
                                                on:input=move |ev| new_sandbox_name.set(event_target_value(&ev))
                                            />
                                            <button class="btn primary" on:click=move |_| create_new_sandbox()>
                                                "Create"
                                            </button>
                                        </div>
                                        <label>"Danger zone"</label>
                                        <div class="settings-row">
                                            <button class="btn" on:click=move |_| reset_target()>"Reset"</button>
                                            <button class="btn" on:click=move |_| compact_target()>"Compact"</button>
                                            <button class="btn danger" on:click=move |_| {
                                                let id = settings_target();
                                                if !id.is_empty() {
                                                    settings_open.set(false);
                                                    delete_session(id);
                                                }
                                            }>"Delete"</button>
                                        </div>
                                    </div>
                                </aside>
                            </>
                        }
                    })
                }}
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

async fn load_sandboxes_async(sandboxes: RwSignal<Vec<SandboxSummary>>) {
    if let Ok((_, v)) = api::get("/api/sandboxes").await
        && let Some(list) = v.get("sandboxes").and_then(|x| x.as_array())
    {
        sandboxes.set(
            list.iter()
                .filter_map(|s| serde_json::from_value::<SandboxSummary>(s.clone()).ok())
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
        // The thinking block is closed when text begins (interval duration).
        assert_eq!(entries[0].times.get_untracked(), (1000, 1100));
        assert_eq!(entries[1].id, 2);
        match &entries[1].block {
            UiBlock::Text { text } => assert_eq!(text.get_untracked(), "Hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn close_open_assistant_and_finish_user_stamp_their_targets() {
        let messages = RwSignal::new(Vec::<MsgEntry>::new());
        messages.set(vec![
            MsgEntry {
                id: 1,
                times: RwSignal::new((10, 0)),
                block: UiBlock::User {
                    content: "hi".into(),
                },
            },
            MsgEntry {
                id: 2,
                times: RwSignal::new((20, 0)),
                block: text_block_signal(),
            },
            MsgEntry {
                id: 3,
                times: RwSignal::new((30, 35)),
                block: text_block_signal(),
            },
        ]);

        // Closes only the open non-user block; leaves the finished one and the
        // user block alone.
        close_open_assistant(&messages, 50);
        let entries = messages.get_untracked();
        assert_eq!(entries[0].times.get_untracked(), (10, 0), "user untouched");
        assert_eq!(entries[1].times.get_untracked(), (20, 50));
        assert_eq!(entries[2].times.get_untracked(), (30, 35));

        // finish_user stamps the user block (turn duration).
        finish_user(&messages, 90);
        let entries = messages.get_untracked();
        assert_eq!(entries[0].times.get_untracked(), (10, 90));
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

        // Each tool event becomes its own block: tool-use (name + arguments)
        // and tool-result, both plain text blocks like thinking/text.
        assert_eq!(entries.len(), 5);
        assert!(
            entries
                .iter()
                .zip(entries.iter().skip(1))
                .all(|(a, b)| a.id < b.id)
        );

        assert!(matches!(entries[0].block, UiBlock::User { ref content } if content == "hi"));
        assert_eq!(entries[0].times.get_untracked(), (1000, 1000));

        match &entries[2].block {
            UiBlock::Tool { text } => {
                assert_eq!(text.get_untracked(), "time {}", "name + arguments");
            }
            other => panic!("expected tool block, got {other:?}"),
        }
        match &entries[3].block {
            UiBlock::Tool { text } => {
                assert_eq!(text.get_untracked(), "→ 12:00:00", "separate result block");
            }
            other => panic!("expected tool block, got {other:?}"),
        }
    }

    #[test]
    fn build_history_marks_error_results() {
        let mut v = history_fixture();
        v["messages"][3]["text"] = json!("boom");
        v["messages"][3]["is_error"] = json!(true);
        let next_id = RwSignal::new(1u64);
        let entries = build_history(&v, &next_id);
        match &entries[3].block {
            UiBlock::Tool { text } => {
                assert_eq!(text.get_untracked(), "→ error: boom");
            }
            other => panic!("expected tool-result block, got {other:?}"),
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
    fn scroll_update_never_resurrects_on_drag_start_at_bottom() {
        // Starting a touch drag while already at the latest (follow off, but
        // at_latest still true) must not re-engage follow on the first tiny
        // near-bottom scroll event.
        assert_eq!(scroll_update(1000.0, 0.0, true, false, true), (true, false));
    }

    #[test]
    fn fmt_duration_renders_always_with_ms_precision() {
        assert_eq!(fmt_duration(0, 0), "0.000s");
        assert_eq!(fmt_duration(1000, 2500), "1.500s");
        assert_eq!(fmt_duration(1000, 2045), "1.045s");
        // finished before created is clamped to zero.
        assert_eq!(fmt_duration(2000, 1500), "0.000s");
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
            on_turn_done: None,
        }
    }

    fn ev(event: &str, data: &str) -> sse::SseEvent {
        sse::SseEvent {
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    #[test]
    fn apply_stream_event_creates_and_fills_tool_blocks() {
        let st = chat_signals();
        assert!(matches!(
            apply_stream_event(&st, 1000, &ev("tool_use", r#"{"id":"c1","name":"time"}"#)),
            EventOutcome::Silent
        ));
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
                r#"{"id":"c1","result_preview":"12:00","is_error":true,"created_at_ms":1500,"finished_at_ms":1900}"#,
            ),
        );

        // tool_use + tool_args merge into one block; tool_result gets its own.
        let entries = st.messages.get_untracked();
        assert_eq!(entries.len(), 2, "one block for use+args, one for result");

        // tool-use duration = time to yield (tool_use -> tool_args): 100ms.
        assert_eq!(entries[0].times.get_untracked(), (1000, 1100));
        // tool-result duration = the server-measured invocation span: 400ms.
        assert_eq!(entries[1].times.get_untracked(), (1500, 1900));

        let texts: Vec<String> = entries
            .iter()
            .map(|e| match &e.block {
                UiBlock::Tool { text } => text.get_untracked(),
                other => panic!("expected tool block, got {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["time {}".to_string(), "→ error: 12:00".to_string()],
            "tool_use+tool_args share a block, tool_result is separate"
        );
    }

    /// The SSE layer double-encodes object payloads as a JSON string literal
    /// (parsed by `parse_event_object`), so `ev.data` is a JSON *string*
    /// whose content is the tool object. Rebuild that exact wire shape with
    /// `serde_json::to_string` and verify fields survive.
    #[test]
    fn apply_stream_event_decodes_double_encoded_tool_payloads() {
        // Wrap an object once to get its JSON text, then wrap again to
        // reproduce the SSE framing: a JSON string containing JSON text.
        let wire =
            |obj: Value| serde_json::to_string(&serde_json::to_string(&obj).unwrap()).unwrap();
        let st = chat_signals();
        apply_stream_event(
            &st,
            1000,
            &ev("tool_use", &wire(json!({"id": "c1", "name": "time"}))),
        );
        apply_stream_event(
            &st,
            1100,
            &ev("tool_args", &wire(json!({"id": "c1", "arguments": "{}"}))),
        );
        apply_stream_event(
            &st,
            1200,
            &ev(
                "tool_result",
                &wire(json!({"id": "c1", "result_preview": "12:00", "is_error": false})),
            ),
        );

        let entries = st.messages.get_untracked();
        assert_eq!(entries.len(), 2, "one block for use+args, one for result");
        let texts: Vec<String> = entries
            .iter()
            .map(|e| match &e.block {
                UiBlock::Tool { text } => text.get_untracked(),
                other => panic!("expected tool block, got {other:?}"),
            })
            .collect();
        assert_eq!(
            texts,
            vec!["time {}".to_string(), "→ 12:00".to_string()],
            "double-encoded payloads still decode into readable blocks"
        );
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
        close_open_assistant(&st.messages, finished_ms);
        finish_user(&st.messages, finished_ms);
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
