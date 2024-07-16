use nostr::util::JsonUtil;
use nostr::{RelayMessage, ClientMessage};
use std::sync::Arc;
use tokio::sync::Mutex;
use serde_json::Value;
use crate::notification_manager::NotificationManager;
use std::str::FromStr;
use std::net::TcpStream;
use tungstenite::{accept, WebSocket};
use log;
use std::fmt::{self, Debug};

const MAX_CONSECUTIVE_ERRORS: u32 = 10;

pub struct RelayConnection {
    websocket: WebSocket<TcpStream>,
    notification_manager: Arc<Mutex<NotificationManager>>
}

impl RelayConnection {
    
    // MARK: - Initializers
    
    pub fn new(stream: TcpStream, notification_manager: Arc<Mutex<NotificationManager>>) -> Result<Self, Box<dyn std::error::Error>> {
        let address = stream.peer_addr()?;
        let websocket = accept(stream)?;
        log::info!("Accepted connection from {:?}", address);
        Ok(RelayConnection {
            websocket,
            notification_manager
        })
    }
    
    pub async fn run(stream: TcpStream, notification_manager: Arc<Mutex<NotificationManager>>) -> Result<(), Box<dyn std::error::Error>> {
        let mut connection = RelayConnection::new(stream, notification_manager)?;
        Ok(connection.run_loop().await?)
    }
    
    // MARK: - Connection Runtime management
    
    pub async fn run_loop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut consecutive_errors = 0;
        log::debug!("Starting run loop for connection with {:?}", self.websocket);
        loop {
            match self.run_loop_iteration().await {
                Ok(_) => {
                    consecutive_errors = 0;
                }
                Err(e) => {
                    log::error!("Error in websocket connection with {:?}: {:?}", self.websocket, e);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        log::error!("Too many consecutive errors, closing connection with {:?}", self.websocket);
                        return Err(e);
                    }
                }
            }
        }
    }
    
    pub async fn run_loop_iteration<'a>(&'a mut self) -> Result<(), Box<dyn std::error::Error>> {
        let websocket = &mut self.websocket;
        let raw_message = websocket.read()?;
        if raw_message.is_text() {
            let message: ClientMessage = ClientMessage::from_value(Value::from_str(raw_message.to_text()?)?)?;
            let response = self.handle_client_message(message).await?;
            self.websocket.send(tungstenite::Message::text(response.try_as_json()?))?;
        }
        Ok(())
    }
    
    // MARK: - Message handling
    
    async fn handle_client_message<'b>(&'b self, message: ClientMessage) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        match message {
            ClientMessage::Event(event) => {
                log::info!("Received event: {:?}", event);
                {
                    // TODO: Reduce resource contention by reducing the scope of the mutex into NotificationManager logic.
                    let mutex_guard = self.notification_manager.lock().await;
                    mutex_guard.send_notifications_if_needed(&event).await?;
                };  // Only hold the mutex for as little time as possible.
                let notice_message = format!("blocked: This relay does not store events");
                let response = RelayMessage::Ok { event_id: event.id, status: false, message: notice_message };
                Ok(response)
            }
            _ => {
                log::info!("Received unsupported message: {:?}", message);
                let notice_message = format!("Unsupported message: {:?}", message);
                let response = RelayMessage::Notice { message: notice_message };
                Ok(response)
            }
        }
    }
}

impl Debug for RelayConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RelayConnection with websocket: {:?}", self.websocket)
    }
}
