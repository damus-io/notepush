use a2::{Client, ClientConfig, DefaultNotificationBuilder, NotificationBuilder};
use nostr::event::EventId;
use nostr::key::PublicKey;
use nostr::types::Timestamp;
use rusqlite;
use rusqlite::params;
use std::collections::HashSet;

use std::fs::File;
use super::mute_manager::MuteManager;
use nostr::Event;
use super::SqlStringConvertible;
use super::ExtendedEvent;
use r2d2_sqlite::SqliteConnectionManager;
use r2d2;

// MARK: - NotificationManager

pub struct NotificationManager {
    db: r2d2::Pool<SqliteConnectionManager>,
    relay_url: String,
    apns_private_key_path: String,
    apns_private_key_id: String,
    apns_team_id: String,
    apns_environment: a2::client::Endpoint,
    apns_topic: String,

    mute_manager: MuteManager,
}

impl NotificationManager {
    // MARK: - Initialization
    
    pub async fn new(db: r2d2::Pool<SqliteConnectionManager>, relay_url: String, apns_private_key_path: String, apns_private_key_id: String, apns_team_id: String, apns_environment: a2::client::Endpoint, apns_topic: String) -> Result<Self, Box<dyn std::error::Error>> {
        let mute_manager = MuteManager::new(relay_url.clone()).await?;
        
        let connection = db.get()?;
        Self::setup_database(&connection)?;

        Ok(Self {
            relay_url,
            apns_private_key_path,
            apns_private_key_id,
            apns_team_id,
            apns_environment,
            apns_topic,
            db,
            mute_manager,
        })
    }
    
    // MARK: - Database setup operations

    pub fn setup_database(db: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
        db.execute(
            "CREATE TABLE IF NOT EXISTS notifications (
                id TEXT PRIMARY KEY,
                event_id TEXT,
                pubkey TEXT,
                received_notification BOOLEAN
            )",
            [],
        )?;

        db.execute(
            "CREATE INDEX IF NOT EXISTS notification_event_id_index ON notifications (event_id)",
            [],
        )?;

