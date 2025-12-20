mod nostr_event_cache;
mod nostr_event_extensions;
pub mod nostr_network_helper;
pub mod utils;

use std::cmp::{max, min};
use nostr_event_extensions::{ExtendedEvent, SqlStringConvertible};

use a2::{Client, ClientConfig, DefaultNotificationBuilder, NotificationBuilder};
use nostr::key::PublicKey;
use nostr::nips::nip51::MuteList;
use nostr::types::Timestamp;
use nostr_sdk::JsonUtil;
use nostr_sdk::Kind;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use nostr::Event;
use nostr_event_extensions::Codable;
use nostr_event_extensions::MaybeConvertibleToMuteList;
use nostr_event_extensions::TimestampedMuteList;
use nostr_network_helper::NostrNetworkHelper;
use r2d2_sqlite::SqliteConnectionManager;
use std::fs::File;
use utils::should_mute_notification_for_mutelist;

use crate::ntfy_client::NtfyClient;
use crate::Platform;

// MARK: - Error types

/// Errors specific to NotificationManager operations
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationManagerError {
    /// The user/device pair is not registered (must call PUT /user-info first)
    DeviceNotRegistered,
}

impl std::fmt::Display for NotificationManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceNotRegistered => write!(
                f,
                "User/device registration not found. Register device first."
            ),
        }
    }
}

impl std::error::Error for NotificationManagerError {}

// MARK: - NotificationManager

// Default threshold of the hellthread pubkey tag count setting if it is not set.
const DEFAULT_HELLTHREAD_MAX_PUBKEYS: i8 = 10;
// Minimum threshold the hellthread pubkey tag count setting can go down to.
const HELLTHREAD_MIN_PUBKEYS: i8 = 6;
// Maximum threshold the hellthread pubkey tag count setting can go up to.
const HELLTHREAD_MAX_PUBKEYS: i8 = 24;

pub struct NotificationManager {
    db: Arc<Mutex<r2d2::Pool<SqliteConnectionManager>>>,
    apns_topic: String,
    apns_client: Mutex<Client>,
    /// ntfy client for Android push notifications
    ntfy_client: NtfyClient,
    nostr_network_helper: NostrNetworkHelper,
    pub event_saver: EventSaver,
    /// Server keys for NIP-44 encryption (None if encryption disabled)
    server_keys: Option<crate::server_keys::ServerKeys>,
}

#[derive(Clone)]
pub struct EventSaver {
    db: Arc<Mutex<r2d2::Pool<SqliteConnectionManager>>>,
}

impl EventSaver {
    pub fn new(db: Arc<Mutex<r2d2::Pool<SqliteConnectionManager>>>) -> Self {
        Self { db }
    }

