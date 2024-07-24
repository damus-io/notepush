use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use tokio::net::TcpListener;
use log;
use tokio::sync::Mutex;
use crate::notification_manager::NotificationManager;
use super::api_request_handler::APIHandler;

pub struct APIServer {
    host: String,
    port: String,
    api_handler: APIHandler,
}

impl APIServer {
    pub async fn run(host: String, port: String, notification_manager: Arc<Mutex<NotificationManager>>, base_url: String) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let api_handler = APIHandler::new(notification_manager, base_url);
        let server = APIServer {
            host,
            port,
            api_handler,
        };
        server.start().await
    }
    
    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let address = format!("{}:{}", self.host, self.port);
        let listener = TcpListener::bind(&address).await?;
        
        log::info!("HTTP server running at {}", address);
        
        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let api_handler = self.api_handler.clone();
    
            tokio::task::spawn(async move {
                let service = service_fn(|req| api_handler.handle_http_request(req));
    
                if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                    log::error!("Failed to serve connection: {:?}", err);
                }
            });
        }
    }
}
