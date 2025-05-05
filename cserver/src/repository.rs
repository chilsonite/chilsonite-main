#![allow(dead_code)]
// repository module: SQLx-based data access for users, tokens, and user_points
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sqlx::Type;
use sqlx::{query, query_as, FromRow, PgPool}; // for deriving SQLx types
use uuid::Uuid;

// Define Rust enum matching Postgres user_role type
#[derive(Type, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "user_role", rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    User,
}

#[derive(Debug, FromRow)]
pub struct UserRecord {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
pub struct TokenRecord {
    pub token: String,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
pub struct UserPointsRecord {
    pub user_id: Uuid,
    pub points: i32,
    pub updated_at: DateTime<Utc>,
}

// --- Users ---
pub async fn create_user(
    pool: &PgPool,
    id: Uuid,
    username: &str,
    password_hash: &str,
    role: UserRole,
    created_at: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let result = query(
        "INSERT INTO users (id, username, password_hash, role, created_at) VALUES ($1, $2, $3, $4, $5)"
    )
    .bind(id)
    .bind(username)
    .bind(password_hash)
    .bind(role)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn get_user_by_username(
    pool: &PgPool,
    username: &str,
) -> sqlx::Result<Option<UserRecord>> {
    let rec = query_as::<_, UserRecord>(
        "SELECT id, username, password_hash, role, created_at FROM users WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(rec)
}

pub async fn get_user_by_id(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<UserRecord>> {
    let rec = query_as::<_, UserRecord>(
        "SELECT id, username, password_hash, role, created_at FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(rec)
}

// --- Tokens ---
pub async fn create_token(
    pool: &PgPool,
    token: &str,
    user_id: Uuid,
    expires_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let result = query!(
        r#"INSERT INTO tokens (token, user_id, expires_at, created_at)
           VALUES ($1, $2, $3, $4)"#,
        token,
        user_id,
        expires_at,
        created_at
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn get_token(pool: &PgPool, token: &str) -> sqlx::Result<Option<TokenRecord>> {
    let rec = query_as!(
        TokenRecord,
        r#"SELECT token, user_id, expires_at, created_at
           FROM tokens WHERE token = $1"#,
        token
    )
    .fetch_optional(pool)
    .await?;
    Ok(rec)
}

pub async fn delete_token(pool: &PgPool, token: &str) -> sqlx::Result<u64> {
    let result = query!("DELETE FROM tokens WHERE token = $1", token)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

// 新規: 指定ユーザの、有効期限内のトークンを全件取得する関数
pub async fn get_user_tokens(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<TokenRecord>> {
    let recs = query_as!(
        TokenRecord,
        r#"SELECT token, user_id, expires_at, created_at
           FROM tokens
           WHERE user_id = $1 AND expires_at > $2"#,
        user_id,
        Utc::now()
    )
    .fetch_all(pool)
    .await?;
    Ok(recs)
}

// --- User Points ---
pub async fn get_user_points(
    pool: &PgPool,
    user_id: Uuid,
) -> sqlx::Result<Option<UserPointsRecord>> {
    let rec = query_as!(
        UserPointsRecord,
        r#"SELECT user_id, points, updated_at
           FROM user_points WHERE user_id = $1"#,
        user_id
    )
    .fetch_optional(pool)
    .await?;
    Ok(rec)
}

pub async fn create_user_points(
    pool: &PgPool,
    user_id: Uuid,
    points: i32,
    updated_at: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let result = query!(
        r#"INSERT INTO user_points (user_id, points, updated_at)
           VALUES ($1, $2, $3)"#,
        user_id,
        points,
        updated_at
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

pub async fn update_user_points(
    pool: &PgPool,
    user_id: Uuid,
    points: i32,
    updated_at: DateTime<Utc>,
) -> sqlx::Result<u64> {
    let result = query!(
        r#"UPDATE user_points SET points = $2, updated_at = $3
           WHERE user_id = $1"#,
        user_id,
        points,
        updated_at
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
