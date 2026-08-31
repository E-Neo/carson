use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::drivers::Usage;
use crate::registry::{AgentDef, ProviderDef, ToolDef};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    model TEXT NOT NULL,
    instances INTEGER NOT NULL DEFAULT 1,
    max_history INTEGER NOT NULL DEFAULT 40,
    context_window INTEGER NOT NULL DEFAULT 128000,
    compaction_ratio REAL NOT NULL DEFAULT 0.8,
    auto_compact INTEGER NOT NULL DEFAULT 1,
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agents_name ON agents(name);
CREATE TABLE IF NOT EXISTS agent_names (
    name TEXT PRIMARY KEY,
    current_id TEXT NOT NULL REFERENCES agents(id)
);
CREATE TABLE IF NOT EXISTS providers (
    name TEXT PRIMARY KEY,
    base_url TEXT NOT NULL,
    api_key TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tools (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL DEFAULT '',
    parameters_json TEXT NOT NULL DEFAULT '{}',
    env_json TEXT NOT NULL DEFAULT '{}',
    wasm BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    agent_name TEXT NOT NULL,
    agent_version_id TEXT NOT NULL,
    name TEXT,
    sandbox_id TEXT,
    summary TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sandboxes (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS messages (
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    agent_version_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    finished_at INTEGER,
    PRIMARY KEY (session_id, seq)
);
"#;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Add a column to an existing table if it is not present yet. SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, so existing databases are upgraded here.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, ddl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    if !columns.iter().any(|c| c == column) {
        conn.execute_batch(ddl)?;
    }
    Ok(())
}

/// Apply schema upgrades to an already-created connection.
fn migrate(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "sessions",
        "name",
        "ALTER TABLE sessions ADD COLUMN name TEXT",
    )?;
    add_column_if_missing(
        conn,
        "sessions",
        "sandbox_id",
        "ALTER TABLE sessions ADD COLUMN sandbox_id TEXT",
    )?;
    Ok(())
}

/// One entry of the persisted conversation block log (mirrors the WIT `block`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredBlock {
    pub agent_version_id: String,
    pub kind: String,
    /// Plain text for user/thinking/text/system; a JSON payload for tool
    /// kinds (`{"id","name","arguments"}` / `{"id","name","output","is_error"}`).
    pub text: Option<String>,
    pub input_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub output_tokens: u32,
    pub created_at_ms: u64,
    pub finished_at_ms: u64,
}

impl From<&crate::bindings::exports::carson::agent::agent::Block> for StoredBlock {
    fn from(b: &crate::bindings::exports::carson::agent::agent::Block) -> Self {
        Self {
            agent_version_id: b.agent_version_id.clone(),
            kind: b.kind.clone(),
            text: b.text.clone(),
            input_tokens: b.input_tokens,
            cache_read_tokens: b.cache_read_tokens,
            cache_creation_tokens: b.cache_creation_tokens,
            output_tokens: b.output_tokens,
            created_at_ms: b.created_at_ms,
            finished_at_ms: b.finished_at_ms,
        }
    }
}

impl From<&StoredBlock> for crate::bindings::exports::carson::agent::agent::Block {
    fn from(b: &StoredBlock) -> Self {
        Self {
            agent_version_id: b.agent_version_id.clone(),
            kind: b.kind.clone(),
            text: b.text.clone(),
            input_tokens: b.input_tokens,
            cache_read_tokens: b.cache_read_tokens,
            cache_creation_tokens: b.cache_creation_tokens,
            output_tokens: b.output_tokens,
            created_at_ms: b.created_at_ms,
            finished_at_ms: b.finished_at_ms,
        }
    }
}

/// A persisted session snapshot (used to restore an agent session on boot).
#[derive(Debug, Clone)]
pub struct PersistedSession {
    pub id: String,
    pub agent_name: String,
    pub agent_version_id: String,
    /// User-facing session name; `None` until the user renames it.
    pub name: Option<String>,
    /// The sandbox this session's tools operate in; backfilled on restore for
    /// sessions created before sandboxes existed.
    pub sandbox_id: Option<String>,
    pub updated_at: i64,
    pub summary: Option<String>,
    pub usage: Usage,
    pub messages: Vec<StoredBlock>,
}

/// One sandbox in the pool. `id` is also the directory name under
/// `$CARSON_HOME/sandbox/<id>`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Sandbox {
    pub id: String,
    pub name: String,
    pub created_at: i64,
}

