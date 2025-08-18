#![forbid(unsafe_code)]

use hyper_util::rt::TokioIo;
use log::warn;
use nostr_sdk::{
    Client, Filter, RelayMessage, RelayPoolNotification, RelayStatus, SubscribeOptions, Timestamp,
};
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
mod notification_manager;
use r2d2_sqlite::SqliteConnectionManager;
mod config;
mod relay_connection;
use crate::config::{DEFAULT_DB_PATH, DEFAULT_NOSTR_EVENT_CACHE_MAX_AGE, DEFAULT_RELAY_URL};
use crate::notification_manager::NotificationManager;
use config::NotePushConfig;

mod api_request_handler;
mod nip98_auth;
mod utils;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // MARK: - Setup basics

    env_logger::init();

    let cfg = NotePushConfig::load_env().expect("Failed to load environment variables");
    let listener = TcpListener::bind(&cfg.relay_address())
        .await
        .expect("Failed to bind to address");
    log::info!("Server running at {}", cfg.relay_address());

    let manager = SqliteConnectionManager::file(
        cfg.db_path
            .as_ref()
            .unwrap_or(&DEFAULT_DB_PATH.to_string())
            .to_owned(),
    );

    let pool: r2d2::Pool<SqliteConnectionManager> =
        r2d2::Pool::new(manager).expect("Failed to create SQLite connection pool");

    // Notification manager is a shared resource that will be used by all connections via a mutex and an atomic reference counter.
    // This is shared to avoid data races when reading/writing to the sqlite database, and reduce outgoing relay connections.
    let mut notification_manager = NotificationManager::new(
        pool,
        cfg.relay_url
            .as_ref()
            .unwrap_or(&DEFAULT_RELAY_URL.to_string())
            .clone(),
        Duration::from_secs(
            cfg.nostr_event_cache_max_age
                .unwrap_or(DEFAULT_NOSTR_EVENT_CACHE_MAX_AGE),
        ),
    )
    .await
    .expect("Failed to create notification manager");

    // setup apns if key path is set
    if let Some(cfg) = &cfg.apns {
        notification_manager = notification_manager
            .with_apns(
                cfg.key_path.clone(),
                cfg.key_id.clone(),
                cfg.team_id.clone(),
                cfg.endpoint(),
                cfg.topic.clone(),
            )
            .expect("Failed to set APNs");
        log::info!("APNS configured for notifications manager!");
    }

    // setup fcm if google services path is set
    if let Some(fcm) = &cfg.fcm {
        notification_manager = notification_manager
            .with_fcm(fcm.google_services_file_path.clone())
            .expect("Failed to setup FCM");
        log::info!("FCM configured for notifications manager!");
    }

    let notification_manager = Arc::new(notification_manager);
    let api_handler = Arc::new(api_request_handler::APIHandler::new(
        notification_manager.clone(),
        cfg.api_base_url.clone(),
    ));

    // start relay puller if configured
    if !cfg.pull_relays.is_empty() {
        let mgr = notification_manager.clone();
        let relays = cfg.pull_relays.clone();
        tokio::spawn(async move {
            if let Err(e) = pull_events(mgr, &relays).await {
                log::error!("Failed to pull events: {}", e);
            }
        });
    }

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let api_handler_clone = api_handler.clone();
        let mut http = hyper::server::conn::http1::Builder::new();
        http.keep_alive(true);

        let cfg = cfg.clone();
        tokio::task::spawn(async move {
            let service =
                hyper::service::service_fn(|req| api_handler_clone.handle_http_request(req, &cfg));

            let connection = http.serve_connection(io, service).with_upgrades();

            if let Err(err) = connection.await {
                log::error!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

// pull events from relays to send as notifications
async fn pull_events(
    mgr: Arc<NotificationManager>,
    relays: &Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build();
    for relay in relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;

    let filter = Filter::default()
        .kinds(NotificationManager::supported_kinds())
        .since(Timestamp::now())
        .limit(1);

    // request supported kinds from all relays
    let _sub = client
        .pool()
        .subscribe(filter, SubscribeOptions::default())
        .await?;

    let mut notif = client.notifications();
    while let Ok(msg) = notif.recv().await {
        match msg {
            RelayPoolNotification::Event { event, .. } => {
                if let Err(e) = mgr.handle_event(event.deref()).await {
                    log::error!("Failed to handle event {:?}", e);
                }
            }
            RelayPoolNotification::Message { .. } => {}
            RelayPoolNotification::Shutdown => {}
        }
    }

    warn!("event pull exited unexpectedly");
    Ok(())
}
