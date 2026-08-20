use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::drivers::Usage;
use crate::registry::{AgentDef, ProviderDef, ToolDef};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    kind TEXT PRIMARY KEY,
    system_prompt TEXT NOT NULL,
    model TEXT NOT NULL,
    instances INTEGER NOT NULL DEFAULT 1,
    max_history INTEGER NOT NULL DEFAULT 40,
    context_window INTEGER NOT NULL DEFAULT 128000,
    compaction_ratio REAL NOT NULL DEFAULT 0.8,
    auto_compact INTEGER NOT NULL DEFAULT 1,
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS providers (
    name TEXT PRIMARY KEY,
    base_url TEXT NOT NULL,
    api_key TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tools (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    parameters_json TEXT NOT NULL DEFAULT '{}',
    env_json TEXT NOT NULL DEFAULT '{}',
    wasm BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    summary TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
    session_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_calls_json TEXT,
    tool_call_id TEXT,
    PRIMARY KEY (session_id, seq)
);
"#;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Apply additive schema changes to databases created by older binaries.
fn migrate(conn: &Connection) -> Result<()> {
    let has_column = |table: &str, column: &str| -> Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_column("providers", "api_key")? {
        conn.execute("ALTER TABLE providers ADD COLUMN api_key TEXT", [])
            .context("migrate providers.api_key")?;
    }
    // Drop the legacy env-var column when the sqlite version supports it.
    if has_column("providers", "api_key_env")? {
        let _ = conn.execute("ALTER TABLE providers DROP COLUMN api_key_env", []);
    }
    Ok(())
}

/// A serializable conversation message (mirrors the WIT `message` record).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<StoredToolCall>>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl From<&crate::bindings::carson::agent::llm::Message> for StoredMessage {
    fn from(m: &crate::bindings::carson::agent::llm::Message) -> Self {
        Self {
            role: m.role.clone(),
            content: m.content.clone(),
            tool_calls: m.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| StoredToolCall {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments_json.clone(),
                    })
                    .collect()
            }),
            tool_call_id: m.tool_call_id.clone(),
        }
    }
}

impl From<StoredMessage> for crate::bindings::carson::agent::llm::Message {
    fn from(m: StoredMessage) -> Self {
        Self {
            role: m.role,
            content: m.content,
            tool_calls: m.tool_calls.map(|calls| {
                calls
                    .into_iter()
                    .map(|tc| crate::bindings::carson::agent::llm::ToolCall {
                        id: tc.id,
                        name: tc.name,
                        arguments_json: tc.arguments,
                    })
                    .collect()
            }),
            tool_call_id: m.tool_call_id,
        }
    }
}