/// Columns of one message row, kept together for the load query.
struct MessageRow(StoredBlock);

fn def_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentDef> {
    let caps: String = row.get(9)?;
    Ok(AgentDef {
        id: row.get(0)?,
        name: row.get(1)?,
        system_prompt: row.get(2)?,
        model: row.get(3)?,
        instances: row.get::<_, i64>(4)? as usize,
        max_history: row.get::<_, i64>(5)? as usize,
        context_window: row.get::<_, i64>(6)? as usize,
        compaction_ratio: row.get(7)?,
        auto_compact: row.get::<_, i64>(8)? != 0,
        capabilities: serde_json::from_str(&caps).unwrap_or_default(),
    })
}

fn tool_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolDef> {
    let parameters: String = row.get(3)?;
    let env: String = row.get(4)?;
    Ok(ToolDef {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        parameters: serde_json::from_str(&parameters).unwrap_or_default(),
        env: serde_json::from_str(&env).unwrap_or_default(),
    })
}

fn collect_tools<I>(rows: I) -> Result<Vec<ToolDef>>
where
    I: Iterator<Item = rusqlite::Result<ToolDef>>,
{
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

const AGENT_COLUMNS: &str = "a.id, a.name, a.system_prompt, a.model, a.instances, a.max_history, a.context_window, \
     a.compaction_ratio, a.auto_compact, a.capabilities_json";

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("initialize schema in {}", path.display()))?;
        migrate(&conn).with_context(|| format!("migrate schema in {}", path.display()))?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// In-memory database; used by tests across the workspace.
    #[doc(hidden)]
    pub fn open_in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// Current version of every named agent.
    pub fn list_agents(&self) -> Result<Vec<AgentDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a \
             JOIN agent_names n ON n.current_id = a.id ORDER BY a.name"
        ))?;
        let rows = stmt.query_map([], def_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every version of an agent, oldest first.
    pub fn list_agent_versions(&self, name: &str) -> Result<Vec<AgentDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a WHERE a.name = ?1 ORDER BY a.rowid"
        ))?;
        let rows = stmt.query_map(params![name], def_from_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get_agent_version(&self, id: &str) -> Result<Option<AgentDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a WHERE a.id = ?1"
        ))?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(def_from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn current_agent(&self, name: &str) -> Result<Option<AgentDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a JOIN agent_names n ON n.current_id = a.id \
             WHERE n.name = ?1"
        ))?;
        let mut rows = stmt.query(params![name])?;
        match rows.next()? {
            Some(row) => Ok(Some(def_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Insert an immutable agent version row. The caller supplies `def.id`.
    pub fn insert_agent_version(&self, def: &AgentDef) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, system_prompt, model, instances, max_history, \
             context_window, compaction_ratio, auto_compact, capabilities_json, created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                def.id,
                def.name,
                def.system_prompt,
                def.model,
                def.instances as i64,
                def.max_history as i64,
                def.context_window as i64,
                def.compaction_ratio,
                def.auto_compact as i64,
                serde_json::to_string(&def.capabilities)?,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Point `name` at `version_id` (which must already exist).
    pub fn set_current_agent(&self, name: &str, version_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_names (name, current_id) VALUES (?1, ?2) \
             ON CONFLICT(name) DO UPDATE SET current_id = ?2",
            params![name, version_id],
        )?;
        Ok(())
    }

    /// Remove only the name pointer; versions and their pinned sessions stay.
    pub fn delete_agent_pointer(&self, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM agent_names WHERE name = ?1", params![name])?;
        Ok(())
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
            "SELECT id, name, description, parameters_json, env_json FROM tools ORDER BY name",
        )?;
        collect_tools(stmt.query_map([], tool_from_row)?)
    }

    pub fn get_tool_wasm(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT wasm FROM tools WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn get_tool(&self, id: &str) -> Result<Option<ToolDef>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, parameters_json, env_json FROM tools WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => collect_tools(std::iter::once(tool_from_row(row))).map(|mut v| v.pop()),
            None => Ok(None),
        }
    }

    /// Insert a custom tool; the caller supplies a fresh uuid in `def.id`.
    pub fn insert_tool(&self, def: &ToolDef, wasm: &[u8]) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tools (id, name, description, parameters_json, env_json, wasm, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?7)",
            params![
                def.id,
                def.name,
                def.description,
                serde_json::to_string(&def.parameters)?,
                serde_json::to_string(&def.env)?,
                wasm,
                now
            ],
        )?;
        Ok(())
    }

    /// Update an existing custom tool's metadata/wasm by id.
    pub fn update_tool(&self, def: &ToolDef, wasm: &[u8]) -> Result<usize> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE tools SET name=?2, description=?3, parameters_json=?4, env_json=?5, \
             wasm=?6, updated_at=?7 WHERE id=?1",
            params![
                def.id,
                def.name,
                def.description,
                serde_json::to_string(&def.parameters)?,
                serde_json::to_string(&def.env)?,
                wasm,
                now
            ],
        )?)
    }

    pub fn delete_tool(&self, id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM tools WHERE id = ?1", params![id])?)
    }

    pub fn upsert_session(&self, session: &PersistedSession) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, agent_name, agent_version_id, name, sandbox_id, summary, \
             input_tokens, cache_read_tokens, cache_creation_tokens, output_tokens, created_at, \
             updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11) \
             ON CONFLICT(id) DO UPDATE SET agent_name=?2, agent_version_id=?3, summary=?6, \
             input_tokens=?7, cache_read_tokens=?8, cache_creation_tokens=?9, output_tokens=?10, \
             updated_at=?11",
            params![
                session.id,
                session.agent_name,
                session.agent_version_id,
                session.name,
                session.sandbox_id,
                session.summary,
                session.usage.input_tokens,
                session.usage.cache_read_tokens,
                session.usage.cache_creation_tokens,
                session.usage.output_tokens,
                now
            ],
        )?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session.id],
        )?;
        for (seq, block) in session.messages.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages (session_id, seq, agent_version_id, kind, content, \
                 input_tokens, cache_read_tokens, cache_creation_tokens, output_tokens, \
                 created_at, finished_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    session.id,
                    seq as i64,
                    // Per-block provenance: which agent version produced it.
                    block.agent_version_id,
                    block.kind,
                    block.text,
                    block.input_tokens,
                    block.cache_read_tokens,
                    block.cache_creation_tokens,
                    block.output_tokens,
                    block.created_at_ms as i64,
                    block.finished_at_ms as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_sessions(&self) -> Result<Vec<PersistedSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.agent_name, s.agent_version_id, s.name, s.sandbox_id, s.summary, \
             s.input_tokens, s.cache_read_tokens, s.cache_creation_tokens, s.output_tokens, \
             s.updated_at, \
             m.seq, m.agent_version_id, m.kind, m.content, m.input_tokens, \
             m.cache_read_tokens, m.cache_creation_tokens, \
             m.output_tokens, m.created_at, m.finished_at \
             FROM sessions s LEFT JOIN messages m ON m.session_id = s.id \
             ORDER BY s.rowid, m.seq",
        )?;
        let rows = stmt.query_map([], |row| {
            let block = if row.get::<_, Option<i64>>(11)?.is_some() {
                Some(MessageRow(StoredBlock {
                    agent_version_id: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                    kind: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                    text: row.get(14)?,
                    input_tokens: row.get::<_, i64>(15)? as u32,
                    cache_read_tokens: row.get::<_, i64>(16)? as u32,
                    cache_creation_tokens: row.get::<_, i64>(17)? as u32,
                    output_tokens: row.get::<_, i64>(18)? as u32,
                    created_at_ms: row.get::<_, Option<i64>>(19)?.unwrap_or(0) as u64,
                    finished_at_ms: row.get::<_, Option<i64>>(20)?.unwrap_or(0) as u64,
                }))
            } else {
                None
            };
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)? as u32,
                row.get::<_, i64>(7)? as u32,
                row.get::<_, i64>(8)? as u32,
                row.get::<_, i64>(9)? as u32,
                row.get::<_, i64>(10)?,
                block,
            ))
        })?;

        let mut sessions: Vec<PersistedSession> = Vec::new();
        let mut index_of: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for row in rows {
            let (
                id,
                agent_name,
                agent_version_id,
                name,
                sandbox_id,
                summary,
                input,
                cache_read,
                cache_creation,
                output,
                updated_at,
                block,
            ) = row?;
            let idx = match index_of.get(&id) {
                Some(idx) => *idx,
                None => {
                    sessions.push(PersistedSession {
                        id: id.clone(),
                        agent_name,
                        agent_version_id,
                        name,
                        sandbox_id,
                        updated_at,
                        summary,
                        usage: Usage {
                            input_tokens: input,
                            cache_read_tokens: cache_read,
                            cache_creation_tokens: cache_creation,
                            output_tokens: output,
                        },
                        messages: Vec::new(),
                    });
                    index_of.insert(id, sessions.len() - 1);
                    sessions.len() - 1
                }
            };
            if let Some(MessageRow(block)) = block {
                sessions[idx].messages.push(block);
            }
        }
        Ok(sessions)
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM messages WHERE session_id = ?1", params![id])?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// All sandboxes in the pool, ordered by name.
    pub fn list_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, name, created_at FROM sandboxes ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok(Sandbox {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Create a sandbox and return it.
    pub fn insert_sandbox(&self, id: &str, name: &str) -> Result<Sandbox> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sandboxes (id, name, created_at, updated_at) VALUES (?1,?2,?3,?3)",
            params![id, name, now],
        )?;
        Ok(Sandbox {
            id: id.to_string(),
            name: name.to_string(),
            created_at: now,
        })
    }

    /// Rename a sandbox's display alias.
    pub fn rename_sandbox(&self, id: &str, name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sandboxes SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now_ms()],
        )?;
        Ok(())
    }

    /// The display name of a sandbox, if it exists.
    pub fn sandbox_name(&self, id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT name FROM sandboxes WHERE id = ?1")?;
        let mut rows = stmt.query_map(params![id], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    /// Update a session's display name. `None` clears it.
    pub fn set_session_name(&self, id: &str, name: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET name = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, name, now_ms()],
        )?;
        Ok(())
    }

    /// Point a session at a different sandbox.
    pub fn set_session_sandbox(&self, id: &str, sandbox_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET sandbox_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, sandbox_id, now_ms()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str) -> AgentDef {
        AgentDef {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            system_prompt: "sys".into(),
            model: "mock".into(),
            instances: 1,
            max_history: 40,
            context_window: 128_000,
            compaction_ratio: 0.8,
            auto_compact: true,
            capabilities: vec!["core/time".into()],
        }
    }

    fn block(version: &str, kind: &str, text: &str) -> StoredBlock {
        StoredBlock {
            agent_version_id: version.into(),
            kind: kind.into(),
            text: Some(text.into()),
            input_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 0,
            created_at_ms: 1_000,
            finished_at_ms: 2_000,
        }
    }

    fn session(
        id: &str,
        agent_name: &str,
        version: &str,
        blocks: Vec<StoredBlock>,
    ) -> PersistedSession {
        PersistedSession {
            id: id.into(),
            agent_name: agent_name.into(),
            agent_version_id: version.into(),
            name: None,
            sandbox_id: Some(id.into()),
            updated_at: 5_000,
            summary: Some("summary".into()),
            usage: Usage {
                input_tokens: 10,
                cache_read_tokens: 2,
                cache_creation_tokens: 1,
                output_tokens: 5,
            },
            messages: blocks,
        }
    }

    fn create_agent(db: &Db, name: &str) -> AgentDef {
        let d = def(name);
        db.insert_agent_version(&d).unwrap();
        db.set_current_agent(name, &d.id).unwrap();
        d
    }

    #[test]
    fn agent_versions_and_pointers() {
        let db = Db::open_in_memory().unwrap();
        let v1 = create_agent(&db, "coder");
        let mut v2 = def("coder");
        v2.system_prompt = "changed".into();
        db.insert_agent_version(&v2).unwrap();
        db.set_current_agent("coder", &v2.id).unwrap();

        let current = db.list_agents().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, v2.id);
        assert_eq!(current[0].system_prompt, "changed");

        let versions = db.list_agent_versions("coder").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].id, v1.id);
        assert_eq!(db.current_agent("coder").unwrap().unwrap().id, v2.id);
        assert!(db.current_agent("nope").unwrap().is_none());

        // Deleting the pointer keeps both version rows.
        db.delete_agent_pointer("coder").unwrap();
        assert!(db.list_agents().unwrap().is_empty());
        assert_eq!(db.list_agent_versions("coder").unwrap().len(), 2);
    }

    #[test]
    fn session_block_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let agent = create_agent(&db, "coder");
        let mut thinking = block(&agent.id, "thinking", "let me reason");
        thinking.input_tokens = 7;
        thinking.output_tokens = 3;
        let call = block(
            &agent.id,
            "tool-use",
            r#"{"id":"c1","name":"time","arguments":"{}"}"#,
        );
        let result = block(
            &agent.id,
            "tool-result",
            r#"{"id":"c1","name":"time","output":"{\"time\":\"2026-01-01T00:00:00.000Z\"}","is_error":false}"#,
        );

        let persisted = session(
            "sess-1",
            "coder",
            &agent.id,
            vec![
                block("v0", "user", "hi"),
                thinking,
                block(&agent.id, "text", "hello"),
                call,
                result,
            ],
        );
        db.upsert_session(&persisted).unwrap();

        let loaded = db.load_sessions().unwrap();
        assert_eq!(loaded.len(), 1);
        let s = &loaded[0];
        assert_eq!(s.id, "sess-1");
        assert_eq!(s.agent_name, "coder");
        assert_eq!(s.agent_version_id, agent.id);
        assert_eq!(s.summary.as_deref(), Some("summary"));
        assert_eq!(s.messages.len(), 5);
        assert_eq!(s.messages[0].kind, "user");
        assert_eq!(s.messages[0].text.as_deref(), Some("hi"));
        assert_eq!(s.messages[1].kind, "thinking");
        assert_eq!(s.messages[1].input_tokens, 7);
        // Per-block provenance: the "user" block kept its legacy stamp while
        // the assistant-side blocks carry the creating version.
        assert_eq!(s.messages[0].agent_version_id, "v0");
        assert_eq!(s.messages[1].agent_version_id, agent.id);

        // Tool payloads decode out of the content JSON.
        let call_payload: serde_json::Value =
            serde_json::from_str(s.messages[3].text.as_deref().unwrap()).unwrap();
        assert_eq!(call_payload["name"], "time");
        assert_eq!(call_payload["arguments"], "{}");
        let result_payload: serde_json::Value =
            serde_json::from_str(s.messages[4].text.as_deref().unwrap()).unwrap();
        assert_eq!(result_payload["is_error"], false);
        assert!(result_payload["output"].as_str().unwrap().contains("2026-"));
        assert_eq!(s.usage.input_tokens, 10);
    }

    #[test]
    fn upsert_session_replaces_blocks() {
        let db = Db::open_in_memory().unwrap();
        let agent = create_agent(&db, "coder");
        db.upsert_session(&session(
            "s",
            "coder",
            &agent.id,
            vec![block("v", "user", "a")],
        ))
        .unwrap();
        db.upsert_session(&session(
            "s",
            "coder",
            &agent.id,
            vec![block("v", "user", "a"), block("v", "user", "b")],
        ))
        .unwrap();
        let loaded = db.load_sessions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 2);
    }

    #[test]
    fn delete_session_removes_rows() {
        let db = Db::open_in_memory().unwrap();
        let agent = create_agent(&db, "coder");
        db.upsert_session(&session(
            "s",
            "coder",
            &agent.id,
            vec![block("v", "user", "x")],
        ))
        .unwrap();
        db.delete_session("s").unwrap();
        assert!(db.load_sessions().unwrap().is_empty());
    }

    #[test]
    fn delete_pointer_keeps_pinned_sessions() {
        let db = Db::open_in_memory().unwrap();
        let agent = create_agent(&db, "coder");
        db.upsert_session(&session("s", "coder", &agent.id, vec![]))
            .unwrap();
        db.delete_agent_pointer("coder").unwrap();
        let loaded = db.load_sessions().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].agent_name, "coder");
        // The version row still resolves for restore.
        assert!(db.get_agent_version(&agent.id).unwrap().is_some());
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
    fn tool_insert_get_delete_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let def = ToolDef {
            id: uuid::Uuid::new_v4().to_string(),
            name: "websearch".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object"}),
            env: [("KEY".to_string(), "value".to_string())]
                .into_iter()
                .collect(),
        };
        db.insert_tool(&def, b"wasm-bytes").unwrap();
        let tools = db.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].id, def.id);
        assert_eq!(tools[0].name, "websearch");
        assert_eq!(tools[0].description, "Search the web");
        assert_eq!(tools[0].env["KEY"], "value");
        assert_eq!(db.get_tool_wasm(&def.id).unwrap().unwrap(), b"wasm-bytes");
        let fetched = db.get_tool(&def.id).unwrap().unwrap();
        assert_eq!(fetched.name, "websearch");
        assert_eq!(db.delete_tool(&def.id).unwrap(), 1);
        assert!(db.list_tools().unwrap().is_empty());
    }
}
