mod agent;
mod api;
mod config;
mod repository;
mod socks5;
mod token;
mod websocket;

use anyhow::{anyhow, Result};
use dashmap::DashMap;
use dotenvy::dotenv;
use env_logger;
use futures::stream::{SplitSink, SplitStream};
use log::{error, info};
// dummy import to align loc
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::WebSocketStream as TungsteniteWebSocketStream;
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

// モジュールから型をインポート
use agent::AgentMap;
use config::Settings;

// WebSocket送信ストリームの型エイリアス
type WsSink = SplitSink<
    TungsteniteWebSocketStream<TcpStream>,
    tokio_tungstenite::tungstenite::protocol::Message,
>;
// WebSocket受信ストリームの型エイリアス
type WsStream = SplitStream<TungsteniteWebSocketStream<TcpStream>>;

// 非同期処理間の通信チャネル管理用列挙型
// SOCKS5のConnect応答(oneshot)とデータ転送(mpsc)で使用
enum PendingSender {
    Oneshot(oneshot::Sender<common::Payload>),
    Mpsc(mpsc::Sender<common::Payload>),
}

// リクエストIDと対応する応答待機チャネルのマッピング
// SOCKS5のConnect応答とデータ転送で使用
type PendingMap = Arc<Mutex<HashMap<String, PendingSender>>>;

// コマンド実行結果のストリーミング用チャネルのマッピング
// APIのコマンド実行で使用
type CommandResponseMap = Arc<Mutex<HashMap<String, mpsc::Sender<common::Payload>>>>;

// アプリケーション全体で共有される状態
#[allow(dead_code)]
#[derive(Clone)]
struct AppState {
    db_pool: PgPool,
    jwt_secret: String,
    agents: Arc<AgentMap>,
    command_responses: CommandResponseMap,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        api::register,
        api::login,
        token::generate_token,
        api::get_current_user,
        api::list_tokens,
        api::execute_command,
        agent::list_agents
    ),
    components(
        schemas(
            api::dto::RegisterRequest,
            api::dto::LoginRequest,
            api::dto::CommandRequestParams,
            api::dto::CurrentUserResponse,
            token::TokenResponse,
            agent::AgentInfo
        )
    ),
    tags(
        (name = "Auth", description = "Authentication operations"),
        (name = "Agent", description = "Agent management")
    )
)]
struct ApiDoc;

// アプリケーションのエントリーポイント
#[tokio::main]
async fn main() -> Result<()> {
    // .env を読み込む
    dotenv().expect("`.env`の読み込みに失敗しました");
    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    // DBプール初期化
    let db_pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .expect("DBプールの作成に失敗しました");
    // JWTシークレット読み込み
    let jwt_secret = env::var("JWT_SECRET").expect("JWT_SECRET must be set");

    // ロガーの初期化
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 共有状態の初期化
    let agents: Arc<AgentMap> = Arc::new(DashMap::new());
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let command_responses: CommandResponseMap = Arc::new(Mutex::new(HashMap::new()));

    // 設定ファイルの読み込み
    let settings = Arc::new(config::load_config().await?);
    // AppStateの作成
    let app_state = AppState {
        db_pool: db_pool.clone(),
        jwt_secret: jwt_secret.clone(),
        agents: agents.clone(),
        command_responses: command_responses.clone(),
    };

    // APIルーターの構築
    let api_router = api::build_api_router(app_state.clone()).layer(
        CorsLayer::new()
            .allow_origin(Any) // 任意のオリジンを許可
            .allow_methods(Any) // 任意のHTTPメソッドを許可
            .allow_headers(Any), // 任意のヘッダーを許可
    );

    // Swagger UI ドキュメント用ルーターの追加
    let docs_router =
        SwaggerUi::new("/swagger-ui/").url("/api-doc/openapi.json", ApiDoc::openapi());

    // API と Swagger UI をマージ
    let app = api_router.merge(docs_router);

    // APIサーバー用リスナーの準備
    let api_addr_str = format!("{}:{}", settings.bind_address, 8080); // APIサーバーは8080番ポートを使用
    let api_addr: std::net::SocketAddr = api_addr_str
        .parse()
        .map_err(|e| anyhow!("Invalid API server address '{}': {}", api_addr_str, e))?;

    let listener = TcpListener::bind(api_addr)
        .await
        .map_err(|e| anyhow!("Failed to bind API server on {}: {}", api_addr, e))?;
    info!("Axum API server listening on {}", api_addr);

    // WebSocketサーバー、SOCKS5サーバー、APIサーバーを並行して実行
    tokio::select! {
        // WebSocketサーバーの実行
        res = websocket::run_websocket_server(agents.clone(), pending.clone(), command_responses.clone(), settings.clone()) => {
            if let Err(e) = res {
                error!("WebSocket server failed: {:?}", e);
            } else {
                 info!("WebSocket server finished.");
            }
        },
        // SOCKS5サーバー（トークン認証付き）の実行
        res = socks5::run_socks5_server_with_token(
            db_pool.clone(),
            agents.clone(),
            pending.clone(),
            settings.clone(),
        ) => {
            if let Err(e) = res {
                error!("SOCKS5 server failed: {:?}", e);
            } else {
                 info!("SOCKS5 server finished.");
            }
        },
        // Axum APIサーバーの実行
        res = axum::serve(listener, app.into_make_service()) => {
            if let Err(e) = res {
                error!("Axum API server failed: {:?}", e);
            } else {
                info!("Axum API server finished.");
            }
        }
    }

    Ok(())
}