/// A persisted session snapshot (used to restore an agent session on boot).
#[derive(Debug, Clone)]
pub struct PersistedSession {
    pub id: u64,
    pub kind: String,
    pub summary: Option<String>,
    pub usage: Usage,
    pub messages: Vec<StoredMessage>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("initialize schema in {}", path.display()))?;
        migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    pub fn open_in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    pub fn list_agents(&self) -> Result<Vec<AgentDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT kind, system_prompt, model, instances, max_history, context_window, \
             compaction_ratio, auto_compact, capabilities_json FROM agents ORDER BY kind",
        )?;
        let rows = stmt.query_map([], |row| {
            let caps: String = row.get(8)?;
            Ok(AgentDef {
                kind: row.get(0)?,
                system_prompt: row.get(1)?,
                model: row.get(2)?,
                instances: row.get::<_, i64>(3)? as usize,
                max_history: row.get::<_, i64>(4)? as usize,
                context_window: row.get::<_, i64>(5)? as usize,
                compaction_ratio: row.get(6)?,
                auto_compact: row.get::<_, i64>(7)? != 0,
                capabilities: serde_json::from_str(&caps).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn insert_agent(&self, def: &AgentDef) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (kind, system_prompt, model, instances, max_history, context_window, \
             compaction_ratio, auto_compact, capabilities_json, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10) \
             ON CONFLICT(kind) DO UPDATE SET system_prompt=?2, model=?3, instances=?4, \
             max_history=?5, context_window=?6, compaction_ratio=?7, auto_compact=?8, \
             capabilities_json=?9, updated_at=?10",
            params![
                def.kind,
                def.system_prompt,
                def.model,
                def.instances as i64,
                def.max_history as i64,
                def.context_window as i64,
                def.compaction_ratio,
                def.auto_compact as i64,
                serde_json::to_string(&def.capabilities)?,
                now,
            ],
        )?;
        Ok(())
    }

    /// Delete an agent kind and cascade-delete its sessions (and their messages).
    pub fn delete_agent(&self, kind: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id IN (SELECT id FROM sessions WHERE kind = ?1)",
            params![kind],
        )?;
        let removed = tx.execute("DELETE FROM sessions WHERE kind = ?1", params![kind])?;
        tx.execute("DELETE FROM agents WHERE kind = ?1", params![kind])?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn list_providers(&self) -> Result<Vec<ProviderDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT name, base_url, api_key FROM providers ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(ProviderDef {
                name: row.get(0)?,
                base_url: row.get(1)?,
                api_key: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn upsert_provider(&self, def: &ProviderDef) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO providers (name, base_url, api_key, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?4) \
             ON CONFLICT(name) DO UPDATE SET base_url=?2, api_key=?3, updated_at=?4",
            params![def.name, def.base_url, def.api_key, now],
        )?;
        Ok(())
    }

    pub fn delete_provider(&self, name: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM providers WHERE name = ?1", params![name])?)
    }

    pub fn list_tools(&self) -> Result<Vec<ToolDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, description, parameters_json, env_json FROM tools ORDER BY name",
        )?;
        let rows = stmt.query_map([], |row| {
            let params: String = row.get(2)?;
            let env: String = row.get(3)?;
            Ok(ToolDef {
                name: row.get(0)?,
                description: row.get(1)?,
                parameters: serde_json::from_str(&params).unwrap_or_default(),
                env: serde_json::from_str(&env).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_tool_wasm(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT wasm FROM tools WHERE name = ?1")?;
        let mut rows = stmt.query(params![name])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn get_tool_def(&self, name: &str) -> Result<Option<ToolDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, description, parameters_json, env_json FROM tools WHERE name = ?1",
        )?;
        let mut rows = stmt.query(params![name])?;
        match rows.next()? {
            Some(row) => {
                let params: String = row.get(2)?;
                let env: String = row.get(3)?;
                Ok(Some(ToolDef {
                    name: row.get(0)?,
                    description: row.get(1)?,
                    parameters: serde_json::from_str(&params).unwrap_or_default(),
                    env: serde_json::from_str(&env).unwrap_or_default(),
                }))
            }
            None => Ok(None),
        }
    }

    pub fn upsert_tool(&self, def: &ToolDef, wasm: &[u8]) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tools (name, description, parameters_json, env_json, wasm, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?6) \
             ON CONFLICT(name) DO UPDATE SET description=?2, parameters_json=?3, env_json=?4, \
             wasm=?5, updated_at=?6",
            params![
                def.name,
                def.description,
                serde_json::to_string(&def.parameters)?,
                serde_json::to_string(&def.env)?,
                wasm,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn delete_tool(&self, name: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM tools WHERE name = ?1", params![name])?)
    }

    pub fn upsert_session(&self, session: &PersistedSession) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, kind, summary, input_tokens, cache_read_tokens, \
             cache_creation_tokens, output_tokens, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8) \
             ON CONFLICT(id) DO UPDATE SET kind=?2, summary=?3, input_tokens=?4, \
             cache_read_tokens=?5, cache_creation_tokens=?6, output_tokens=?7, updated_at=?8",
            params![
                session.id as i64,
                session.kind,
                session.summary,
                session.usage.input_tokens,
                session.usage.cache_read_tokens,
                session.usage.cache_creation_tokens,
                session.usage.output_tokens,
                now,
            ],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session.id as i64],
        )?;
        for (seq, message) in session.messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages (session_id, seq, role, content, tool_calls_json, tool_call_id) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    session.id as i64,
                    seq as i64,
                    message.role,
                    message.content,
                    message
                        .tool_calls
                        .as_ref()
                        .map(|calls| serde_json::to_string(calls).unwrap_or_else(|_| "[]".into())),
                    message.tool_call_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_sessions(&self) -> Result<Vec<PersistedSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.kind, s.summary, s.input_tokens, s.cache_read_tokens, \
             s.cache_creation_tokens, s.output_tokens, m.seq, m.role, m.content, \
             m.tool_calls_json, m.tool_call_id \
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id \
             ORDER BY s.id, m.seq",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)? as u32,
                row.get::<_, i64>(4)? as u32,
                row.get::<_, i64>(5)? as u32,
                row.get::<_, i64>(6)? as u32,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;

        let mut sessions: Vec<PersistedSession> = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                summary,
                input,
                cache_read,
                cache_creation,
                output,
                seq,
                role,
                content,
                tool_calls,
                tool_call_id,
            ) = row?;
            let messages = &mut sessions.iter_mut().find(|s| s.id == id);
            match messages {
                Some(session) => {
                    if let Some(seq) = seq {
                        session.messages.resize(
                            seq as usize + 1,
                            StoredMessage {
                                role: String::new(),
                                content: None,
                                tool_calls: None,
                                tool_call_id: None,
                            },
                        );
                        session.messages[seq as usize] = StoredMessage {
                            role: role.unwrap_or_default(),
                            content,
                            tool_calls: tool_calls.and_then(|j| serde_json::from_str(&j).ok()),
                            tool_call_id,
                        };
                    }
                }
                None => {
                    let mut session = PersistedSession {
                        id,
                        kind,
                        summary,
                        usage: Usage {
                            input_tokens: input,
                            cache_read_tokens: cache_read,
                            cache_creation_tokens: cache_creation,
                            output_tokens: output,
                        },
                        messages: Vec::new(),
                    };
                    if let Some(seq) = seq {
                        session.messages.resize(
                            seq as usize + 1,
                            StoredMessage {
                                role: String::new(),
                                content: None,
                                tool_calls: None,
                                tool_call_id: None,
                            },
                        );
                        session.messages[seq as usize] = StoredMessage {
                            role: role.unwrap_or_default(),
                            content,
                            tool_calls: tool_calls.and_then(|j| serde_json::from_str(&j).ok()),
                            tool_call_id,
                        };
                    }
                    sessions.push(session);
                }
            }
        }
        Ok(sessions)
    }

    pub fn delete_session(&self, id: u64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![id as i64],
        )?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id as i64])?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(kind: &str) -> AgentDef {
        AgentDef {
            kind: kind.into(),
            system_prompt: "sys".into(),
            model: "mock".into(),
            instances: 1,
            max_history: 40,
            context_window: 128_000,
            compaction_ratio: 0.8,
            auto_compact: true,
            capabilities: vec!["time".into()],
        }
    }

    fn msg(role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn session(id: u64, kind: &str, messages: Vec<StoredMessage>) -> PersistedSession {
        PersistedSession {
            id,
            kind: kind.into(),
            summary: Some("summary".into()),
            usage: Usage {
                input_tokens: 10,
                cache_read_tokens: 2,
                cache_creation_tokens: 1,
                output_tokens: 5,
            },
            messages,
        }
    }

    #[test]
    fn agent_upsert_and_list() {
        let db = Db::open_in_memory().unwrap();
        db.insert_agent(&def("coder")).unwrap();
        let mut changed = def("coder");
        changed.system_prompt = "changed".into();
        db.insert_agent(&changed).unwrap();
        let agents = db.list_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].system_prompt, "changed");
        assert_eq!(agents[0].capabilities, vec!["time"]);
    }

    #[test]
    fn session_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let persisted = session(
            1,
            "coder",
            vec![msg("user", "hi"), msg("assistant", "hello")],
        );
        db.upsert_session(&persisted).unwrap();
        let loaded = db.load_sessions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, 1);
        assert_eq!(loaded[0].summary.as_deref(), Some("summary"));
        assert_eq!(loaded[0].messages.len(), 2);
        assert_eq!(loaded[0].messages[0].role, "user");
        assert_eq!(loaded[0].messages[0].content.as_deref(), Some("hi"));
        assert_eq!(loaded[0].messages[1].role, "assistant");
        assert_eq!(loaded[0].usage.input_tokens, 10);
        assert_eq!(loaded[0].usage.cache_read_tokens, 2);
        assert_eq!(loaded[0].usage.cache_creation_tokens, 1);
    }

    #[test]
    fn upsert_session_replaces_messages() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_session(&session(1, "coder", vec![msg("user", "a")]))
            .unwrap();
        db.upsert_session(&session(
            1,
            "coder",
            vec![msg("user", "a"), msg("user", "b")],
        ))
        .unwrap();
        let loaded = db.load_sessions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 2);
    }

    #[test]
    fn delete_agent_cascades_sessions() {
        let db = Db::open_in_memory().unwrap();
        db.insert_agent(&def("coder")).unwrap();
        db.upsert_session(&session(1, "coder", vec![msg("user", "x")]))
            .unwrap();
        db.upsert_session(&session(2, "coder", vec![])).unwrap();
        let removed = db.delete_agent("coder").unwrap();
        assert_eq!(removed, 2);
        assert!(db.load_sessions().unwrap().is_empty());
        assert!(db.list_agents().unwrap().is_empty());
    }

    #[test]
    fn delete_agent_keeps_other_kinds() {
        let db = Db::open_in_memory().unwrap();
        db.insert_agent(&def("coder")).unwrap();
        db.insert_agent(&def("researcher")).unwrap();
        db.upsert_session(&session(1, "coder", vec![])).unwrap();
        db.upsert_session(&session(2, "researcher", vec![]))
            .unwrap();
        db.delete_agent("coder").unwrap();
        let agents = db.list_agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].kind, "researcher");
        let sessions = db.load_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].kind, "researcher");
    }

    #[test]
    fn provider_upsert_and_list() {
        let db = Db::open_in_memory().unwrap();
        let def = ProviderDef {
            name: "groq".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: Some("gsk-secret".into()),
        };
        db.upsert_provider(&def).unwrap();
        let mut changed = def.clone();
        changed.base_url = "https://changed.example".into();
        db.upsert_provider(&changed).unwrap();
        let providers = db.list_providers().unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].base_url, "https://changed.example");
        assert_eq!(providers[0].api_key.as_deref(), Some("gsk-secret"));
        assert_eq!(db.delete_provider("groq").unwrap(), 1);
        assert!(db.list_providers().unwrap().is_empty());
    }

    #[test]
    fn tool_upsert_list_and_wasm_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let def = ToolDef {
            name: "custom/websearch".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object"}),
            env: [("KEY".to_string(), "value".to_string())]
                .into_iter()
                .collect(),
        };
        db.upsert_tool(&def, b"wasm-bytes").unwrap();
        let tools = db.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "custom/websearch");
        assert_eq!(tools[0].description, "Search the web");
        assert_eq!(tools[0].env["KEY"], "value");
        assert_eq!(
            db.get_tool_wasm("custom/websearch").unwrap().unwrap(),
            b"wasm-bytes"
        );
        assert_eq!(db.delete_tool("custom/websearch").unwrap(), 1);
        assert!(db.list_tools().unwrap().is_empty());
    }
}
