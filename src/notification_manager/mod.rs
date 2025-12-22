mod nostr_event_cache;
mod nostr_event_extensions;
pub mod nostr_network_helper;
pub mod utils;

use std::cmp::{max, min};
use nostr_event_extensions::{ExtendedEvent, SqlStringConvertible};
use nostrdb::{Config as NdbConfig, Ndb, Transaction as NdbTransaction};

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
    nostr_network_helper: NostrNetworkHelper,
    pub event_saver: EventSaver,
    /// nostrdb for profile lookups (None if disabled via NDB_PATH="disabled")
    pub ndb: Option<Ndb>,
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
        ndb_path: Option<String>,
        ndb_mapsize_mb: usize,
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

        let db = Arc::new(Mutex::new(db));
        let event_saver = EventSaver::new(db.clone());

        // Initialize nostrdb for profile lookups (if enabled)
        let ndb = match ndb_path {
            Some(ref path) => {
                let mapsize_bytes = ndb_mapsize_mb * 1024 * 1024;
                let ndb_config = NdbConfig::new().set_mapsize(mapsize_bytes);
                match Ndb::new(path, &ndb_config) {
                    Ok(db) => {
                        log::info!("nostrdb initialized at {} (mapsize: {}MB)", path, ndb_mapsize_mb);
                        Some(db)
                    }
                    Err(e) => {
                        log::warn!("Failed to initialize nostrdb at {}: {:?}. Profile lookups disabled.", path, e);
                        None
                    }
                }
            }
            None => {
                log::info!("nostrdb disabled (NDB_PATH not set or set to 'disabled')");
                None
            }
        };

        let manager = NotificationManager {
            db,
            apns_topic,
            apns_client: Mutex::new(client),
            nostr_network_helper: NostrNetworkHelper::new(
                relay_url.clone(),
                cache_max_age,
                event_saver.clone(),
            )
            .await?,
            event_saver,
            ndb,
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
            self.send_event_notification_to_device_token(event, &device_token)
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
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (title, subtitle, body) = self.format_notification_message(event);

        log::debug!("Sending notification to device token: {}", device_token);

        let mut payload = DefaultNotificationBuilder::new()
            .set_title(&title)
            .set_subtitle(&subtitle)
            .set_body(&body)
            .set_mutable_content()
            .set_content_available()
            .build(device_token, Default::default());

        payload.options.apns_topic = Some(self.apns_topic.as_str());
        payload.data.insert(
            "nostr_event",
            serde_json::Value::String(event.try_as_json()?),
        );

        // Add author's profile data if available
        let (name, picture) = self
            .get_author_profile(&event.pubkey)
            .unwrap_or_default();
        if !name.is_empty() {
            payload
                .data
                .insert("name", serde_json::Value::String(name));
        }
        if !picture.is_empty() {
            payload
                .data
                .insert("picture", serde_json::Value::String(picture));
        }

        let apns_client_mutex_guard = self.apns_client.lock().await;

        match apns_client_mutex_guard.send(payload).await {
            Ok(_response) => {}
            Err(e) => log::error!(
                "Failed to send notification to device token '{}': {}",
                device_token,
                e
            ),
        }

        log::info!("Notification sent to device token: {}", device_token);

        Ok(())
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

    /// Gets the author's profile (name, picture) from nostrdb
    fn get_author_profile(&self, pubkey: &PublicKey) -> Option<(String, String)> {
        let ndb = self.ndb.as_ref()?;
        let txn = NdbTransaction::new(ndb).ok()?;
        let pubkey_hex = pubkey.to_hex();
        let pubkey_bytes: [u8; 32] = hex::decode(&pubkey_hex)
            .ok()?
            .try_into()
            .ok()?;
        let profile_record = ndb.get_profile_by_pubkey(&txn, &pubkey_bytes).ok()?;
        let profile = profile_record.record().profile()?;

        let name = profile
            .display_name()
            .or(profile.name())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let picture = profile.picture().map(|s| s.to_string()).unwrap_or_default();

        Some((name, picture))
    }

    // MARK: - User device info and settings

    pub async fn save_user_device_info_if_not_present(
        &self,
        pubkey: nostr::PublicKey,
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self
            .is_pubkey_token_pair_registered(&pubkey, device_token)
            .await?
        {
            return Ok(());
        }
        self.save_user_device_info(pubkey, device_token).await
    }

    pub async fn save_user_device_info(
        &self,
        pubkey: nostr::PublicKey,
        device_token: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let current_time_unix = Timestamp::now();
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "INSERT OR REPLACE INTO user_info (id, pubkey, device_token, added_at) VALUES (?, ?, ?, ?)",
            params![
                format!("{}:{}", pubkey.to_sql_string(), device_token),
                pubkey.to_sql_string(),
                device_token,
                current_time_unix.to_sql_string()
            ],
        )?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use nostrdb::{Config as NdbConfig, Ndb, Transaction};

    /// Test that profile lookup works after ingesting a kind:0 metadata event
    /// NOTE: This test is flaky due to nostrdb async ingestion timing. Run with --ignored to test.
    #[test]
    #[ignore]
    fn test_profile_lookup_from_nostrdb() {
        let db_path = "target/testdbs/profile_lookup_test";

        // Clean up any previous test run
        let _ = std::fs::remove_dir_all(db_path);

        // A real kind:0 profile event (jb55's profile) - note the field order matches nostrdb-rs tests
        let profile_event = r#"["EVENT","nostril-query",{"content":"{\"nip05\":\"_@jb55.com\",\"website\":\"https://damus.io\",\"name\":\"jb55\",\"about\":\"I made damus\",\"lud16\":\"jb55@sendsats.lol\",\"display_name\":\"Will\",\"picture\":\"https://cdn.jb55.com/img/red-me.jpg\"}","created_at":1700855305,"id":"cad04d11f7fa9c36d57400baca198582dfeb94fa138366c4469e58da9ed60051","kind":0,"pubkey":"32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245","sig":"7a15e379ff27318460172b4a1d55a13e064c5007d05d5a188e7f60e244a9ed08996cb7676058b88c7a91ae9488f8edc719bc966cb5bf1eb99be44cdb745f915f","tags":[]}]"#;

        // Ingest the profile event and close db to flush
        {
            let config = NdbConfig::new();
            let ndb = Ndb::new(db_path, &config).expect("Failed to create ndb");
            ndb.process_event(profile_event)
                .expect("Failed to process profile event");
            // Wait for async ingestion before closing
            std::thread::sleep(std::time::Duration::from_millis(150));
        } // ndb dropped here, forces flush

        // Reopen and verify
        let config = NdbConfig::new();
        let ndb = Ndb::new(db_path, &config).expect("Failed to reopen ndb");

        let pubkey_hex = "32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245";
        let pubkey_bytes: [u8; 32] = hex::decode(pubkey_hex)
            .expect("valid hex")
            .try_into()
            .expect("32 bytes");

        let mut txn = Transaction::new(&ndb).expect("Failed to create transaction");
        let profile_record = ndb
            .get_profile_by_pubkey(&mut txn, &pubkey_bytes)
            .expect("Profile should exist after reopen");

        let profile = profile_record
            .record()
            .profile()
            .expect("Profile record should have profile");

        // Verify profile data
        assert_eq!(profile.name(), Some("jb55"));
        assert_eq!(profile.display_name(), Some("Will"));
        assert_eq!(profile.picture(), Some("https://cdn.jb55.com/img/red-me.jpg"));

        // Cleanup
        let _ = std::fs::remove_dir_all(db_path);
    }

    /// Test that profile lookup returns None for unknown pubkey
    #[test]
    fn test_profile_lookup_unknown_pubkey() {
        let db_path = "target/testdbs/unknown_pubkey_test";
        let _ = std::fs::remove_dir_all(db_path);

        let config = NdbConfig::new();
        let ndb = Ndb::new(db_path, &config).expect("Failed to create ndb");

        // Try to look up a pubkey that doesn't exist
        let unknown_pubkey: [u8; 32] = [0u8; 32];

        let txn = Transaction::new(&ndb).expect("Failed to create transaction");
        let result = ndb.get_profile_by_pubkey(&txn, &unknown_pubkey);

        assert!(result.is_err(), "Should not find unknown pubkey");

        let _ = std::fs::remove_dir_all(db_path);
    }

    /// Test that ingesting a note event (kind:1) doesn't create a profile
    #[test]
    fn test_note_event_does_not_create_profile() {
        let db_path = "target/testdbs/note_no_profile_test";
        let _ = std::fs::remove_dir_all(db_path);

        let config = NdbConfig::new();
        let ndb = Ndb::new(db_path, &config).expect("Failed to create ndb");

        // A kind:1 text note (not a profile)
        let note_event = r#"["EVENT","test",{"id":"702555e52e82cc24ad517ba78c21879f6e47a7c0692b9b20df147916ae8731a3","pubkey":"32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15","created_at":1702675561,"kind":1,"tags":[],"content":"hello, world","sig":"2275c5f5417abfd644b7bc74f0388d70feb5d08b6f90fa18655dda5c95d013bfbc5258ea77c05b7e40e0ee51d8a2efa931dc7a0ec1db4c0a94519762c6625675"}]"#;

        ndb.process_event(note_event)
            .expect("Failed to process note event");

        std::thread::sleep(std::time::Duration::from_millis(150));

        // Try to look up profile for the note author
        let pubkey_hex = "32bf915904bfde2d136ba45dde32c88f4aca863783999faea2e847a8fafd2f15";
        let pubkey_bytes: [u8; 32] = hex::decode(pubkey_hex)
            .expect("valid hex")
            .try_into()
            .expect("32 bytes");

        let txn = Transaction::new(&ndb).expect("Failed to create transaction");
        let result = ndb.get_profile_by_pubkey(&txn, &pubkey_bytes);

        // Should not have a profile since we only ingested a note
        assert!(result.is_err(), "Note author should not have profile");

        let _ = std::fs::remove_dir_all(db_path);
    }
}
