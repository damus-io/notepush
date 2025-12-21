use notepush::notification_manager::NotificationManager;
use notepush::Platform;
use futures::sink::SinkExt;
use futures::StreamExt;
use hyper::upgrade::Upgraded;
use hyper_tungstenite::{HyperWebsocket, WebSocketStream};
use hyper_util::rt::TokioIo;
use nostr::util::JsonUtil;
use nostr::{Alphabet, ClientMessage, Event, Kind, PublicKey, RelayMessage, SingleLetterTag, TagKind, Timestamp};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::{self, Debug};
use std::str::FromStr;
use std::sync::Arc;
use tungstenite::{Error, Message};
use uuid::Uuid;

const MAX_CONSECUTIVE_ERRORS: u32 = 10;
/// Maximum age of auth event in seconds (10 minutes per NIP-42)
const AUTH_EVENT_MAX_AGE_SECS: u64 = 600;
/// Kind for NIP-42 auth events
const KIND_AUTH: u16 = 22242;
/// Kind for device registration events (custom)
const KIND_DEVICE_REGISTRATION: u16 = 30078; // Ephemeral, replaceable

pub struct RelayConnection {
    notification_manager: Arc<NotificationManager>,
    /// The relay URL for validating auth events
    relay_url: String,
    /// Current auth challenge for this connection
    challenge: String,
    /// Authenticated public keys for this connection
    authenticated_pubkeys: HashSet<PublicKey>,
}

impl RelayConnection {
    // MARK: - Initializers

    pub async fn new(
        notification_manager: Arc<NotificationManager>,
        relay_url: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        log::info!("Accepted websocket connection");
        let challenge = Uuid::new_v4().to_string();
        Ok(RelayConnection {
            notification_manager,
            relay_url,
            challenge,
            authenticated_pubkeys: HashSet::new(),
        })
    }

    pub async fn run(
        websocket: HyperWebsocket,
        notification_manager: Arc<NotificationManager>,
        relay_url: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut connection = RelayConnection::new(notification_manager, relay_url).await?;
        connection.run_loop(websocket).await
    }

    // MARK: - Connection Runtime management