        db.execute(
            "CREATE TABLE IF NOT EXISTS user_info (
                id TEXT PRIMARY KEY,
                device_token TEXT,
                pubkey TEXT
            )",
            [],
        )?;

        db.execute(
            "CREATE INDEX IF NOT EXISTS user_info_pubkey_index ON user_info (pubkey)",
            [],
        )?;

        Self::add_column_if_not_exists(&db, "notifications", "sent_at", "INTEGER")?;
        Self::add_column_if_not_exists(&db, "user_info", "added_at", "INTEGER")?;
        
        Ok(())
    }

    fn add_column_if_not_exists(db: &rusqlite::Connection, table_name: &str, column_name: &str, column_type: &str) -> Result<(), rusqlite::Error> {
        let query = format!("PRAGMA table_info({})", table_name);
        let mut stmt = db.prepare(&query)?;
        let column_names: Vec<String> = stmt.query_map([], |row| row.get(1))?
            .filter_map(|r| r.ok())
            .collect();

        if !column_names.contains(&column_name.to_string()) {
            let query = format!("ALTER TABLE {} ADD COLUMN {} {}", table_name, column_name, column_type);
            db.execute(&query, [])?;
        }
        Ok(())
    }
    
    // MARK: - Business logic

    pub async fn send_notifications_if_needed(&self, event: &Event) -> Result<(), Box<dyn std::error::Error>> {
        let one_week_ago = nostr::Timestamp::now() - 7 * 24 * 60 * 60;
        if event.created_at < one_week_ago {
            return Ok(());
        }

        let pubkeys_to_notify = self.pubkeys_to_notify_for_event(event).await?;

        for pubkey in pubkeys_to_notify {
            self.send_event_notifications_to_pubkey(event, &pubkey).await?;
            self.db.get()?.execute(
                "INSERT OR REPLACE INTO notifications (id, event_id, pubkey, received_notification, sent_at)
                VALUES (?, ?, ?, ?, ?)",
                params![
                    format!("{}:{}", event.id, pubkey),
                    event.id.to_sql_string(),
                    pubkey.to_sql_string(),
                    true,
                    nostr::Timestamp::now().to_sql_string(),
                ],
            )?;
        }
        Ok(())
    }

    async fn pubkeys_to_notify_for_event(&self, event: &Event) -> Result<HashSet<nostr::PublicKey>, Box<dyn std::error::Error>> {
        let notification_status = self.get_notification_status(event)?;
        let relevant_pubkeys = self.pubkeys_relevant_to_event(event)?;
        let pubkeys_that_received_notification = notification_status.pubkeys_that_received_notification();
        let relevant_pubkeys_yet_to_receive: HashSet<PublicKey> = relevant_pubkeys
            .difference(&pubkeys_that_received_notification)
            .filter(|&x| *x != event.pubkey)
            .cloned()
            .collect();

        let mut pubkeys_to_notify = HashSet::new();
        for pubkey in relevant_pubkeys_yet_to_receive {
            let should_mute: bool = self.mute_manager.should_mute_notification_for_pubkey(event, &pubkey).await;
            if !should_mute {
                pubkeys_to_notify.insert(pubkey);
            }
        }
        Ok(pubkeys_to_notify)
    }

    fn pubkeys_relevant_to_event(&self, event: &Event) -> Result<HashSet<PublicKey>, Box<dyn std::error::Error>> {
        let mut relevant_pubkeys = event.relevant_pubkeys();
        let referenced_event_ids = event.referenced_event_ids();
        for referenced_event_id in referenced_event_ids {
            let pubkeys_relevant_to_referenced_event = self.pubkeys_subscribed_to_event_id(&referenced_event_id)?;
            relevant_pubkeys.extend(pubkeys_relevant_to_referenced_event);
        }
        Ok(relevant_pubkeys)
    }

    fn pubkeys_subscribed_to_event(&self, event: &Event) -> Result<HashSet<PublicKey>, Box<dyn std::error::Error>> {
        self.pubkeys_subscribed_to_event_id(&event.id)
    }

    fn pubkeys_subscribed_to_event_id(&self, event_id: &EventId) -> Result<HashSet<PublicKey>, Box<dyn std::error::Error>> {
        let connection = self.db.get()?;
        let mut stmt = connection.prepare("SELECT pubkey FROM notifications WHERE event_id = ?")?;
        let pubkeys = stmt.query_map([event_id.to_sql_string()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .filter_map(|r: String| PublicKey::from_sql_string(r).ok())
            .collect();
        Ok(pubkeys)
    }

    async fn send_event_notifications_to_pubkey(&self, event: &Event, pubkey: &PublicKey) -> Result<(), Box<dyn std::error::Error>> {
        let user_device_tokens = self.get_user_device_tokens(pubkey)?;
        for device_token in user_device_tokens {
            self.send_event_notification_to_device_token(event, &device_token).await?;
        }
        Ok(())
    }

    fn get_user_device_tokens(&self, pubkey: &PublicKey) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let connection = self.db.get()?;
        let mut stmt = connection.prepare("SELECT device_token FROM user_info WHERE pubkey = ?")?;
        let device_tokens = stmt.query_map([pubkey.to_sql_string()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(device_tokens)
    }

    fn get_notification_status(&self, event: &Event) -> Result<NotificationStatus, Box<dyn std::error::Error>> {
        let connection = self.db.get()?;
        let mut stmt = connection.prepare("SELECT pubkey, received_notification FROM notifications WHERE event_id = ?")?;
        let rows: std::collections::HashMap<PublicKey, bool> = stmt.query_map([event.id.to_sql_string()], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
            .filter_map(|r: Result<(String, bool), rusqlite::Error>| r.ok())
            .filter_map(|r: (String, bool)| {
                let pubkey = PublicKey::from_sql_string(r.0).ok()?;
                let received_notification = r.1;
                Some((pubkey, received_notification))
            })
            .collect();

        let mut status_info = std::collections::HashMap::new();
        for row in rows {
            let (pubkey, received_notification) = row;
            status_info.insert(pubkey, received_notification);
        }

        Ok(NotificationStatus { status_info })
    }

    async fn send_event_notification_to_device_token(&self, event: &Event, device_token: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (title, subtitle, body) = self.format_notification_message(event);

        let builder = DefaultNotificationBuilder::new()
            .set_title(&title)
            .set_subtitle(&subtitle)
            .set_body(&body)
            .set_mutable_content()
            .set_content_available();
        
        let mut payload = builder.build(
            device_token,
            Default::default()
        );
        payload.add_custom_data("nostr_event", event);
        payload.options.apns_topic = Some(self.apns_topic.as_str());
        
        let mut file = File::open(&self.apns_private_key_path)?;
        
        let client = Client::token(
            &mut file,
            &self.apns_private_key_id,
            &self.apns_team_id,
            ClientConfig::new(self.apns_environment.clone())
        )?;
        
        let _response = client.send(payload).await?;
        Ok(())
    }

    fn format_notification_message(&self, event: &Event) -> (String, String, String) {
        let title = "New activity".to_string();
        let subtitle = format!("From: {}", event.pubkey);
        let body = event.content.clone();
        (title, subtitle, body)
    }

    pub fn save_user_device_info(&self, pubkey: &str, device_token: &str) -> Result<(), Box<dyn std::error::Error>> {
        let current_time_unix = Timestamp::now();
        self.db.get()?.execute(
            "INSERT OR REPLACE INTO user_info (id, pubkey, device_token, added_at) VALUES (?, ?, ?, ?)",
            params![
                format!("{}:{}", pubkey, device_token), 
                pubkey,
                device_token,
                current_time_unix.to_sql_string()
            ],
        )?;
        Ok(())
    }

    pub fn remove_user_device_info(&self, pubkey: &str, device_token: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.db.get()?.execute(
            "DELETE FROM user_info WHERE pubkey = ? AND device_token = ?",
            params![pubkey, device_token],
        )?;
        Ok(())
    }
}

struct NotificationStatus {
    status_info: std::collections::HashMap<PublicKey, bool>,
}

impl NotificationStatus {
    fn pubkeys_that_received_notification(&self) -> HashSet<PublicKey> {
        self.status_info
            .iter()
            .filter(|&(_, &received_notification)| received_notification)
            .map(|(pubkey, _)| pubkey.clone())
            .collect()
    }
}