    pub async fn save_if_needed(
        &self,
        event: &nostr::Event,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match event.to_mute_list() {
            Some(mute_list) => {
                match self
                    .get_saved_mute_list_for(event.author())
                    .await
                    .ok()
                    .flatten()
                {
                    Some(saved_timestamped_mute_list) => {
                        let saved_mute_list_timestamp = saved_timestamped_mute_list.timestamp;
                        if saved_mute_list_timestamp < event.created_at() {
                            self.save_mute_list(event.author(), mute_list, event.created_at)
                                .await?;
                        } else {
                            return Ok(false);
                        }
                    }
                    None => {
                        self.save_mute_list(event.author(), mute_list, event.created_at)
                            .await?;
                    }
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // MARK: - Muting preferences

    pub async fn save_mute_list(
        &self,
        pubkey: PublicKey,
        mute_list: MuteList,
        created_at: Timestamp,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mute_list_json = mute_list.to_json()?;
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;

        connection.execute(
            "INSERT OR REPLACE INTO muting_preferences (user_pubkey, mute_list, created_at) VALUES (?, ?, ?)",
            params![
                pubkey.to_sql_string(),
                mute_list_json,
                created_at.to_sql_string()
            ],
        )?;

        log::debug!("Mute list saved for pubkey {}", pubkey.to_hex());
        log::debug!("Mute list: {:?}", mute_list);

        Ok(())
    }

    pub async fn get_saved_mute_list_for(
        &self,
        pubkey: PublicKey,
    ) -> Result<Option<TimestampedMuteList>, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;

        let mut stmt = connection.prepare(
            "SELECT mute_list, created_at FROM muting_preferences WHERE user_pubkey = ?",
        )?;

        let mute_list_info: (serde_json::Value, nostr::Timestamp) = match stmt
            .query_row([pubkey.to_sql_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            }) {
            Ok(info) => (info.0, nostr::Timestamp::from_sql_string(info.1)?),
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        let mute_list = MuteList::from_json(mute_list_info.0)?;
        let timestamped_mute_list = TimestampedMuteList {
            mute_list,
            timestamp: mute_list_info.1,
        };

        Ok(Some(timestamped_mute_list))
    }
}

impl NotificationManager {
    // MARK: - Initialization

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        db: r2d2::Pool<SqliteConnectionManager>,
        relay_url: String,
        apns_private_key_path: String,
        apns_private_key_id: String,
        apns_team_id: String,
        apns_environment: a2::client::Endpoint,
        apns_topic: String,
        cache_max_age: std::time::Duration,
        server_keys: Option<crate::server_keys::ServerKeys>,
        ntfy_server_url: String,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let connection = db.get()?;
        Self::setup_database(&connection)?;

        let mut file = File::open(&apns_private_key_path)?;

        let client = Client::token(
            &mut file,
            &apns_private_key_id,
            &apns_team_id,
            ClientConfig::new(apns_environment.clone()),
        )?;

        // Create ntfy client for Android push notifications
        let ntfy_client = NtfyClient::new(ntfy_server_url);
        log::info!("ntfy client initialized for Android push notifications");

        let db = Arc::new(Mutex::new(db));
        let event_saver = EventSaver::new(db.clone());

        let manager = NotificationManager {
            db,
            apns_topic,
            apns_client: Mutex::new(client),
            ntfy_client,
            nostr_network_helper: NostrNetworkHelper::new(
                relay_url.clone(),
                cache_max_age,
                event_saver.clone(),
            )
            .await?,
            event_saver,
            server_keys,
        };

        Ok(manager)
    }

    // MARK: - Database setup operations

    pub fn setup_database(db: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
        // Initial schema setup

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

        Self::add_column_if_not_exists(db, "notifications", "sent_at", "INTEGER", None)?;
        Self::add_column_if_not_exists(db, "user_info", "added_at", "INTEGER", None)?;

        // Notification settings migration (https://github.com/damus-io/damus/issues/2360)

        Self::add_column_if_not_exists(
            db,
            "user_info",
            "zap_notifications_enabled",
            "BOOLEAN",
            Some("true"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "mention_notifications_enabled",
            "BOOLEAN",
            Some("true"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "repost_notifications_enabled",
            "BOOLEAN",
            Some("true"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "reaction_notifications_enabled",
            "BOOLEAN",
            Some("true"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "dm_notifications_enabled",
            "BOOLEAN",
            Some("true"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "only_notifications_from_following_enabled",
            "BOOLEAN",
            Some("false"),
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "hellthread_notifications_disabled",
            "BOOLEAN",
            None,
        )?;
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "hellthread_notifications_max_pubkeys",
            "TINYINT",
            None,
        )?;

        // NIP-44 E2E encryption support
        // Device pubkey is the client's public key for encrypting notifications
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "device_pubkey",
            "TEXT",
            None,
        )?;

        // Platform support for routing to APNs (iOS) or ntfy (Android)
        // Default to "ios" for backwards compatibility with existing registrations
        Self::add_column_if_not_exists(
            db,
            "user_info",
            "platform",
            "TEXT",
            Some("'ios'"),
        )?;

        // Migration related to mute list improvements (https://github.com/damus-io/damus/issues/2118)

        db.execute(
            "CREATE TABLE IF NOT EXISTS muting_preferences (
                user_pubkey TEXT PRIMARY KEY,
                mute_list JSON NOT NULL,
                created_at TEXT NOT NULL
            )",
            [],
        )?;

        Ok(())
    }

    fn add_column_if_not_exists(
        db: &rusqlite::Connection,
        table_name: &str,
        column_name: &str,
        column_type: &str,
        default_value: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let query = format!("PRAGMA table_info({})", table_name);
        let mut stmt = db.prepare(&query)?;
        let column_names: Vec<String> = stmt
            .query_map([], |row| row.get(1))?
            .filter_map(|r| r.ok())
            .collect();

        if !column_names.contains(&column_name.to_string()) {
            let query = format!(
                "ALTER TABLE {} ADD COLUMN {} {} {}",
                table_name,
                column_name,
                column_type,
                match default_value {
                    Some(value) => format!("DEFAULT {}", value),
                    None => "".to_string(),
                },
            );
            db.execute(&query, [])?;
        }
        Ok(())
    }

    // MARK: - Business logic

    pub async fn send_notifications_if_needed(
        &self,
        event: &Event,
    ) -> Result<(), Box<dyn std::error::Error>> {
        log::debug!(
            "Checking if notifications need to be sent for event: {}",
            event.id
        );
        let one_week_ago = nostr::Timestamp::now() - 7 * 24 * 60 * 60;
        if event.created_at < one_week_ago {
            log::debug!("Event is older than a week, not sending notifications");
            return Ok(());
        }

        // Allow notes that are created no more than 3 seconds in the future
        // to account for natural clock skew between sender and receiver.
        if event.created_at > Timestamp::now() + 3 {
            log::debug!("Event was scheduled for the future, not sending notifications");
            return Ok(());
        }

        if !Self::is_event_kind_supported(event.kind) {
            log::debug!("Event kind is not supported, not sending notifications");
            return Ok(());
        }

        let pubkeys_to_notify = self.pubkeys_to_notify_for_event(event).await?;

        log::debug!(
            "Sending notifications to {} pubkeys",
            pubkeys_to_notify.len()
        );

        for pubkey in pubkeys_to_notify {
            self.send_event_notifications_to_pubkey(event, &pubkey)
                .await?;
            {
                let db_mutex_guard = self.db.lock().await;
                db_mutex_guard.get()?.execute(
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
        }
        Ok(())
    }

    fn is_event_kind_supported(event_kind: nostr::Kind) -> bool {
        match event_kind {
            nostr_sdk::Kind::TextNote => true,
            nostr_sdk::Kind::EncryptedDirectMessage => true,
            nostr_sdk::Kind::Repost => true,
            nostr_sdk::Kind::GenericRepost => true,
            nostr_sdk::Kind::Reaction => true,
            nostr_sdk::Kind::ZapPrivateMessage => true,
            nostr_sdk::Kind::ZapRequest => false,
            nostr_sdk::Kind::ZapReceipt => true,
            _ => false,
        }
    }

    async fn pubkeys_to_notify_for_event(
        &self,
        event: &Event,
    ) -> Result<HashSet<nostr::PublicKey>, Box<dyn std::error::Error>> {
        let notification_status = self.get_notification_status(event).await?;
        let relevant_pubkeys = self.pubkeys_relevant_to_event(event);
        let mut relevant_pubkeys_that_are_registered = HashSet::new();
        for pubkey in relevant_pubkeys {
            if self.is_pubkey_registered(&pubkey).await? {
                relevant_pubkeys_that_are_registered.insert(pubkey);
            }
        }
        let pubkeys_that_received_notification =
            notification_status.pubkeys_that_received_notification();
        let relevant_pubkeys_yet_to_receive: HashSet<PublicKey> =
            relevant_pubkeys_that_are_registered
                .difference(&pubkeys_that_received_notification)
                .filter(|&x| *x != event.pubkey)
                .cloned()
                .collect();

        let mut pubkeys_to_notify = HashSet::new();
        for pubkey in relevant_pubkeys_yet_to_receive {
            let should_mute: bool = {
                self.should_mute_notification_for_pubkey(event, &pubkey)
                    .await
            };
            if !should_mute {
                pubkeys_to_notify.insert(pubkey);
            }
        }
        Ok(pubkeys_to_notify)
    }

    async fn should_mute_notification_for_pubkey(&self, event: &Event, pubkey: &PublicKey) -> bool {
        let latest_mute_list = self
            .get_newest_mute_list_available(pubkey)
            .await
            .ok()
            .flatten();
        if let Some(latest_mute_list) = latest_mute_list {
            return should_mute_notification_for_mutelist(event, &latest_mute_list);
        }
        false
    }

    async fn get_newest_mute_list_available(
        &self,
        pubkey: &PublicKey,
    ) -> Result<Option<MuteList>, Box<dyn std::error::Error>> {
        let timestamped_saved_mute_list = self.event_saver.get_saved_mute_list_for(*pubkey).await?;
        let timestamped_network_mute_list =
            self.nostr_network_helper.get_public_mute_list(pubkey).await;
        Ok(
            match (timestamped_saved_mute_list, timestamped_network_mute_list) {
                (Some(local_mute), Some(network_mute)) => {
                    if local_mute.timestamp > network_mute.timestamp {
                        log::debug!("Mute lists available in both database and from the network for pubkey {}. Using local mute list since it's newer.", pubkey.to_hex());
                        Some(local_mute.mute_list)
                    } else {
                        log::debug!("Mute lists available in both database and from the network for pubkey {}. Using network mute list since it's newer.", pubkey.to_hex());
                        Some(network_mute.mute_list)
                    }
                }
                (Some(local_mute), None) => {
                    log::debug!("Mute list available in database for pubkey {}, but not from the network. Using local mute list.", pubkey.to_hex());
                    Some(local_mute.mute_list)
                }
                (None, Some(network_mute)) => {
                    log::debug!("Mute list for pubkey {} available from the network, but not in the database. Using network mute list.", pubkey.to_hex());
                    Some(network_mute.mute_list)
                }
                (None, None) => {
                    log::debug!("No mute list available for pubkey {}", pubkey.to_hex());
                    None
                }
            },
        )
    }

    fn pubkeys_relevant_to_event(&self, event: &Event) -> HashSet<PublicKey> {
        event.relevant_pubkeys()
    }

    fn pubkeys_referenced_by_event(&self, event: &Event) -> HashSet<PublicKey> {
        event.referenced_pubkeys()
    }

    fn is_hellthread_eligible(&self, event_kind: Kind) -> bool {
        match event_kind {
            Kind::TextNote => true,
            Kind::EncryptedDirectMessage => false,
            Kind::Repost => true,
            Kind::GenericRepost => true,
            Kind::Reaction => true,
            Kind::ZapPrivateMessage => false,
            Kind::ZapRequest => false,
            Kind::ZapReceipt => true,
            _ => false,
        }
    }

    async fn send_event_notifications_to_pubkey(
        &self,
        event: &Event,
        pubkey: &PublicKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let user_device_tokens = self.get_user_device_tokens(pubkey).await?;
        for device_token in user_device_tokens {
            if !self
                .user_wants_notification(pubkey, device_token.clone(), event)
                .await?
            {
                continue;
            }
            self.send_event_notification_to_device_token(event, pubkey, &device_token)
                .await?;
        }
        Ok(())
    }

    async fn user_wants_notification(
        &self,
        pubkey: &PublicKey,
        device_token: String,
        event: &Event,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let notification_preferences = self
            .get_user_notification_settings(pubkey, device_token)
            .await?;
        if notification_preferences.only_notifications_from_following_enabled
            && !self
                .nostr_network_helper
                .does_pubkey_follow_pubkey(pubkey, &event.author())
                .await
        {
            return Ok(false);
        }
        if notification_preferences.hellthread_notifications_disabled
            && self.is_hellthread_eligible(event.kind())
        {
            if let Ok(pubkeys_count) = i8::try_from(self.pubkeys_referenced_by_event(event).len()) {
                if pubkeys_count > notification_preferences.hellthread_notifications_max_pubkeys {
                    return Ok(false);
                }
            }
        }
        match event.kind {
            Kind::TextNote => Ok(notification_preferences.mention_notifications_enabled), // TODO: Not 100% accurate
            Kind::EncryptedDirectMessage => Ok(notification_preferences.dm_notifications_enabled),
            Kind::Repost => Ok(notification_preferences.repost_notifications_enabled),
            Kind::GenericRepost => Ok(notification_preferences.repost_notifications_enabled),
            Kind::Reaction => Ok(notification_preferences.reaction_notifications_enabled),
            Kind::ZapPrivateMessage => Ok(notification_preferences.zap_notifications_enabled),
            Kind::ZapRequest => Ok(notification_preferences.zap_notifications_enabled),
            Kind::ZapReceipt => Ok(notification_preferences.zap_notifications_enabled),
            _ => Ok(false),
        }
    }

    async fn is_pubkey_token_pair_registered(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let current_device_tokens = self.get_user_device_tokens(pubkey).await?;
        Ok(current_device_tokens.contains(&device_token.to_string()))
    }

    async fn is_pubkey_registered(
        &self,
        pubkey: &PublicKey,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(!self.get_user_device_tokens(pubkey).await?.is_empty())
    }

    async fn get_user_device_tokens(
        &self,
        pubkey: &PublicKey,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare("SELECT device_token FROM user_info WHERE pubkey = ?")?;
        let device_tokens = stmt
            .query_map([pubkey.to_sql_string()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(device_tokens)
    }

    async fn get_notification_status(
        &self,
        event: &Event,
    ) -> Result<NotificationStatus, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT pubkey, received_notification FROM notifications WHERE event_id = ?",
        )?;
        let rows: std::collections::HashMap<PublicKey, bool> = stmt
            .query_map([event.id.to_sql_string()], |row| {
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

    async fn send_event_notification_to_device_token(
        &self,
        event: &Event,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let platform = self.get_device_platform(pubkey, device_token).await?;

        match platform {
            Platform::Ios => self.send_apns_notification(event, pubkey, device_token).await,
            Platform::Android => self.send_ntfy_notification(event, pubkey, device_token).await,
        }
    }

    /// Send notification via APNs (iOS/macOS)
    async fn send_apns_notification(
        &self,
        event: &Event,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (title, subtitle, body) = self.format_notification_message(event);

        log::debug!("Sending APNs notification to device token: {}", device_token);

        let mut payload = DefaultNotificationBuilder::new()
            .set_title(&title)
            .set_subtitle(&subtitle)
            .set_body(&body)
            .set_mutable_content()
            .set_content_available()
            .build(device_token, Default::default());

        payload.options.apns_topic = Some(self.apns_topic.as_str());

        // NIP-44 E2E Encryption: encrypt to device pubkey if registered
        // This ensures Apple only sees ciphertext, not notification content
        let event_json = event.try_as_json()?;
        let encrypted = self.maybe_encrypt_payload(pubkey, device_token, &event_json).await;

        match encrypted {
            Some(ciphertext) => {
                // Encrypted payload: client decrypts using device privkey
                payload.data.insert("encrypted", serde_json::Value::Bool(true));
                payload.data.insert(
                    "ciphertext",
                    serde_json::Value::String(ciphertext),
                );
                log::debug!(
                    "nip44_notification encrypted=true platform=ios pubkey={}...",
                    &pubkey.to_hex()[..8]
                );
            }
            None => {
                // Plaintext payload: legacy mode for devices without encryption
                payload.data.insert("encrypted", serde_json::Value::Bool(false));
                payload.data.insert(
                    "nostr_event",
                    serde_json::Value::String(event_json),
                );
                log::debug!(
                    "nip44_notification encrypted=false platform=ios pubkey={}",
                    pubkey.to_hex()
                );
            }
        }

        let apns_client_mutex_guard = self.apns_client.lock().await;

        match apns_client_mutex_guard.send(payload).await {
            Ok(_response) => {}
            Err(e) => log::error!(
                "Failed to send APNs notification to device: {}",
                e
            ),
        }

        log::debug!("APNs notification sent");
        Ok(())
    }

    /// Send notification via ntfy (Android)
    async fn send_ntfy_notification(
        &self,
        event: &Event,
        pubkey: &PublicKey,
        device_token: &str,  // For Android, this is the ntfy topic
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (title, _subtitle, body) = self.format_notification_message(event);

        log::debug!("Sending ntfy notification to topic: {}", device_token);

        // NIP-44 E2E Encryption: encrypt to device pubkey if registered
        let event_json = event.try_as_json()?;
        let encrypted = self.maybe_encrypt_payload(pubkey, device_token, &event_json).await;

        // Build JSON payload for ntfy
        let mut data_payload = serde_json::Map::new();

        match encrypted {
            Some(ciphertext) => {
                data_payload.insert("encrypted".to_string(), serde_json::Value::Bool(true));
                data_payload.insert("ciphertext".to_string(), serde_json::Value::String(ciphertext));
                log::debug!(
                    "nip44_notification encrypted=true platform=android pubkey={}...",
                    &pubkey.to_hex()[..8]
                );
            }
            None => {
                data_payload.insert("encrypted".to_string(), serde_json::Value::Bool(false));
                data_payload.insert("nostr_event".to_string(), serde_json::Value::String(event_json));
                log::debug!(
                    "nip44_notification encrypted=false platform=android pubkey={}",
                    pubkey.to_hex()
                );
            }
        }

        // ntfy JSON payload format
        let ntfy_payload = serde_json::json!({
            "topic": device_token,
            "title": title,
            "message": body,
            "priority": 4,  // High priority
            "data": data_payload
        });

        match self.ntfy_client.send_json(device_token, &ntfy_payload).await {
            Ok(_response) => {
                log::debug!("ntfy notification sent");
            }
            Err(e) => {
                log::error!("Failed to send ntfy notification: {}", e);
            }
        }

        Ok(())
    }

    /// Encrypt notification payload if device has registered a pubkey
    ///
    /// Returns Some(ciphertext) if encryption is enabled and device has a pubkey,
    /// None otherwise (fallback to plaintext).
    async fn maybe_encrypt_payload(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
        event_json: &str,
    ) -> Option<String> {
        // Early return if server doesn't have encryption keys
        let server_keys = self.server_keys.as_ref()?;

        // Check if device has registered an encryption pubkey
        let device_pubkey = match self.get_device_pubkey(pubkey, device_token).await {
            Ok(Some(pk)) => pk,
            Ok(None) => {
                // Device hasn't registered for encryption - this is normal, not an error
                return None;
            }
            Err(e) => {
                // Metrics: Track database lookup errors
                log::warn!(
                    "nip44_db_error error=\"{}\" device={} pubkey={}",
                    e,
                    device_token,
                    pubkey.to_hex()
                );
                return None;
            }
        };

        // Encrypt the event JSON using NIP-44 (rust-nostr implementation)
        match nostr::nips::nip44::encrypt(
            server_keys.secret_key(),
            &device_pubkey,
            event_json,
            nostr::nips::nip44::Version::V2,
        ) {
            Ok(ciphertext) => Some(ciphertext),
            Err(e) => {
                // Metrics: Track encryption errors
                log::error!(
                    "nip44_encryption_error error=\"{}\" device={} pubkey={}",
                    e,
                    device_token,
                    pubkey.to_hex()
                );
                None
            }
        }
    }

    fn format_notification_message(&self, event: &Event) -> (String, String, String) {
        // NOTE: This is simple because the client will handle formatting. These are just fallbacks.
        let (title, body) = match event.kind {
            nostr_sdk::Kind::TextNote => ("New activity".to_string(), event.content.clone()),
            nostr_sdk::Kind::EncryptedDirectMessage => (
                "New direct message".to_string(),
                "Contents are encrypted".to_string(),
            ),
            nostr_sdk::Kind::Repost => ("Someone reposted".to_string(), event.content.clone()),
            nostr_sdk::Kind::Reaction => {
                let content_text = event.content.clone();
                let formatted_text = match content_text.as_str() {
                    "" => "❤️",
                    "+" => "❤️",
                    "-" => "👎",
                    _ => content_text.as_str(),
                };
                ("New reaction".to_string(), formatted_text.to_string())
            }
            nostr_sdk::Kind::ZapPrivateMessage => (
                "New zap private message".to_string(),
                "Contents are encrypted".to_string(),
            ),
            nostr_sdk::Kind::ZapReceipt => ("Someone zapped you".to_string(), "".to_string()),
            _ => ("New activity".to_string(), "".to_string()),
        };
        (title, "".to_string(), body)
    }

    // MARK: - User device info and settings

    pub async fn save_user_device_info_if_not_present(
        &self,
        pubkey: nostr::PublicKey,
        device_token: &str,
        platform: Platform,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self
            .is_pubkey_token_pair_registered(&pubkey, device_token)
            .await?
        {
            return Ok(());
        }
        self.save_user_device_info(pubkey, device_token, platform).await
    }

    pub async fn save_user_device_info(
        &self,
        pubkey: nostr::PublicKey,
        device_token: &str,
        platform: Platform,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let current_time_unix = Timestamp::now();
        let db_mutex_guard = self.db.lock().await;
        // Use UPSERT to preserve existing columns on re-registration.
        // INSERT OR REPLACE would drop device_pubkey, notification settings, etc.
        // ON CONFLICT updates added_at and platform; other columns remain unchanged.
        db_mutex_guard.get()?.execute(
            "INSERT INTO user_info (id, pubkey, device_token, added_at, platform) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET added_at = excluded.added_at, platform = excluded.platform",
            params![
                format!("{}:{}", pubkey.to_sql_string(), device_token),
                pubkey.to_sql_string(),
                device_token,
                current_time_unix.to_sql_string(),
                platform.as_str()
            ],
        )?;
        log::debug!(
            "Registered device for pubkey {}... platform={}",
            &pubkey.to_hex()[..8],
            platform.as_str()
        );
        Ok(())
    }

    /// Get the platform for a device token
    async fn get_device_platform(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> Result<Platform, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT platform FROM user_info WHERE pubkey = ? AND device_token = ?",
        )?;

        let platform_str: Option<String> = stmt
            .query_row([pubkey.to_sql_string(), device_token.to_string()], |row| {
                row.get(0)
            })
            .ok();

        Ok(platform_str
            .map(|s| Platform::from_str(&s))
            .unwrap_or(Platform::Ios))
    }

    pub async fn remove_user_device_info(
        &self,
        pubkey: nostr::PublicKey,
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "DELETE FROM user_info WHERE pubkey = ? AND device_token = ?",
            params![pubkey.to_sql_string(), device_token],
        )?;
        Ok(())
    }

    /// Save a device pubkey for NIP-44 encrypted notifications
    ///
    /// When a device pubkey is registered, all notifications to this user/device
    /// will be encrypted using NIP-44 before being sent via APNs. The client
    /// decrypts using the corresponding private key.
    pub async fn save_device_pubkey(
        &self,
        pubkey: &nostr::PublicKey,
        device_token: &str,
        device_pubkey: &nostr::PublicKey,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let rows_updated = db_mutex_guard.get()?.execute(
            "UPDATE user_info SET device_pubkey = ? WHERE pubkey = ? AND device_token = ?",
            params![
                device_pubkey.to_hex(),
                pubkey.to_sql_string(),
                device_token
            ],
        )?;

        // If no rows were updated, the user/device pair doesn't exist yet
        if rows_updated == 0 {
            return Err(NotificationManagerError::DeviceNotRegistered.into());
        }

        // Debug level: avoid logging full pubkeys/device tokens in production
        log::debug!(
            "Registered device pubkey for user {}...",
            &pubkey.to_hex()[..8]
        );

        Ok(())
    }

    /// Get the device pubkey for a user/device pair, if one is registered
    ///
    /// Returns None if the user hasn't registered a device pubkey for encryption,
    /// in which case notifications should be sent in plaintext (legacy mode).
    pub async fn get_device_pubkey(
        &self,
        pubkey: &nostr::PublicKey,
        device_token: &str,
    ) -> Result<Option<nostr::PublicKey>, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT device_pubkey FROM user_info WHERE pubkey = ? AND device_token = ?",
        )?;

        let result: Option<Option<String>> = stmt
            .query_row([pubkey.to_sql_string(), device_token.to_string()], |row| {
                row.get(0)
            })
            .ok();

        // Flatten Option<Option<String>> and parse the hex pubkey
        match result.flatten() {
            Some(hex) => {
                let pk = nostr::PublicKey::from_hex(&hex)?;
                Ok(Some(pk))
            }
            None => Ok(None),
        }
    }

    pub async fn get_user_notification_settings(
        &self,
        pubkey: &PublicKey,
        device_token: String,
    ) -> Result<UserNotificationSettings, Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT zap_notifications_enabled, mention_notifications_enabled, repost_notifications_enabled, reaction_notifications_enabled, dm_notifications_enabled, only_notifications_from_following_enabled, hellthread_notifications_disabled, hellthread_notifications_max_pubkeys FROM user_info WHERE pubkey = ? AND device_token = ?",
        )?;
        let settings = stmt.query_row([pubkey.to_sql_string(), device_token], |row| {
            Ok(UserNotificationSettings {
                zap_notifications_enabled: row.get(0)?,
                mention_notifications_enabled: row.get(1)?,
                repost_notifications_enabled: row.get(2)?,
                reaction_notifications_enabled: row.get(3)?,
                dm_notifications_enabled: row.get(4)?,
                only_notifications_from_following_enabled: row.get(5)?,
                hellthread_notifications_disabled: row.get::<_, Option<bool>>(6)?.unwrap_or(false),
                hellthread_notifications_max_pubkeys: row.get::<_, Option<i8>>(7)?.unwrap_or(DEFAULT_HELLTHREAD_MAX_PUBKEYS),
            })
        })?;

        Ok(settings)
    }

    pub async fn save_user_notification_settings(
        &self,
        pubkey: &PublicKey,
        device_token: String,
        settings: UserNotificationSettings,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        connection.execute(
            "UPDATE user_info SET zap_notifications_enabled = ?, mention_notifications_enabled = ?, repost_notifications_enabled = ?, reaction_notifications_enabled = ?, dm_notifications_enabled = ?, only_notifications_from_following_enabled = ?, hellthread_notifications_disabled = ?, hellthread_notifications_max_pubkeys = ? WHERE pubkey = ? AND device_token = ?",
            params![
                settings.zap_notifications_enabled,
                settings.mention_notifications_enabled,
                settings.repost_notifications_enabled,
                settings.reaction_notifications_enabled,
                settings.dm_notifications_enabled,
                settings.only_notifications_from_following_enabled,
                settings.hellthread_notifications_disabled,
                max(HELLTHREAD_MIN_PUBKEYS, min(HELLTHREAD_MAX_PUBKEYS, settings.hellthread_notifications_max_pubkeys)),
                pubkey.to_sql_string(),
                device_token,
            ],
        )?;
        Ok(())
    }
}

fn default_hellthread_max_pubkeys() -> i8 {
    DEFAULT_HELLTHREAD_MAX_PUBKEYS
}


#[derive(Serialize, Deserialize, Debug)]
pub struct UserNotificationSettings {
    zap_notifications_enabled: bool,
    mention_notifications_enabled: bool,
    repost_notifications_enabled: bool,
    reaction_notifications_enabled: bool,
    dm_notifications_enabled: bool,
    only_notifications_from_following_enabled: bool,

    #[serde(default)]
    hellthread_notifications_disabled: bool,

    #[serde(default = "default_hellthread_max_pubkeys")]
    hellthread_notifications_max_pubkeys: i8,
}

struct NotificationStatus {
    status_info: std::collections::HashMap<PublicKey, bool>,
}

impl NotificationStatus {
    fn pubkeys_that_received_notification(&self) -> HashSet<PublicKey> {
        self.status_info
            .iter()
            .filter(|&(_, &received_notification)| received_notification)
            .map(|(pubkey, _)| *pubkey)
            .collect()
    }
}

// MARK: - Integration Tests

#[cfg(test)]
mod nip44_integration_tests {
    use super::*;
    use crate::server_keys::ServerKeys;
    use nostr::Keys;
    use r2d2_sqlite::SqliteConnectionManager;

    /// Test harness for NIP-44 integration tests
    ///
    /// Provides an in-memory database and server keys for testing
    /// the encryption flow without requiring APNs.
    struct TestHarness {
        db: Arc<Mutex<r2d2::Pool<SqliteConnectionManager>>>,
        server_keys: ServerKeys,
    }

    impl TestHarness {
        fn new() -> Self {
            // Create in-memory SQLite database
            let manager = SqliteConnectionManager::memory();
            let pool = r2d2::Pool::new(manager).expect("Failed to create pool");

            // Set up database schema
            {
                let conn = pool.get().expect("Failed to get connection");
                NotificationManager::setup_database(&conn).expect("Failed to setup database");
            }

            let server_keys = ServerKeys::generate();

            TestHarness {
                db: Arc::new(Mutex::new(pool)),
                server_keys,
            }
        }

        /// Save user device info (registration)
        async fn save_user_device_info(
            &self,
            pubkey: &nostr::PublicKey,
            device_token: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let current_time_unix = Timestamp::now();
            let db_mutex_guard = self.db.lock().await;
            db_mutex_guard.get()?.execute(
                "INSERT INTO user_info (id, pubkey, device_token, added_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET added_at = excluded.added_at",
                params![
                    format!("{}:{}", pubkey.to_hex(), device_token),
                    pubkey.to_hex(),
                    device_token,
                    current_time_unix.as_u64().to_string()
                ],
            )?;
            Ok(())
        }

        /// Save device pubkey for encryption
        async fn save_device_pubkey(
            &self,
            pubkey: &nostr::PublicKey,
            device_token: &str,
            device_pubkey: &nostr::PublicKey,
        ) -> Result<(), NotificationManagerError> {
            let db_mutex_guard = self.db.lock().await;
            let rows_updated = db_mutex_guard
                .get()
                .map_err(|_| NotificationManagerError::DeviceNotRegistered)?
                .execute(
                    "UPDATE user_info SET device_pubkey = ? WHERE pubkey = ? AND device_token = ?",
                    params![device_pubkey.to_hex(), pubkey.to_hex(), device_token],
                )
                .map_err(|_| NotificationManagerError::DeviceNotRegistered)?;

            if rows_updated == 0 {
                return Err(NotificationManagerError::DeviceNotRegistered);
            }
            Ok(())
        }

        /// Get device pubkey if registered
        async fn get_device_pubkey(
            &self,
            pubkey: &nostr::PublicKey,
            device_token: &str,
        ) -> Result<Option<nostr::PublicKey>, Box<dyn std::error::Error>> {
            let db_mutex_guard = self.db.lock().await;
            let connection = db_mutex_guard.get()?;
            let mut stmt = connection.prepare(
                "SELECT device_pubkey FROM user_info WHERE pubkey = ? AND device_token = ?",
            )?;

            let result: Option<Option<String>> = stmt
                .query_row([pubkey.to_hex(), device_token.to_string()], |row| row.get(0))
                .ok();

            match result.flatten() {
                Some(hex) => {
                    let pk = nostr::PublicKey::from_hex(&hex)?;
                    Ok(Some(pk))
                }
                None => Ok(None),
            }
        }

        /// Encrypt payload if device has registered pubkey (mirrors maybe_encrypt_payload)
        async fn maybe_encrypt_payload(
            &self,
            pubkey: &nostr::PublicKey,
            device_token: &str,
            event_json: &str,
        ) -> Option<String> {
            let device_pubkey = self.get_device_pubkey(pubkey, device_token).await.ok()??;

            nostr::nips::nip44::encrypt(
                self.server_keys.secret_key(),
                &device_pubkey,
                event_json,
                nostr::nips::nip44::Version::V2,
            )
            .ok()
        }
    }

    // =========================================================================
    // Test 1: Encrypted notification flow
    // =========================================================================

    #[tokio::test]
    async fn test_encrypted_notification_flow() {
        let harness = TestHarness::new();

        // Generate test keys
        let user_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let device_token = "test_device_token_abc123";

        // Client generates a device keypair for receiving encrypted notifications
        let device_keys = Keys::generate();
        let device_pubkey = device_keys.public_key();

        // Step 1: Register device
        harness
            .save_user_device_info(&user_pubkey, device_token)
            .await
            .expect("Device registration should succeed");

        // Step 2: Register device encryption pubkey
        harness
            .save_device_pubkey(&user_pubkey, device_token, &device_pubkey)
            .await
            .expect("Device pubkey registration should succeed");

        // Step 3: Verify device pubkey was stored
        let stored_pubkey = harness
            .get_device_pubkey(&user_pubkey, device_token)
            .await
            .expect("Get device pubkey should succeed");
        assert_eq!(stored_pubkey, Some(device_pubkey));

        // Step 4: Simulate notification - server encrypts payload
        let event_json = r#"{"id":"abc123","pubkey":"def456","content":"Hello!"}"#;
        let encrypted = harness
            .maybe_encrypt_payload(&user_pubkey, device_token, event_json)
            .await;

        // Should return encrypted ciphertext
        assert!(encrypted.is_some(), "Should encrypt when device has pubkey");
        let ciphertext = encrypted.unwrap();
        assert!(!ciphertext.is_empty());
        assert_ne!(ciphertext, event_json, "Ciphertext should differ from plaintext");

        // Step 5: Client decrypts the notification
        let decrypted = nostr::nips::nip44::decrypt(
            device_keys.secret_key().expect("device has secret key"),
            &harness.server_keys.public_key(),
            &ciphertext,
        )
        .expect("Client should be able to decrypt");

        assert_eq!(decrypted, event_json, "Decrypted content should match original");

        println!("✓ Test 1 PASSED: Encrypted notification flow works end-to-end");
    }

    // =========================================================================
    // Test 2: Plaintext fallback (no device pubkey registered)
    // =========================================================================

    #[tokio::test]
    async fn test_plaintext_fallback_flow() {
        let harness = TestHarness::new();

        // Generate test keys
        let user_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let device_token = "test_device_token_xyz789";

        // Step 1: Register device (but DON'T register encryption pubkey)
        harness
            .save_user_device_info(&user_pubkey, device_token)
            .await
            .expect("Device registration should succeed");

        // Step 2: Verify no device pubkey is stored
        let stored_pubkey = harness
            .get_device_pubkey(&user_pubkey, device_token)
            .await
            .expect("Get device pubkey should succeed");
        assert_eq!(stored_pubkey, None, "No device pubkey should be registered");

        // Step 3: Simulate notification - should return None (plaintext mode)
        let event_json = r#"{"id":"abc123","content":"Hello world"}"#;
        let encrypted = harness
            .maybe_encrypt_payload(&user_pubkey, device_token, event_json)
            .await;

        // Should return None, indicating plaintext fallback
        assert!(
            encrypted.is_none(),
            "Should return None when device has no pubkey (plaintext fallback)"
        );

        println!("✓ Test 2 PASSED: Plaintext fallback works for devices without pubkey");
    }

    // =========================================================================
    // Test 3: UPSERT preserves device_pubkey on re-registration
    // =========================================================================

    #[tokio::test]
    async fn test_upsert_preserves_device_pubkey() {
        let harness = TestHarness::new();

        let user_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let device_token = "test_device_reregister";
        let device_keys = Keys::generate();
        let device_pubkey = device_keys.public_key();

        // Step 1: Register device and set encryption pubkey
        harness
            .save_user_device_info(&user_pubkey, device_token)
            .await
            .expect("Initial registration should succeed");
        harness
            .save_device_pubkey(&user_pubkey, device_token, &device_pubkey)
            .await
            .expect("Device pubkey registration should succeed");

        // Verify pubkey is stored
        let stored = harness
            .get_device_pubkey(&user_pubkey, device_token)
            .await
            .unwrap();
        assert_eq!(stored, Some(device_pubkey));

        // Step 2: Re-register the same device (simulates app reinstall/token refresh)
        harness
            .save_user_device_info(&user_pubkey, device_token)
            .await
            .expect("Re-registration should succeed");

        // Step 3: Verify device_pubkey was preserved
        let stored_after = harness
            .get_device_pubkey(&user_pubkey, device_token)
            .await
            .unwrap();
        assert_eq!(
            stored_after,
            Some(device_pubkey),
            "Device pubkey should be preserved after re-registration"
        );

        println!("✓ Test 3 PASSED: UPSERT preserves device_pubkey on re-registration");
    }

    // =========================================================================
    // Test 4: Setting encryption key before registration fails
    // =========================================================================

    #[tokio::test]
    async fn test_encryption_key_before_registration_fails() {
        let harness = TestHarness::new();

        let user_keys = Keys::generate();
        let user_pubkey = user_keys.public_key();
        let device_token = "unregistered_device";
        let device_keys = Keys::generate();
        let device_pubkey = device_keys.public_key();

        // Try to set encryption key WITHOUT registering device first
        let result = harness
            .save_device_pubkey(&user_pubkey, device_token, &device_pubkey)
            .await;

        assert!(
            matches!(result, Err(NotificationManagerError::DeviceNotRegistered)),
            "Should fail with DeviceNotRegistered when device not registered"
        );

        println!("✓ Test 4 PASSED: Setting encryption key before registration returns typed error");
    }
}
