use crate::repository::UserRole;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}

#[derive(Serialize, ToSchema)]
pub struct RegisterResponse {
    pub user_id: String,
}

#[derive(Serialize, ToSchema)]
pub struct LoginResponse {
    pub success: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct CommandRequestParams {
    pub agent_id: String,
    pub command: String,
}

#[derive(Deserialize, ToSchema, IntoParams)]
pub struct AgentQuery {
    pub country: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct CurrentUserResponse {
    pub id: Uuid,
    pub username: String,
    pub role: UserRole,
    pub points: i32,
}
