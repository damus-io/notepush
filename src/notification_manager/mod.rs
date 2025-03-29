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
