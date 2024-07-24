use std::net::TcpListener;
use std::sync::Arc;
use tokio::sync::Mutex;
mod notification_manager;
use log;
use env_logger;
use r2d2_sqlite::SqliteConnectionManager;
mod relay_connection;
use relay_connection::RelayConnection;
use r2d2;
mod notepush_env;
use notepush_env::NotePushEnv;
mod api_server;
use api_server::APIServer;

#[tokio::main]
async fn main () {
    
    // MARK: - Setup basics
    
    env_logger::init();
    
    let env = NotePushEnv::load_env().expect("Failed to load environment variables");
    let server = TcpListener::bind(&env.relay_address()).expect("Failed to bind to address");
    
    let manager = SqliteConnectionManager::file(env.db_path.clone());
    let pool: r2d2::Pool<SqliteConnectionManager> = r2d2::Pool::new(manager).expect("Failed to create SQLite connection pool");
    // Notification manager is a shared resource that will be used by all connections via a mutex and an atomic reference counter.
    // This is shared to avoid data races when reading/writing to the sqlite database, and reduce outgoing relay connections.
    let notification_manager = Arc::new(Mutex::new(notification_manager::NotificationManager::new(
        pool,
        env.relay_url.clone(),
        env.apns_private_key_path.clone(), 
        env.apns_private_key_id.clone(),
        env.apns_team_id.clone(),
        env.apns_environment.clone(),
        env.apns_topic.clone(),
    ).await.expect("Failed to create notification manager")));
    
    // MARK: - Start the API server
    
    let api_server = APIServer::new(env.api_host.clone(), env.api_port.clone(), notification_manager.clone(), env.api_base_url.clone());
    
    // MARK: - Start handling incoming connections
    
    log::info!("Relay server listening on {}", env.relay_address().clone());
    
    for stream in server.incoming() {
        if let Ok(stream) = stream {
            let peer_address_string = stream.peer_addr().map_or("unknown".to_string(), |addr| addr.to_string());
            log::info!("New connection from {}", peer_address_string);
            let notification_manager = notification_manager.clone();
            tokio::spawn(async move {
                match RelayConnection::run(stream, notification_manager).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("Error with websocket connection from {}: {:?}", peer_address_string, e);
                    }
                }
            });
        } else if let Err(e) = stream {
            log::error!("Error in incoming connection stream: {:?}", e);
        }
    }
}
