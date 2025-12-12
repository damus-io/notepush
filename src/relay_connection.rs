//! WebSocket relay connection handler.
//!
//! Handles Nostr protocol messages over WebSocket and routes events
//! to the notification system after filtering.

use crate::event_filter::{EventFilter, FilterResult};
use crate::notification_manager::NotificationManager;
use futures::sink::SinkExt;
use futures::StreamExt;
use hyper::upgrade::Upgraded;
use hyper_tungstenite::{HyperWebsocket, WebSocketStream};
use hyper_util::rt::TokioIo;
use nostr::util::JsonUtil;
use nostr::{ClientMessage, Event, RelayMessage};
use serde_json::Value;
use std::fmt::{self, Debug};
use std::str::FromStr;
use std::sync::Arc;
use tungstenite::{Error, Message};

const MAX_CONSECUTIVE_ERRORS: u32 = 10;

pub struct RelayConnection {
    notification_manager: Arc<NotificationManager>,
    event_filter: Arc<EventFilter>,
}

impl RelayConnection {
    pub async fn new(
        notification_manager: Arc<NotificationManager>,
        event_filter: Arc<EventFilter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        log::info!("Accepted websocket connection");
        Ok(RelayConnection {
            notification_manager,
            event_filter,
        })
    }

    pub async fn run(
        websocket: HyperWebsocket,
        notification_manager: Arc<NotificationManager>,
        event_filter: Arc<EventFilter>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut connection = RelayConnection::new(notification_manager, event_filter).await?;
        connection.run_loop(websocket).await
    }

    pub async fn run_loop(
        &mut self,
        websocket: HyperWebsocket,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut consecutive_errors = 0;
        log::debug!("Starting run loop for connection with {:?}", websocket);
        let mut websocket_stream = websocket.await?;

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

    /// Handles a parsed Nostr client message.
    ///
    /// For EVENT messages, applies filtering before notification processing
    /// to prevent DoS attacks from spam events.
    async fn handle_client_message(
        &self,
        message: ClientMessage,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        match message {
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

    /// Handles an incoming Nostr event.
    ///
    /// Flow:
    /// 1. Run event through noteguard filter pipeline
    /// 2. If rejected, return rejection message (or fake accept for shadow reject)
    /// 3. If accepted, save mute lists and send notifications
    /// 4. Return "blocked" message (this relay doesn't store events)
    async fn handle_event(
        &self,
        event: Event,
    ) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        log::info!("Received event with id: {:?}", event.id.to_hex());
        log::debug!("Event received: {:?}", event);

        // Apply noteguard filter pipeline BEFORE any expensive operations.
        // This prevents DoS by rejecting spam before triggering remote lookups.
        // We use the pubkey as source_info for per-user rate limiting.
        let filter_result = self.event_filter.filter_event(&event, &event.pubkey.to_hex());

        match filter_result {
            FilterResult::Accept => {
                // Event passed filters - process normally
            }
            FilterResult::Reject(msg) => {
                log::info!("Event {} rejected by filter: {}", event.id.to_hex(), msg);
                return Ok(RelayMessage::Ok {
                    event_id: event.id,
                    status: false,
                    message: msg,
                });
            }
            FilterResult::ShadowReject => {
                // Pretend to accept but don't process - hides filter from spammers
                log::info!("Event {} shadow-rejected by filter", event.id.to_hex());
                return Ok(RelayMessage::Ok {
                    event_id: event.id,
                    status: true,
                    message: String::new(),
                });
            }
        }

        // Event passed filters - now safe to do expensive operations
        self.notification_manager
            .event_saver
            .save_if_needed(&event)
            .await?;

        self.notification_manager
            .send_notifications_if_needed(&event)
            .await?;

        // This relay doesn't store events, just sends notifications
        Ok(RelayMessage::Ok {
            event_id: event.id,
            status: false,
            message: "blocked: This relay does not store events".to_string(),
        })
    }
}

impl Debug for RelayConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelayConnection with websocket")
    }
}
