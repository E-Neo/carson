use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::host::HostContext;
use crate::hub::Hub;
use crate::registry::{AgentInstance, AgentRegistry};

#[derive(Clone)]
pub struct AppState {
    pub ctx: Arc<HostContext>,
    pub registry: Arc<Mutex<AgentRegistry>>,
    pub db: Arc<Db>,
    pub hub: Arc<Hub>,
    /// Live sessions keyed by uuid. Each entry pins the agent version that
    /// created it; agent edits never move existing sessions off their version.
    pub sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    pub cfg: Arc<Config>,
}

#[derive(Clone)]
pub struct SessionEntry {
    pub agent_name: String,
    pub agent_version_id: String,
    pub name: Option<String>,
    pub sandbox_id: String,
    /// Last activity (message, rename or sandbox switch) in ms since epoch.
    /// Drives the session list ordering.
    pub updated_at: i64,
    pub instance: Arc<AgentInstance>,
}

pub fn build_app_state(
    ctx: Arc<HostContext>,
    registry: AgentRegistry,
    db: Arc<Db>,
    config: Config,
) -> AppState {
    AppState {
        hub: ctx.hub.clone(),
        ctx,
        registry: Arc::new(Mutex::new(registry)),
        db,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        cfg: Arc::new(config),
    }
}
