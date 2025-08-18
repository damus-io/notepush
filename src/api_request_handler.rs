use crate::config::NotePushConfig;
use crate::nip98_auth;
use crate::notification_manager::NotificationManager;
use crate::notification_manager::{KindsSettings, NotificationBackend, UserNotificationSettings};
use crate::relay_connection::RelayConnection;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper::body::Buf;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::Method;
use hyper::{Request, Response, StatusCode};
use log::warn;
use matchit::{Params, Router};
use nostr_sdk::prelude::form_urlencoded;
use nostr_sdk::{Event, PublicKey};
use serde::Serialize;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;
use thiserror::Error;

pub struct APIHandler {
    notification_manager: Arc<NotificationManager>,
    base_url: String,
}

impl APIHandler {
    pub fn new(notification_manager: Arc<NotificationManager>, base_url: String) -> Self {
        APIHandler {
            notification_manager,
            base_url,
        }
    }

    // MARK: - HTTP handling

    pub async fn handle_http_request(
        &self,
        req: Request<Incoming>,
        config: &NotePushConfig,
    ) -> Result<Response<Full<Bytes>>, hyper::http::Error> {
        // Check if the request is a websocket upgrade request.
        if hyper_tungstenite::is_upgrade_request(&req) {
            return match self.handle_websocket_upgrade(req).await {
                Ok(response) => Ok(response),
                Err(err) => {
                    log::error!("Error handling websocket upgrade request: {}", err);
                    Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Full::new(Bytes::from("Internal server error")))?)
                }
            };
        }

        // Handle root request
        if req.uri().path() == "/" || req.uri().path() == "/index.html" {
            return Ok(Response::builder()
                .header("Content-Type", "text/html")
                .header("Access-Control-Allow-Origin", "*")
                .body(Full::new(Bytes::from_static(include_bytes!(
                    "./index.html"
                ))))?);
        }

        // If not, handle the request as a normal API request.
        let final_api_response: APIResponse = match self
            .try_to_handle_http_request(req, config)
            .await
        {
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
                        _ => APIResponse {
                            status: StatusCode::BAD_REQUEST,
                            body: json!({ "error": api_error.to_string() }),
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
            if let Err(e) = RelayConnection::run(websocket, new_notification_manager).await {
                log::error!("Error with websocket connection: {}", e.to_string());
            }
        });

        Ok(response)
    }

    async fn try_to_handle_http_request(
        &self,
        mut req: Request<Incoming>,
        config: &NotePushConfig,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let parsed_request = self.parse_http_request(&mut req).await?;
        let api_response: APIResponse = self
            .handle_parsed_http_request(&parsed_request, config)
            .await?;
        log::info!("[{}] {}: {}", req.method(), req.uri(), api_response.status);
        Ok(api_response)
    }

    async fn parse_http_request(
        &self,
        req: &mut Request<Incoming>,
    ) -> Result<ParsedRequest, Box<dyn std::error::Error + Send + Sync>> {
        // 1. Read the request body
        let body_buffer = req.body_mut().collect().await?.aggregate();
        let body_bytes = body_buffer.chunk();
        let body_bytes = if body_bytes.is_empty() {
            None
        } else {
            Some(body_bytes)
        };

        // 3. Parse the request
        Ok(ParsedRequest {
            path: req.uri().path().to_string(),
            absolute_uri: format!(
                "{}{}",
                self.base_url,
                req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("")
            ),
            method: req.method().clone(),
            body_bytes: body_bytes.map(|b| b.to_vec()),
            auth_header: req
                .headers()
                .get("Authorization")
                .and_then(|h| h.to_str().ok())
                .map(|h| h.to_string()),
            query: match req.uri().query() {
                Some(q) => form_urlencoded::parse(q.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect(),
                None => vec![],
            },
        })
    }

    // MARK: - Router

    async fn handle_parsed_http_request(
        &self,
        parsed_request: &ParsedRequest,
        config: &NotePushConfig,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        enum RouteMatch {
            UserInfo,
            UserSetting,
            NotifySetting,
            GetNotify,
            WebPushInfo,
        }
        let mut routes = Router::new();
        routes.insert("/web", RouteMatch::WebPushInfo)?;
        routes.insert("/user-info/{pubkey}/{deviceToken}", RouteMatch::UserInfo)?;
        routes.insert(
            "/user-info/{pubkey}/notify/{target}",
            RouteMatch::NotifySetting,
        )?;
        routes.insert("/user-info/{pubkey}/notify", RouteMatch::GetNotify)?;
        routes.insert(
            "/user-info/{pubkey}/{deviceToken}/preference",
            RouteMatch::UserSetting,
        )?;

        match routes.at(&parsed_request.path) {
            Ok(m) => match m.value {
                RouteMatch::UserInfo if parsed_request.method == Method::PUT => {
                    return self.handle_user_info(parsed_request, m.params).await;
                }
                RouteMatch::UserInfo if parsed_request.method == Method::DELETE => {
                    return self.handle_user_info_remove(parsed_request, m.params).await;
                }
                RouteMatch::UserSetting if parsed_request.method == Method::GET => {
                    return self.get_user_settings(parsed_request, m.params).await;
                }
                RouteMatch::UserSetting if parsed_request.method == Method::PUT => {
                    return self.set_user_settings(parsed_request, m.params).await;
                }
                RouteMatch::GetNotify if parsed_request.method == Method::GET => {
                    return self.get_notify_keys(parsed_request, m.params).await;
                }
                RouteMatch::NotifySetting if parsed_request.method == Method::PUT => {
                    return self.put_notify_key(parsed_request, m.params).await;
                }
                RouteMatch::NotifySetting if parsed_request.method == Method::DELETE => {
                    return self.delete_notify_key(parsed_request, m.params).await;
                }
                RouteMatch::WebPushInfo if parsed_request.method == Method::GET => {
                    #[derive(Serialize)]
                    struct WebPushInfo {
                        vaapi_key: Option<String>,
                    }
                    return Ok(APIResponse::ok_body(&WebPushInfo {
                        vaapi_key: config.fcm.as_ref().and_then(|c| c.vaapi_key.clone()),
                    }));
                }
                _ => {
                    // fallthrough to 404
                }
            },
            Err(e) => {
                warn!("Match failed: {}", e);
            }
        }

        Ok(APIResponse {
            status: StatusCode::NOT_FOUND,
            body: json!({ "error": "Not found" }),
        })
    }

    // MARK: - Endpoint handlers

    async fn handle_user_info(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let device_token = get_required_param(&url_params, "deviceToken")?;
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;

        let backend = match NotificationBackend::from_str(
            req.query
                .iter()
                .find(|c| c.0 == "backend")
                .map(|c| &c.1)
                .unwrap_or(&"apns".to_string()),
        ) {
            Ok(token) => token,
            Err(_) => return Ok(APIResponse::bad_request("Backend is invalid")),
        };

        // Early return if backend is not supported
        if !self.notification_manager.has_backend(backend) {
            return Ok(APIResponse::bad_request("Backend not supported"));
        }

        // Proceed with the main logic after passing all checks
        self.notification_manager
            .save_user_device_info(pubkey, &device_token, backend)
            .await?;
        Ok(APIResponse::ok("User info saved successfully"))
    }

    async fn handle_user_info_remove(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let device_token = get_required_param(&url_params, "deviceToken")?;
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;

        // Proceed with the main logic after passing all checks
        self.notification_manager
            .remove_user_device_info(&pubkey, &device_token)
            .await?;

        Ok(APIResponse::ok("User info removed successfully"))
    }

    async fn get_notify_keys(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;

        let keys = self.notification_manager.get_notify_keys(pubkey).await?;

        Ok(APIResponse::ok_body(&keys))
    }

    async fn put_notify_key(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;
        let target = get_required_param(&url_params, "target")?;
        let target = parse_pubkey(&target)?;

        let settings: KindsSettings = if let Some(s) = req
            .body_bytes
            .as_ref()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            s
        } else {
            return Ok(APIResponse::bad_request("Invalid or missing settings"));
        };

        self.notification_manager
            .save_notify_key(pubkey, target, settings.kinds)
            .await?;

        Ok(APIResponse::ok("Saved notification target successfully"))
    }

    async fn delete_notify_key(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;
        let target = get_required_param(&url_params, "target")?;
        let target = parse_pubkey(&target)?;

        self.notification_manager
            .delete_notify_key(pubkey, target)
            .await?;

        Ok(APIResponse::ok("Deleted notification target successfully"))
    }

    async fn set_user_settings(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let device_token = get_required_param(&url_params, "deviceToken")?;
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;

        let settings: UserNotificationSettings = if let Some(s) = req
            .body_bytes
            .as_ref()
            .and_then(|b| serde_json::from_slice(&b).ok())
        {
            s
        } else {
            return Ok(APIResponse::bad_request("Invalid or missing settings"));
        };

        self.notification_manager
            .save_user_notification_settings(&pubkey, device_token.to_string(), settings)
            .await?;

        Ok(APIResponse::ok("User settings saved successfully"))
    }

    async fn get_user_settings(
        &self,
        req: &ParsedRequest,
        url_params: Params<'_, '_>,
    ) -> Result<APIResponse, Box<dyn std::error::Error + Send + Sync>> {
        let device_token = get_required_param(&url_params, "deviceToken")?;
        let pubkey = get_required_param(&url_params, "pubkey")?;
        let pubkey = check_pubkey(&pubkey, req)?;

        // Proceed with the main logic after passing all checks
        let settings = self
            .notification_manager
            .get_user_notification_settings(&pubkey, &device_token)
            .await?;

        Ok(APIResponse::ok_body(&settings))
    }
}