    pub async fn run_loop(
        &mut self,
        websocket: HyperWebsocket,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut consecutive_errors = 0;
        log::debug!("Starting run loop for connection");
        let mut websocket_stream = websocket.await?;

        // Send AUTH challenge immediately on connection
        let auth_challenge = RelayMessage::Auth {
            challenge: self.challenge.clone(),
        };
        websocket_stream
            .send(tungstenite::Message::text(auth_challenge.try_as_json()?))
            .await?;
        log::debug!("Sent AUTH challenge: {}", &self.challenge[..8]);

        while let Some(raw_message) = websocket_stream.next().await {
            match self
                .run_loop_iteration_if_raw_message_is_ok(raw_message, &mut websocket_stream)
                .await
            {
                Ok(_) => {
                    consecutive_errors = 0;
                }
                Err(e) => {
                    log::error!("Error in websocket connection: {:?}", e);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        log::error!("Too many consecutive errors, closing connection");
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn run_loop_iteration_if_raw_message_is_ok(
        &mut self,
        raw_message: Result<Message, Error>,
        stream: &mut WebSocketStream<TokioIo<Upgraded>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let raw_message = raw_message?;
        self.run_loop_iteration(raw_message, stream).await
    }

    pub async fn run_loop_iteration(
        &mut self,
        raw_message: Message,
        stream: &mut WebSocketStream<TokioIo<Upgraded>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !raw_message.is_text() {
            return Ok(());
        }

        let message: ClientMessage =
            ClientMessage::from_value(Value::from_str(raw_message.to_text()?)?)?;

        let response = self.handle_client_message(message).await?;
        stream
            .send(tungstenite::Message::text(response.try_as_json()?))
            .await?;

        Ok(())
    }

    // MARK: - Message handling

    async fn handle_client_message(
        &mut self,
        message: ClientMessage,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        match message {
            ClientMessage::Auth(event) => self.handle_auth(*event).await,
            ClientMessage::Event(event) => self.handle_event(*event).await,
            _ => {
                log::info!("Received unsupported Nostr client message");
                log::debug!("Unsupported Nostr client message: {:?}", message);
                Ok(RelayMessage::Notice {
                    message: "Unsupported message.".to_string(),
                })
            }
        }
    }

    /// Handle NIP-42 AUTH message
    async fn handle_auth(
        &mut self,
        event: Event,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        log::info!("Received AUTH from pubkey: {}", event.pubkey.to_hex());

        // Verify the auth event
        if let Err(reason) = self.verify_auth_event(&event) {
            log::warn!("AUTH failed: {}", reason);
            return Ok(RelayMessage::Ok {
                event_id: event.id,
                status: false,
                message: format!("auth-required: {}", reason),
            });
        }

        // Add pubkey to authenticated set
        self.authenticated_pubkeys.insert(event.pubkey);
        log::info!("Authenticated pubkey: {}", event.pubkey.to_hex());

        Ok(RelayMessage::Ok {
            event_id: event.id,
            status: true,
            message: "".to_string(),
        })
    }

    /// Verify a NIP-42 auth event
    fn verify_auth_event(&self, event: &Event) -> Result<(), String> {
        // Check kind
        if event.kind != Kind::Authentication {
            return Err(format!("Invalid kind: expected {}, got {}", KIND_AUTH, event.kind.as_u16()));
        }

        // Verify signature
        if event.verify().is_err() {
            return Err("Invalid signature".to_string());
        }

        // Check timestamp (within 10 minutes)
        let now = Timestamp::now();
        let event_time = event.created_at;
        let delta = if now > event_time {
            now.as_u64() - event_time.as_u64()
        } else {
            event_time.as_u64() - now.as_u64()
        };
        if delta > AUTH_EVENT_MAX_AGE_SECS {
            return Err(format!("Event too old: {} seconds", delta));
        }

        // Check challenge tag
        let challenge = event.get_tag_content(TagKind::Challenge);
        if challenge.as_deref() != Some(&self.challenge) {
            return Err("Invalid challenge".to_string());
        }

        // Check relay tag (allow some flexibility in URL matching)
        let relay = event.get_tag_content(TagKind::Relay);
        let Some(relay_url) = relay else {
            return Err("Missing relay tag".to_string());
        };

        // Basic URL matching - check if domains match
        if !urls_match(&self.relay_url, &relay_url) {
            return Err(format!("Relay mismatch: expected {}, got {}", self.relay_url, relay_url));
        }

        Ok(())
    }

    /// Handle EVENT message
    async fn handle_event(
        &mut self,
        event: Event,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        log::info!("Received event kind {} from pubkey: {}", event.kind.as_u16(), event.pubkey.to_hex());

        // Check if this is a device registration event
        if event.kind.as_u16() == KIND_DEVICE_REGISTRATION {
            return self.handle_device_registration(event).await;
        }

        // For other events, process notifications as before
        log::debug!("Event received: {:?}", event);
        self.notification_manager
            .event_saver
            .save_if_needed(&event)
            .await?;
        self.notification_manager
            .send_notifications_if_needed(&event)
            .await?;

        Ok(RelayMessage::Ok {
            event_id: event.id,
            status: false,
            message: "blocked: This relay does not store events".to_string(),
        })
    }

    /// Handle device registration event (kind 30078)
    async fn handle_device_registration(
        &mut self,
        event: Event,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        // Require authentication
        if !self.authenticated_pubkeys.contains(&event.pubkey) {
            return Ok(RelayMessage::Ok {
                event_id: event.id,
                status: false,
                message: "auth-required: must authenticate before registering device".to_string(),
            });
        }

        // Verify the event signature
        if event.verify().is_err() {
            return Ok(RelayMessage::Ok {
                event_id: event.id,
                status: false,
                message: "invalid: bad signature".to_string(),
            });
        }

        // Extract device token from 'd' tag
        let d_tag = TagKind::SingleLetter(SingleLetterTag::lowercase(Alphabet::D));
        let Some(device_token) = event.get_tag_content(d_tag) else {
            return Ok(RelayMessage::Ok {
                event_id: event.id,
                status: false,
                message: "invalid: missing 'd' tag with device token".to_string(),
            });
        };

        // Extract platform from content or default to android
        let platform = if event.content.contains("ios") {
            Platform::Ios
        } else {
            Platform::Android
        };

        // Register the device
        let pubkey_hex = event.pubkey.to_hex();
        log::info!("Registering device {} for pubkey {} ({})", device_token, &pubkey_hex[..8], platform.as_str());

        match self.notification_manager
            .save_user_device_info_if_not_present(event.pubkey, &device_token, platform)
            .await
        {
            Ok(_) => {
                log::info!("Device registered successfully");
                Ok(RelayMessage::Ok {
                    event_id: event.id,
                    status: true,
                    message: "device registered".to_string(),
                })
            }
            Err(e) => {
                log::error!("Failed to register device: {}", e);
                Ok(RelayMessage::Ok {
                    event_id: event.id,
                    status: false,
                    message: format!("error: {}", e),
                })
            }
        }
    }
}

/// Check if two relay URLs match (basic domain comparison)
fn urls_match(url1: &str, url2: &str) -> bool {
    // Extract domain from URLs
    fn extract_domain(url: &str) -> Option<&str> {
        url.trim_start_matches("wss://")
            .trim_start_matches("ws://")
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .map(|s| s.split(':').next().unwrap_or(s))
    }

    match (extract_domain(url1), extract_domain(url2)) {
        (Some(d1), Some(d2)) => d1.eq_ignore_ascii_case(d2),
        _ => false,
    }
}

impl Debug for RelayConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelayConnection(auth={} pubkeys)", self.authenticated_pubkeys.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_urls_match() {
        assert!(urls_match("wss://relay.example.com", "wss://relay.example.com"));
        assert!(urls_match("wss://relay.example.com/", "wss://relay.example.com"));
        assert!(urls_match("wss://Relay.Example.COM", "wss://relay.example.com"));
        assert!(urls_match("wss://relay.example.com:443", "wss://relay.example.com"));
        assert!(!urls_match("wss://relay1.example.com", "wss://relay2.example.com"));
    }
}
