use damus_push_notification_relay::log_describable::LogDescribable;
use nostr::util::JsonUtil;
use nostr::{RelayMessage, ClientMessage};
use std::sync::{Arc, Mutex};
use serde_json::Value;
use notification_manager::NotificationManager;
use std::str::FromStr;
use std::net::TcpStream;
use tungstenite::accept;
use log;
use std::fmt;

pub struct RelayConnection {
    stream: TcpStream,
    notification_manager: Arc<Mutex<NotificationManager>>
}

impl RelayConnection {
    pub fn new(stream: TcpStream, notification_manager: Arc<Mutex<NotificationManager>>) -> Self {
        RelayConnection {
            stream,
            notification_manager
        }
    }
    
    pub fn start(&self) -> Result<(), Box<dyn std::error::Error>> {
        let notification_manager = NotificationManager::new(None, None)?;
        let mut websocket = accept(self.stream)?;
        loop {
            match self.run_loop_iteration() {
                Ok(_) => {}
                Err(e) => {
                    log::error!("Error in {}: {:?}", self, e);
                }
            }
        }
    }
    
    fn run_loop_iteration(&self) -> Result<(), Box<dyn std::error::Error>> {
        let raw_message = self.stream.read()?;
        if raw_message.is_text() {
            let message: ClientMessage = ClientMessage::from_value(Value::from_str(raw_msg.to_text()?)?)?;
            let response = self.handle_client_message(message)?;
            self.stream.send(response.try_as_json()?)?;
        }
        Ok(())
    }
    
    fn handle_client_message(&self, message: ClientMessage) -> Result<RelayMessage, Box<dyn std::error::Error>> {
        match message {
            ClientMessage::Event(event) => {
                log::info("Received event: {:?}", event);
                self.notification_manager.lock()?.send_notification_if_needed(&event)?;
                let notice_message = format!("blocked: This relay does not store events");
                let response = RelayMessage::Ok { event_id: event.id, status: false, message: notice_message };
                Ok(response)
            }
            _ => {
                log::info("Received unsupported message: {:?}", message);
                let notice_message = format!("Unsupported message: {:?}", message);
                let response = RelayMessage::Notice { message: notice_message };
                Ok(response)
            }
        }
    }
}

impl fmt::Debug for RelayConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let peer_address = self.stream.peer_addr().map_or("unknown".to_string(), |addr| addr.to_string());
        let description = format!("relay connection with {}", peer_address);
        write!(f, "{}", description)
    }
}