// MARK: - Extensions

impl Clone for APIHandler {
    fn clone(&self) -> Self {
        APIHandler {
            notification_manager: self.notification_manager.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

// MARK: - Helper types

// Define enum error types including authentication error
#[derive(Debug, Error)]
enum APIError {
    #[error("Missing required parameter {0}")]
    MissingParameter(String),
    #[error("Invalid parameter {0}")]
    InvalidParameter(String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
}

struct ParsedRequest {
    absolute_uri: String,
    path: String,
    auth_header: Option<String>,
    method: Method,
    body_bytes: Option<Vec<u8>>,
    query: Vec<(String, String)>,
}

impl ParsedRequest {
    fn body_json(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        if let Some(body_bytes) = &self.body_bytes {
            Ok(serde_json::from_slice(body_bytes)?)
        } else {
            Ok(json!({}))
        }
    }

    fn authenticate(&self) -> Result<Event, APIError> {
        let auth = self
            .auth_header
            .as_ref()
            .ok_or(APIError::AuthenticationError(
                "Auth header missing".to_string(),
            ))?;
        nip98_auth::nip98_verify_auth_header(
            auth,
            &self.absolute_uri,
            self.method.as_str(),
            &self.body_bytes,
        )
        .map_err(|e| APIError::AuthenticationError(e.to_string()))
    }
}

struct APIResponse {
    status: StatusCode,
    body: Value,
}

impl APIResponse {
    pub fn ok(msg: &str) -> APIResponse {
        APIResponse {
            status: StatusCode::OK,
            body: json!({ "message": msg }),
        }
    }

    pub fn ok_body<T: Serialize>(body: &T) -> APIResponse {
        APIResponse {
            status: StatusCode::OK,
            body: json!(body),
        }
    }

    pub fn bad_request(msg: &str) -> APIResponse {
        APIResponse {
            status: StatusCode::BAD_REQUEST,
            body: json!({ "error": msg }),
        }
    }
}

fn get_required_param(params: &Params<'_, '_>, key: &str) -> Result<String, APIError> {
    match params.get(key) {
        Some(token) => urlencoding::decode(token)
            .map(|s| match s {
                Cow::Borrowed(s) => s.to_owned(),
                Cow::Owned(s) => s,
            })
            .map_err(|_| APIError::InvalidParameter(key.to_string())),
        None => Err(APIError::MissingParameter(key.to_string())),
    }
}

fn parse_pubkey(pubkey: &str) -> Result<PublicKey, APIError> {
    PublicKey::from_hex(pubkey).map_err(|_| APIError::InvalidParameter("pubkey".to_string()))
}

fn check_pubkey(pubkey: &str, req: &ParsedRequest) -> Result<PublicKey, APIError> {
    let pubkey = parse_pubkey(pubkey)?;
    let auth = req.authenticate()?;
    if pubkey != auth.pubkey {
        Err(APIError::AuthenticationError(
            "pubkey doesnt match authorized pubkey".to_string(),
        ))
    } else {
        Ok(pubkey)
    }
}
