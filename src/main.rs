use std::io::Read;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
mod notification_manager;
use log;
use env_logger;

mod relay_connection;
use relay_connection::RelayConnection;


fn main () {
    env_logger::init();
    
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port = std::env::var("PORT").unwrap_or_else(|_| "9001".to_string());
    let address = format!("{}:{}", host, port);
    let server = TcpListener::bind(&address).unwrap();
    
    let notification_manager = Arc::new(Mutex::new(notification_manager::NotificationManager::new(None, None).unwrap()));
    
    log::info!("Server listening on {}", address);
    
    for stream in server.incoming() {
        if let Ok(stream) = stream {
            log::info!("New connection: {:?}", stream.peer_addr().map_or("unknown".to_string(), |addr| addr.to_string()));
        } else if let Err(e) = stream {
            log::error!("Error: {:?}", e);
        }
        thread::spawn (move || {
            let notification_manager = notification_manager.clone();
            let websocket_connection = RelayConnection::new(stream, notification_manager);
            match websocket_connection.start() {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Error in websocket connection: {:?}", e);
                }
            }
        });
    }
}
