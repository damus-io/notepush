#![forbid(unsafe_code)]

use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::net::TcpListener;

mod api_request_handler;
mod event_filter;
mod nip98_auth;
mod notification_manager;
mod notepush_env;
mod relay_connection;
mod utils;

use api_request_handler::APIHandler;
use event_filter::EventFilter;
use notepush_env::NotePushEnv;
use r2d2_sqlite::SqliteConnectionManager;

/// Default path for noteguard configuration file.
const NOTEGUARD_CONFIG_PATH: &str = "noteguard.toml";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    env_logger::init();

    let env = NotePushEnv::load_env().expect("Failed to load environment variables");
    log::info!("Starting notepush relay at {}", env.relay_address());

    // Initialize event filter from noteguard config.
    // If config doesn't exist or fails to load, use permissive filter (accept all).
    let event_filter = load_event_filter();
    let event_filter = Arc::new(event_filter);

    // Set up database connection pool
    let manager = SqliteConnectionManager::file(env.db_path.clone());
    let pool: r2d2::Pool<SqliteConnectionManager> =
        r2d2::Pool::new(manager).expect("Failed to create SQLite connection pool");

    // Notification manager is shared across all connections via Arc.
    // This avoids data races on SQLite and reduces outgoing relay connections.
    let notification_manager = Arc::new(
        notification_manager::NotificationManager::new(
            pool,
            env.relay_url.clone(),
            env.apns_private_key_path.clone(),
            env.apns_private_key_id.clone(),
            env.apns_team_id.clone(),
            env.apns_environment.clone(),
            env.apns_topic.clone(),
            env.nostr_event_cache_max_age,
        )
        .await
        .expect("Failed to create notification manager"),
    );

    let api_handler = Arc::new(APIHandler::new(
        notification_manager.clone(),
        event_filter.clone(),
        env.api_base_url.clone(),
    ));

    let listener = TcpListener::bind(&env.relay_address())
        .await
        .expect("Failed to bind to address");
    log::info!("Server running at {}", env.relay_address());

    // Main accept loop
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let api_handler_clone = api_handler.clone();

        let mut http = hyper::server::conn::http1::Builder::new();
        http.keep_alive(true);

        tokio::task::spawn(async move {
            let service =
                hyper::service::service_fn(|req| api_handler_clone.handle_http_request(req));
            let connection = http.serve_connection(io, service).with_upgrades();

            if let Err(err) = connection.await {
                log::error!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

/// Loads the event filter from configuration file.
///
/// Tries to load from NOTEGUARD_CONFIG_PATH. If the file doesn't exist
/// or fails to parse, returns a permissive filter that accepts all events.
fn load_event_filter() -> EventFilter {
    use std::io::Read;

    let config_path = std::env::var("NOTEGUARD_CONFIG_PATH")
        .unwrap_or_else(|_| NOTEGUARD_CONFIG_PATH.to_string());

    // Try to open config file
    let mut file = match std::fs::File::open(&config_path) {
        Ok(f) => f,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::info!(
                    "No noteguard config at '{}', running without event filtering",
                    config_path
                );
            } else {
                log::warn!(
                    "Could not open noteguard config '{}': {}, running without event filtering",
                    config_path,
                    e
                );
            }
            return EventFilter::permissive();
        }
    };

    // Read and parse config
    let mut contents = String::new();
    if let Err(e) = file.read_to_string(&mut contents) {
        log::warn!(
            "Could not read noteguard config '{}': {}, running without event filtering",
            config_path,
            e
        );
        return EventFilter::permissive();
    }

    let config: noteguard_core::Config = match toml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            log::error!(
                "Failed to parse noteguard config '{}': {}, running without event filtering",
                config_path,
                e
            );
            return EventFilter::permissive();
        }
    };

    // Create filter from config
    match EventFilter::new(&config) {
        Some(filter) => {
            log::info!(
                "Loaded noteguard config from '{}' with {} filters in pipeline",
                config_path,
                config.pipeline.len()
            );
            filter
        }
        None => {
            log::error!("Failed to initialize event filter, running without event filtering");
            EventFilter::permissive()
        }
    }
}
