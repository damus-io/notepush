use crate::nip98_auth;
use crate::relay_connection::RelayConnection;
use notepush::notification_manager::{NotificationManagerError, UserNotificationSettings};
use notepush::server_keys::ServerKeys;
use notepush::Platform;
use http_body_util::Full;
use hyper::body::Buf;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

use http_body_util::BodyExt;
use serde_json::from_value;

use notepush::notification_manager::NotificationManager;
use hyper::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

pub struct APIHandler {
    notification_manager: Arc<NotificationManager>,
    base_url: String,
    /// Server keys for NIP-44 encryption (None if encryption disabled)
    server_keys: Option<ServerKeys>,
}

impl APIHandler {
    pub fn new(
        notification_manager: Arc<NotificationManager>,
        base_url: String,
        server_keys: Option<ServerKeys>,
    ) -> Self {
        APIHandler {
            notification_manager,
            base_url,
            server_keys,
        }
    }

    // MARK: - HTTP handling

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

        Response::builder()
            .header("Content-Type", "application/json")
            .header("Access-Control-Allow-Origin", "*")
            .status(final_api_response.status)
            .body(http_body_util::Full::new(Bytes::from(
                final_api_response.body.to_string(),
            )))
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
        // Check for public (unauthenticated) routes first
        // These endpoints are accessible without NIP-98 auth
        if let Some(response) = self.handle_public_routes(&req).await {
            // Safe to log path for public routes (no device tokens)
            log::info!("[{}] {} (public): {}", req.method(), req.uri().path(), response.status);
            return Ok(response);
        }

