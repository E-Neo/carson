use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

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
    pub sessions: Arc<Mutex<HashMap<u64, SessionEntry>>>,
    pub next_session_id: Arc<AtomicU64>,
    pub cfg: Arc<Config>,
}

#[derive(Clone)]
pub struct SessionEntry {
    pub kind: String,
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
        next_session_id: Arc::new(AtomicU64::new(0)),
        cfg: Arc::new(config),
    }
}
