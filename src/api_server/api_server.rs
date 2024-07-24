use hyper::{Body, Request, Response, Server, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use std::convert::Infallible;
use std::sync::Arc;
use log;
use hyper::Method;
use tokio::sync::Mutex;
use serde_json::Value;
use crate::notification_manager::NotificationManager;
use super::nip98_auth::nip98_verify_auth_header;


pub struct APIServer {
    host: String,
    port: String,
    notification_manager: Arc<Mutex<NotificationManager>>,
    base_url: String,
}

impl APIServer {
    pub async fn new(host: String, port: String, notification_manager: Arc<Mutex<NotificationManager>>, base_url: String) {
        let make_svc = make_service_fn(|_| async {
            Ok::<_, Infallible>(service_fn(Self::handle_http_request))
        });
    
        let address = format!("{}:{}", host, port);
        let server = Server::bind(&address).serve(make_svc);
    
        log::info!("HTTP server running at {}", address);
    
        // Run this server for HTTP requests
        if let Err(e) = server.await {
            log::error!("Server error: {}", e);
        }
    }
    
    async fn handle_http_request(&self, req: Request<Body>) -> Result<Response<Body>, Infallible> {
        let mut response = Response::new(Body::empty());
    
        // Logging middleware
        log::info!("[{}] {}", req.method(), req.uri());
    
        // NIP-98 authentication
        if let Err(auth_error) = self.authenticate(&req, &state).await {
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            *response.body_mut() = Body::from(auth_error);
            return Ok(response);
        }
    
        // Route handling
        match (req.method(), req.uri().path()) {
            (&Method::POST, "/user-info") => handle_user_info(req, &state).await,
            (&Method::POST, "/user-info/remove") => handle_user_info_remove(req, &state).await,
            _ => {
                *response.status_mut() = StatusCode::NOT_FOUND;
                Ok(response)
            }
        }
    }
    
    async fn authenticate(&self, req: &Request<Body>, state: &AppState) -> Result<String, String> {
        let auth_header = req.headers().get("Authorization")
            .ok_or_else(|| "No authorization header provided".to_string())?;
    
        let body_bytes = hyper::body::to_bytes(req.body()).await.map_err(|e| e.to_string())?;
    
        nip98
        let (authorized_pubkey, error) = nip98_auth::nip98_verify_auth_header(
            auth_header.to_str().map_err(|e| e.to_string())?,
            &format!("{}{}", state.base_url, req.uri().path()),
            req.method().as_str(),
            &body_bytes
        ).await;
    
        if let Some(error) = error {
            return Err(error);
        }
    
        authorized_pubkey.ok_or_else(|| "No authorized pubkey found".to_string())
    }
    
    async fn handle_user_info(&self, req: Request<Body>, state: &AppState) -> Result<Response<Body>, Infallible> {
        let body_bytes = hyper::body::to_bytes(req.body()).await.unwrap();
        let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    
        let device_token = body["deviceToken"].as_str().unwrap();
        let pubkey = body["pubkey"].as_str().unwrap();
    
        // Assumption: authorized_pubkey is stored in request extensions
        let authorized_pubkey = req.extensions().get::<String>().unwrap();
    
        if pubkey != authorized_pubkey {
            let mut response = Response::new(Body::from("Pubkey does not match authorized pubkey"));
            *response.status_mut() = StatusCode::FORBIDDEN;
            return Ok(response);
        }
    
        let mut notification_manager = state.notification_manager.lock().await;
        notification_manager.save_user_device_info(pubkey, device_token);
    
        Ok(Response::new(Body::from("User info saved successfully")))
    }
    
    async fn handle_user_info_remove(&self, req: Request<Body>, state: &AppState) -> Result<Response<Body>, Infallible> {
        let body_bytes = hyper::body::to_bytes(req.body()).await.unwrap();
        let body: Value = serde_json::from_slice(&body_bytes).unwrap();
    
        let device_token = body["deviceToken"].as_str().unwrap();
        let pubkey = body["pubkey"].as_str().unwrap();
    
        // Assumption: authorized_pubkey is stored in request extensions
        let authorized_pubkey = req.extensions().get::<String>().unwrap();
    
        if pubkey != authorized_pubkey {
            let mut response = Response::new(Body::from("Pubkey does not match authorized pubkey"));
            *response.status_mut() = StatusCode::FORBIDDEN;
            return Ok(response);
        }
    
        let mut notification_manager = state.notification_manager.lock().await;
        notification_manager.remove_user_device_info(pubkey, device_token);
    
        Ok(Response::new(Body::from("User info removed successfully")))
    }
    
}