        // All other routes require authentication
        let parsed_request = self.parse_http_request(&mut req).await?;
        let api_response: APIResponse = self.handle_parsed_http_request(&parsed_request).await?;
        // Log abbreviated pubkey; full URIs contain device tokens which are sensitive
        let pubkey_abbrev = &parsed_request.authorized_pubkey.to_hex()[..8];
        log::info!(
            "[{}] /user-info/... (pubkey: {}...): {}",
            req.method(),
            pubkey_abbrev,
            api_response.status
        );
        Ok(api_response)
    }

    /// Handle public endpoints that don't require authentication
    ///
    /// Returns Some(response) if the route matches a public endpoint,
    /// None if the route should require authentication.
    async fn handle_public_routes(&self, req: &Request<Incoming>) -> Option<APIResponse> {
        // GET /server-pubkey - Returns the server's public key for NIP-44 encryption
        // Clients need this to know which key to expect encrypted payloads from
        if req.method() == Method::GET && req.uri().path() == "/server-pubkey" {
            return Some(self.handle_server_pubkey());
        }

        None
    }

    /// Returns the server's public key for NIP-44 encryption
    ///
    /// Clients call this endpoint to discover the server's pubkey before
    /// registering their device pubkey. When they receive an encrypted
    /// notification, they'll use this pubkey as the "sender" when decrypting.
    fn handle_server_pubkey(&self) -> APIResponse {
        match &self.server_keys {
            Some(keys) => APIResponse {
                status: StatusCode::OK,
                body: json!({
                    "pubkey": keys.public_key_hex(),
                    "encryption_enabled": true
                }),
            },
            None => APIResponse {
                status: StatusCode::OK,
                body: json!({
                    "pubkey": null,
                    "encryption_enabled": false
                }),
            },
        }
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
        let authorized_pubkey = match self.authenticate(req, body_bytes).await? {
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

    // MARK: - Router

    async fn handle_parsed_http_request(
        &self,
        parsed_request: &ParsedRequest,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        if let Some(url_params) = route_match(
            &Method::PUT,
            "/user-info/:pubkey/:deviceToken",
            parsed_request,
        ) {
            return self.handle_user_info(parsed_request, &url_params).await;
        }

        if let Some(url_params) = route_match(
            &Method::DELETE,
            "/user-info/:pubkey/:deviceToken",
            parsed_request,
        ) {
            return self
                .handle_user_info_remove(parsed_request, &url_params)
                .await;
        }

        if let Some(url_params) = route_match(
            &Method::GET,
            "/user-info/:pubkey/:deviceToken/preferences",
            parsed_request,
        ) {
            return self.get_user_settings(parsed_request, &url_params).await;
        }

        if let Some(url_params) = route_match(
            &Method::PUT,
            "/user-info/:pubkey/:deviceToken/preferences",
            parsed_request,
        ) {
            return self.set_user_settings(parsed_request, &url_params).await;
        }

        // NIP-44 E2E encryption: Register device pubkey for encrypted notifications
        if let Some(url_params) = route_match(
            &Method::PUT,
            "/user-info/:pubkey/:deviceToken/encryption-key",
            parsed_request,
        ) {
            return self
                .handle_set_encryption_key(parsed_request, &url_params)
                .await;
        }

        Ok(APIResponse {
            status: StatusCode::NOT_FOUND,
            body: json!({ "error": "Not found" }),
        })
    }

    // MARK: - Authentication

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

    // MARK: - Endpoint handlers

    async fn handle_user_info(
        &self,
        req: &ParsedRequest,
        url_params: &HashMap<&str, String>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        // Early return if `deviceToken` is missing
        let device_token = match url_params.get("deviceToken") {
            Some(token) => token,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "deviceToken is required on the URL" }),
                })
            }
        };

        // Early return if `pubkey` is missing
        let pubkey = match url_params.get("pubkey") {
            Some(key) => key,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "pubkey is required on the URL" }),
                })
            }
        };

        // Validate the `pubkey` and prepare it for use
        let pubkey = match nostr::PublicKey::from_hex(pubkey) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid pubkey" }),
                })
            }
        };

        // Early return if `pubkey` does not match `req.authorized_pubkey`
        if pubkey != req.authorized_pubkey {
            return Ok(APIResponse {
                status: StatusCode::FORBIDDEN,
                body: json!({ "error": "Forbidden" }),
            });
        }

        // Parse platform from request body (optional, defaults to "ios")
        // Android clients should send { "platform": "android" }
        let platform = match req.body_json().ok().and_then(|b| b.get("platform").and_then(|p| p.as_str()).map(String::from)) {
            Some(p) => Platform::from_str(&p),
            None => Platform::Ios, // Default to iOS for backwards compatibility
        };

        // Proceed with the main logic after passing all checks
        self.notification_manager
            .save_user_device_info_if_not_present(pubkey, device_token, platform)
            .await?;
        Ok(APIResponse {
            status: StatusCode::OK,
            body: json!({
                "message": "User info saved successfully",
                "platform": platform.as_str()
            }),
        })
    }

    async fn handle_user_info_remove(
        &self,
        req: &ParsedRequest,
        url_params: &HashMap<&str, String>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        // Early return if `deviceToken` is missing
        let device_token = match url_params.get("deviceToken") {
            Some(token) => token,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "deviceToken is required on the URL" }),
                })
            }
        };

        // Early return if `pubkey` is missing
        let pubkey = match url_params.get("pubkey") {
            Some(key) => key,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "pubkey is required on the URL" }),
                })
            }
        };

        // Validate the `pubkey` and prepare it for use
        let pubkey = match nostr::PublicKey::from_hex(pubkey) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid pubkey" }),
                })
            }
        };

        // Early return if `pubkey` does not match `req.authorized_pubkey`
        if pubkey != req.authorized_pubkey {
            return Ok(APIResponse {
                status: StatusCode::FORBIDDEN,
                body: json!({ "error": "Forbidden" }),
            });
        }

        // Proceed with the main logic after passing all checks
        self.notification_manager
            .remove_user_device_info(pubkey, device_token)
            .await?;

        Ok(APIResponse {
            status: StatusCode::OK,
            body: json!({ "message": "User info removed successfully" }),
        })
    }

    async fn set_user_settings(
        &self,
        req: &ParsedRequest,
        url_params: &HashMap<&str, String>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        // Early return if `deviceToken` is missing
        let device_token = match url_params.get("deviceToken") {
            Some(token) => token,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "deviceToken is required on the URL" }),
                })
            }
        };

        // Early return if `pubkey` is missing
        let pubkey = match url_params.get("pubkey") {
            Some(key) => key,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "pubkey is required on the URL" }),
                })
            }
        };

        // Validate the `pubkey` and prepare it for use
        let pubkey = match nostr::PublicKey::from_hex(pubkey) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid pubkey" }),
                })
            }
        };

        // Early return if `pubkey` does not match `req.authorized_pubkey`
        if pubkey != req.authorized_pubkey {
            return Ok(APIResponse {
                status: StatusCode::FORBIDDEN,
                body: json!({ "error": "Forbidden" }),
            });
        }

        // Proceed with the main logic after passing all checks
        let body = req.body_json()?;

        let settings: UserNotificationSettings = match from_value(body.clone()) {
            Ok(settings) => settings,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid settings" }),
                });
            }
        };

        self.notification_manager
            .save_user_notification_settings(
                &req.authorized_pubkey,
                device_token.to_string(),
                settings,
            )
            .await?;

        Ok(APIResponse {
            status: StatusCode::OK,
            body: json!({ "message": "User settings saved successfully" }),
        })
    }

    async fn get_user_settings(
        &self,
        req: &ParsedRequest,
        url_params: &HashMap<&str, String>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        // Early return if `deviceToken` is missing
        let device_token = match url_params.get("deviceToken") {
            Some(token) => token,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "deviceToken is required on the URL" }),
                })
            }
        };

        // Early return if `pubkey` is missing
        let pubkey = match url_params.get("pubkey") {
            Some(key) => key,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "pubkey is required on the URL" }),
                })
            }
        };

        // Validate the `pubkey` and prepare it for use
        let pubkey = match nostr::PublicKey::from_hex(pubkey) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid pubkey" }),
                })
            }
        };

        // Early return if `pubkey` does not match `req.authorized_pubkey`
        if pubkey != req.authorized_pubkey {
            return Ok(APIResponse {
                status: StatusCode::FORBIDDEN,
                body: json!({ "error": "Forbidden" }),
            });
        }

        // Proceed with the main logic after passing all checks
        let settings = self
            .notification_manager
            .get_user_notification_settings(&req.authorized_pubkey, device_token.to_string())
            .await?;

        Ok(APIResponse {
            status: StatusCode::OK,
            body: json!(settings),
        })
    }

    /// Register a device pubkey for NIP-44 encrypted notifications
    ///
    /// When a client registers their device pubkey, the server will encrypt
    /// all future notification payloads to that pubkey. The client decrypts
    /// locally using the corresponding private key.
    ///
    /// Request body: { "device_pubkey": "<64-char hex pubkey>" }
    async fn handle_set_encryption_key(
        &self,
        req: &ParsedRequest,
        url_params: &HashMap<&str, String>,
    ) -> Result<APIResponse, Box<dyn std::error::Error>> {
        // Early return if NIP-44 encryption is not enabled on this server
        if self.server_keys.is_none() {
            return Ok(APIResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: json!({ "error": "NIP-44 encryption is not enabled on this server" }),
            });
        }

        // Early return if `deviceToken` is missing
        let device_token = match url_params.get("deviceToken") {
            Some(token) => token,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "deviceToken is required on the URL" }),
                })
            }
        };

        // Early return if `pubkey` is missing
        let pubkey = match url_params.get("pubkey") {
            Some(key) => key,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "pubkey is required on the URL" }),
                })
            }
        };

        // Validate the `pubkey` and prepare it for use
        let pubkey = match nostr::PublicKey::from_hex(pubkey) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid pubkey" }),
                })
            }
        };

        // Early return if `pubkey` does not match `req.authorized_pubkey`
        // Only the owner of this device registration can set its encryption key
        if pubkey != req.authorized_pubkey {
            return Ok(APIResponse {
                status: StatusCode::FORBIDDEN,
                body: json!({ "error": "Forbidden" }),
            });
        }

        // Parse and validate the device pubkey from request body
        let body = req.body_json()?;
        let device_pubkey_hex = match body.get("device_pubkey").and_then(|v| v.as_str()) {
            Some(hex) => hex,
            None => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "device_pubkey is required in request body" }),
                })
            }
        };

        // Validate device_pubkey is a valid nostr public key
        let device_pubkey = match nostr::PublicKey::from_hex(device_pubkey_hex) {
            Ok(key) => key,
            Err(_) => {
                return Ok(APIResponse {
                    status: StatusCode::BAD_REQUEST,
                    body: json!({ "error": "Invalid device_pubkey: must be 64-char hex" }),
                })
            }
        };

        // Store the device pubkey for this user/device pair
        // Returns typed error if user/device not registered yet
        match self
            .notification_manager
            .save_device_pubkey(&pubkey, device_token, &device_pubkey)
            .await
        {
            Ok(()) => Ok(APIResponse {
                status: StatusCode::OK,
                body: json!({
                    "message": "Device encryption key registered successfully",
                    "device_pubkey": device_pubkey.to_hex()
                }),
            }),
            Err(e) => {
                // Match on typed error for robust error handling
                if e.downcast_ref::<NotificationManagerError>()
                    == Some(&NotificationManagerError::DeviceNotRegistered)
                {
                    Ok(APIResponse {
                        status: StatusCode::NOT_FOUND,
                        body: json!({
                            "error": "Device not registered",
                            "message": "Register the device first with PUT /user-info/:pubkey/:deviceToken"
                        }),
                    })
                } else {
                    // Re-propagate other errors for generic 500 handling
                    Err(e)
                }
            }
        }
    }
}

