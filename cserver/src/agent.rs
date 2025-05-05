use crate::api::dto::AgentQuery;
use crate::{AppState, WsSink};
use axum::{extract::Query, extract::State, Json};
use dashmap::DashMap;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use utoipa::ToSchema;

// エージェント接続情報（WebSocket Sinkとメタデータ）
// SinkはMutexで保護され、スレッドセーフなアクセスを保証
pub(crate) struct AgentConnection {
    pub sink: Mutex<WsSink>,
    pub metadata: AgentMetadata,
}

// エージェントのメタデータ
#[derive(Debug, Clone)]
#[allow(dead_code)] // 将来的に使用する可能性のあるフィールドの警告抑制
pub(crate) struct AgentMetadata {
    pub ip: String,
    pub remote_host: String,
    pub country_code: String,
    pub city: String,
    pub region: String,
    pub asn: String,
    pub asn_org: String,
    // システム情報フィールド
    pub os_type: String,
    pub os_version: String,
    pub hostname: String,
    pub kernel_version: String,
    pub username: String,
}

// エージェントIDとAgentConnectionのマップ（スレッドセーフ）
pub(crate) type AgentMap = DashMap<String, Arc<AgentConnection>>;

// APIレスポンス用のエージェント情報
#[derive(Serialize, ToSchema)]
pub(crate) struct AgentInfo {
    pub agent_id: String,
    pub ip: String,
    pub remote_host: String,
    pub country_code: String,
    pub city: String,
    pub region: String,
    pub asn: String,
    pub asn_org: String,
    // システム情報フィールド
    pub os_type: String,
    pub os_version: String,
    pub hostname: String,
    pub kernel_version: String,
    pub username: String,
}

#[utoipa::path(
    get,
    path = "/api/agents",
    params(AgentQuery),
    responses(
        (status = 200, description = "List of agents", body = [AgentInfo])
    ),
    tag = "Agent"
)]
#[allow(dead_code)] // Suppress dead_code warning as it might be used externally or later
pub(crate) async fn list_agents(
    State(state): State<AppState>,
    // クエリパラメータ (例: /api/agents?country=JP)
    Query(query): Query<AgentQuery>,
) -> Json<Vec<AgentInfo>> {
    let mut result = Vec::new();
    // AgentMapをイテレート
    for entry in state.agents.iter() {
        let meta = &entry.value().metadata;
        if let Some(ref country) = query.country {
            if &meta.country_code != country {
                continue;
            }
        }
        // Use From implementation to construct AgentInfo
        result.push((entry.key(), meta).into());
    }
    // 結果をJSON形式で返す
    Json(result)
}

// Allow conversion from DashMap entry (key and metadata) to AgentInfo
impl From<(&String, &AgentMetadata)> for AgentInfo {
    fn from((agent_id, meta): (&String, &AgentMetadata)) -> Self {
        AgentInfo {
            agent_id: agent_id.clone(),
            ip: meta.ip.clone(),
            remote_host: meta.remote_host.clone(),
            country_code: meta.country_code.clone(),
            city: meta.city.clone(),
            region: meta.region.clone(),
            asn: meta.asn.clone(),
            asn_org: meta.asn_org.clone(),
            os_type: meta.os_type.clone(),
            os_version: meta.os_version.clone(),
            hostname: meta.hostname.clone(),
            kernel_version: meta.kernel_version.clone(),
            username: meta.username.clone(),
        }
    }
}
