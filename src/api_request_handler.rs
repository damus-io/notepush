use crate::nip98_auth;
use crate::relay_connection::RelayConnection;
use http_body_util::Full;
use hyper::body::Buf;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use hyper_tungstenite;

use http_body_util::BodyExt;
use nostr;

use crate::notification_manager::NotificationManager;
use hyper::Method;
use log;
use serde_json::{json, Value};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

struct ParsedRequest {
    uri: String,
    method: Method,
    body_bytes: Option<Vec<u8>>,
    authorized_pubkey: nostr::PublicKey,
}

impl ParsedRequest {
    fn body_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        if let Some(body_bytes) = &self.body_bytes {
            Ok(serde_json::from_slice(body_bytes)?)
        } else {
            Ok(json!({}))
        }
    }
}

struct APIResponse {
    status: StatusCode,
    body: Value,
}

pub struct APIHandler {
    notification_manager: Arc<Mutex<NotificationManager>>,
    base_url: String,
}

impl Clone for APIHandler {
    fn clone(&self) -> Self {
        APIHandler {
            notification_manager: self.notification_manager.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

impl APIHandler {
    pub fn new(notification_manager: Arc<Mutex<NotificationManager>>, base_url: String) -> Self {
        APIHandler {
            notification_manager,
            base_url,
        }
    }

    pub async fn handle_http_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::http::Error> {
        // Check if the request is a websocket upgrade request.
        if hyper_tungstenite::is_upgrade_request(&req) {
            return match self.handle_websocket_upgrade(req).await {
                Ok(response) => Ok(response),
                Err(err) => {
                    log::error!("Error handling websocket upgrade request: {}", err);
                    Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(http_body_util::Full::new(Bytes::from(
                            "Internal server error",
                        )))?)
                }
            };
        }

        // If not, handle the request as a normal API request.
        let final_api_response: APIResponse = match self.try_to_handle_http_request(req).await {
            Ok(api_response) => APIResponse {
                status: api_response.status,
                body: api_response.body,
            },
            Err(err) => {
                // Detect if error is a APIError::AuthenticationError and return a 401 status code
                if let Some(api_error) = err.downcast_ref::<APIError>() {
                    match api_error {
                        APIError::AuthenticationError(message) => APIResponse {
                            status: StatusCode::UNAUTHORIZED,
                            body: json!({ "error": "Unauthorized", "message": message }),
                        },
                    }
                } else {
                    // Otherwise, return a 500 status code
                    let random_case_uuid = uuid::Uuid::new_v4();
                    log::error!(
                        "Error handling request: {} (Case ID: {})",
                        err,
                        random_case_uuid
                    );
                    APIResponse {
                        status: StatusCode::INTERNAL_SERVER_ERROR,
                        body: json!({ "error": "Internal server error", "message": format!("Case ID: {}", random_case_uuid) }),
                    }
                }
            }
        };

        Ok(Response::builder()
            .header("Content-Type", "application/json")
            .header("Access-Control-Allow-Origin", "*")
            .status(final_api_response.status)
            .body(http_body_util::Full::new(Bytes::from(
                final_api_response.body.to_string(),
            )))?)
    }

    async fn handle_websocket_upgrade(
        &self,
        mut req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, Box<dyn std::error::Error>> {
        let (response, websocket) = hyper_tungstenite::upgrade(&mut req, None)?;
        log::info!("New websocket connection.");

        let new_notification_manager = self.notification_manager.clone();
        tokio::spawn(async move {
            match RelayConnection::run(websocket, new_notification_manager).await {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Error with websocket connection: {:?}", e);
                }
            }
        });

        Ok(response)
    }

    async fn try_to_handle_http_request(
        &self,
        mut req: Request<Incoming>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        let parsed_request = self.parse_http_request(&mut req).await?;
        let api_response: APIResponse = self.handle_parsed_http_request(&parsed_request).await?;
        log::info!(
            "[{}] {} (Authorized pubkey: {}): {}",
            req.method(),
            req.uri(),
            parsed_request.authorized_pubkey,
            api_response.status
        );
        Ok(api_response)
    }

    async fn parse_http_request(
        &self,
        req: &mut Request<Incoming>,
    ) -> Result<ParsedRequest, Box<dyn std::error::Error>> {
        // 1. Read the request body
        let body_buffer = req.body_mut().collect().await?.aggregate();
        let body_bytes = body_buffer.chunk();
        let body_bytes = if body_bytes.is_empty() {
            None
        } else {
            Some(body_bytes)
        };

        // 2. NIP-98 authentication
        let authorized_pubkey = match self.authenticate(&req, body_bytes).await? {
            Ok(pubkey) => pubkey,
            Err(auth_error) => {
                return Err(Box::new(APIError::AuthenticationError(auth_error)));
            }
        };

        // 3. Parse the request
        Ok(ParsedRequest {
            uri: req.uri().path().to_string(),
            method: req.method().clone(),
            body_bytes: body_bytes.map(|b| b.to_vec()),
            authorized_pubkey,
        })
    }

    async fn handle_parsed_http_request(
        &self,
        parsed_request: &ParsedRequest,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        match (&parsed_request.method, parsed_request.uri.as_str()) {
            (&Method::POST, "/user-info") => self.handle_user_info(parsed_request).await,
            (&Method::POST, "/user-info/remove") => {
                self.handle_user_info_remove(parsed_request).await
            }
            _ => Ok(APIResponse {
                status: StatusCode::NOT_FOUND,
                body: json!({ "error": "Not found" }),
            }),
        }
    }

    async fn authenticate(
        &self,
        req: &Request<Incoming>,
        body_bytes: Option<&[u8]>,
    ) -> Result<Result<nostr::PublicKey, String>, Box<dyn std::error::Error>> {
        let auth_header = match req.headers().get("Authorization") {
            Some(header) => header,
            None => return Ok(Err("Authorization header not found".to_string())),
        };

        Ok(nip98_auth::nip98_verify_auth_header(
            auth_header.to_str()?.to_string(),
            &format!("{}{}", self.base_url, req.uri().path()),
            req.method().as_str(),
            body_bytes,
        )
        .await)
    }

    async fn handle_user_info(
        &self,
        req: &ParsedRequest,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        let body = req.body_json()?;

        if let Some(device_token) = body["deviceToken"].as_str() {
            let notification_manager = self.notification_manager.lock().await;
            notification_manager.save_user_device_info(req.authorized_pubkey, device_token)?;
            return Ok(APIResponse {
                status: StatusCode::OK,
                body: json!({ "message": "User info saved successfully" }),
            });
        } else {
            return Ok(APIResponse {
                status: StatusCode::BAD_REQUEST,
                body: json!({ "error": "deviceToken is required" }),
            });
        }
    }

    async fn handle_user_info_remove(
        &self,
        req: &ParsedRequest,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        let body: Value = req.body_json()?;

        if let Some(device_token) = body["deviceToken"].as_str() {
            let notification_manager = self.notification_manager.lock().await;
            notification_manager.remove_user_device_info(req.authorized_pubkey, device_token)?;
            return Ok(APIResponse {
                status: StatusCode::OK,
                body: json!({ "message": "User info removed successfully" }),
            });
        } else {
            return Ok(APIResponse {
                status: StatusCode::BAD_REQUEST,
                body: json!({ "error": "deviceToken is required" }),
            });
        }
    }
}

// Define enum error types including authentication error
#[derive(Debug, Error)]
enum APIError {
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
}