// MARK: - Extensions

impl Clone for APIHandler {
    fn clone(&self) -> Self {
        APIHandler {
            notification_manager: self.notification_manager.clone(),
            base_url: self.base_url.clone(),
            server_keys: self.server_keys.clone(),
        }
    }
}

// MARK: - Helper types

// Define enum error types including authentication error
#[derive(Debug, Error)]
enum APIError {
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
}

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

// MARK: - Helper functions

/// Matches the request to a specified route, returning a hashmap of the route parameters
/// e.g. GET /user/:id/info route against request GET /user/123/info matches to { "id": "123" }
fn route_match<'a>(
    method: &Method,
    path: &'a str,
    req: &ParsedRequest,
) -> Option<HashMap<&'a str, String>> {
    if method != req.method {
        return None;
    }
    let mut params = HashMap::new();
    let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let req_segments: Vec<&str> = req.uri.split('/').filter(|s| !s.is_empty()).collect();

    if path_segments.len() != req_segments.len() {
        return None;
    }

    for (i, segment) in path_segments.iter().enumerate() {
        if let Some(key) = segment.strip_prefix(':') {
            let value = req_segments[i].to_string();
            params.insert(key, value);
        } else if segment != &req_segments[i] {
            return None;
        }
    }

    Some(params)
}
