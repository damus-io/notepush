mod nostr_event_cache;
mod nostr_event_extensions;
pub mod nostr_network_helper;
pub mod utils;

use nostr_event_extensions::{ExtendedEvent, SqlStringConvertible};
use std::cmp::{max, min};

use a2::{Client, ClientConfig, DefaultNotificationBuilder, NotificationBuilder};
use nostr_sdk::{Alphabet, Event, JsonUtil, Kind, PublicKey, SingleLetterTag, TagKind, Timestamp};
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

use fcm_service::{FcmMessage, FcmNotification, FcmService, Target, WebpushConfig};
use nostr_event_extensions::Codable;
use nostr_event_extensions::MaybeConvertibleToMuteList;
use nostr_event_extensions::TimestampedMuteList;
use nostr_network_helper::NostrNetworkHelper;
use nostr_sdk::prelude::MuteList;
use r2d2_sqlite::SqliteConnectionManager;
use std::fs::File;
use std::str::FromStr;
use thiserror::Error;
use utils::should_mute_notification_for_mutelist;

// MARK: - NotificationManager

// Default threshold of the hellthread pubkey tag count setting if it is not set.
const DEFAULT_HELLTHREAD_MAX_PUBKEYS: i8 = 10;
// Minimum threshold the hellthread pubkey tag count setting can go down to.
const HELLTHREAD_MIN_PUBKEYS: i8 = 6;
// Maximum threshold the hellthread pubkey tag count setting can go up to.
const HELLTHREAD_MAX_PUBKEYS: i8 = 24;

#[derive(Debug, Clone, Copy)]
pub enum NotificationBackend {
    APNS,
    FCM,
}

impl From<u8> for NotificationBackend {
    fn from(value: u8) -> Self {
        match value {
            0 => NotificationBackend::APNS,
            1 => NotificationBackend::FCM,
            _ => panic!("Invalid value for NotificationBackend"),
        }
    }
}

impl Into<u8> for NotificationBackend {
    fn into(self) -> u8 {
        match self {
            NotificationBackend::APNS => 0,
            NotificationBackend::FCM => 1,
        }
    }
}

impl FromStr for NotificationBackend {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "fcm" => Ok(NotificationBackend::FCM),
            "apns" => Ok(NotificationBackend::APNS),
            _ => Err(()),
        }
    }
}

#[derive(Error, Debug)]
pub enum NotificationManagerError {
    #[error("APNS is not configured")]
    APNSMissing,
    #[error("FCM is not configured")]
    FCMMissing,

    #[error(transparent)]
    Other(BoxSendError),
}

type BoxSendError = Box<dyn std::error::Error + Send + Sync>;
type NResult<T> = Result<T, BoxSendError>;
type DatabaseArc = Arc<Mutex<r2d2::Pool<SqliteConnectionManager>>>;

pub struct NotificationManager {
    db: DatabaseArc,
    apns_topic: Option<String>,
    apns_client: Option<Mutex<Client>>,
    fcm_client: Option<Mutex<FcmService>>,
    nostr_network_helper: NostrNetworkHelper,
    pub event_saver: EventSaver,
}

#[derive(Clone)]
pub struct EventSaver {
    db: DatabaseArc,
}

impl EventSaver {
    pub fn new(db: DatabaseArc) -> Self {
        Self { db }
    }

    pub async fn save_if_needed(&self, event: &Event) -> NResult<bool> {
        match event.to_mute_list() {
            Some(mute_list) => {
                match self
                    .get_saved_mute_list_for(event.pubkey)
                    .await
                    .ok()
                    .flatten()
                {
                    Some(saved_timestamped_mute_list) => {
                        let saved_mute_list_timestamp = saved_timestamped_mute_list.timestamp;
                        if saved_mute_list_timestamp < event.created_at {
                            self.save_mute_list(event.pubkey, mute_list, event.created_at)
                                .await?;
                        } else {
                            return Ok(false);
                        }
                    }
                    None => {
                        self.save_mute_list(event.pubkey, mute_list, event.created_at)
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
    ) -> NResult<()> {
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
    ) -> NResult<Option<TimestampedMuteList>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;

        let mut stmt = connection.prepare(
            "SELECT mute_list, created_at FROM muting_preferences WHERE user_pubkey = ?",
        )?;

        let mute_list_info: (serde_json::Value, Timestamp) = match stmt
            .query_row([pubkey.to_sql_string()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            }) {
            Ok(info) => (info.0, Timestamp::from_sql_string(info.1)?),
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
        cache_max_age: std::time::Duration,
    ) -> NResult<Self> {
        let connection = db.get()?;
        Self::setup_database(&connection)?;

        let db = Arc::new(Mutex::new(db));
        let event_saver = EventSaver::new(db.clone());

        let manager = NotificationManager {
            db,
            apns_topic: None,
            apns_client: None,
            fcm_client: None,
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

    /// Adds APNS configuration
    pub fn with_apns(
        mut self,
        apns_private_key_path: String,
        apns_private_key_id: String,
        apns_team_id: String,
        apns_environment: a2::client::Endpoint,
        apns_topic: String,
    ) -> NResult<Self> {
        let mut file = File::open(&apns_private_key_path)?;

        let client = Client::token(
            &mut file,
            &apns_private_key_id,
            &apns_team_id,
            ClientConfig::new(apns_environment.clone()),
        )?;

        self.apns_client.replace(Mutex::new(client));
        self.apns_topic.replace(apns_topic);

        Ok(self)
    }

    pub fn with_fcm(mut self, google_services_file_path: impl Into<String>) -> NResult<Self> {
        self.fcm_client
            .replace(Mutex::new(FcmService::new(google_services_file_path)));
        Ok(self)
    }

    pub async fn handle_event(&self, event: &Event) -> NResult<()> {
        log::info!(
            "Received event kind={},id={}",
            event.kind.as_u16(),
            event.notification_id(),
        );
        log::debug!("Event received: {:?}", event);
        self.event_saver.save_if_needed(&event).await?;
        self.send_notifications_if_needed(&event).await?;
        Ok(())
    }

    pub fn has_backend(&self, backend: NotificationBackend) -> bool {
        match backend {
            NotificationBackend::APNS if !self.apns_client.is_some() => true,
            NotificationBackend::FCM if self.fcm_client.is_some() => true,
            _ => false,
        }
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
                pubkey TEXT,
                kinds TEXT
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

        // Migration for FCM

        Self::add_column_if_not_exists(db, "user_info", "backend", "TINYINT", Some("0"))?;

        // Migration to kinds based preferences
        if (Self::add_column_if_not_exists(db, "user_info", "kinds", "TEXT", None)?) {
            // migrate existing settings into kinds col
            db.execute_batch(
                "
UPDATE user_info
SET kinds = TRIM(
    CASE WHEN zap_notifications_enabled = 1 THEN '9735,9733' ELSE '' END || ',' ||
    CASE WHEN mention_notifications_enabled = 1 THEN '1' ELSE '' END || ',' ||
    CASE WHEN repost_notifications_enabled = 1 THEN '6,17' ELSE '' END || ',' ||
    CASE WHEN reaction_notifications_enabled = 1 THEN '7' ELSE '' END || ',' ||
    CASE WHEN dm_notifications_enabled = 1 THEN '4' ELSE '' END,
    ','
);
ALTER TABLE user_info drop column zap_notifications_enabled;
ALTER TABLE user_info drop column mention_notifications_enabled;
ALTER TABLE user_info drop column repost_notifications_enabled;
ALTER TABLE user_info drop column reaction_notifications_enabled;
ALTER TABLE user_info drop column dm_notifications_enabled;",
            )?;
        }

        // Migration tracking priority notifications (notify me when somebody posts x)
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS notify_keys (
                user_pubkey TEXT NOT NULL,
                target_pubkey TEXT NOT NULL,
                created_at TEXT NOT NULL,
                kinds TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS notify_keys_user_target ON notify_keys (user_pubkey, target_pubkey);",
        )?;

        // Migration to avoid extra SQL query when inserting user_info
        db.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS user_info_device ON user_info (pubkey, device_token);")?;

        Ok(())
    }

    fn add_column_if_not_exists(
        db: &rusqlite::Connection,
        table_name: &str,
        column_name: &str,
        column_type: &str,
        default_value: Option<&str>,
    ) -> Result<bool, rusqlite::Error> {
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
            return Ok(true);
        }
        Ok(false)
    }

    // MARK: - "Business" logic

    pub async fn send_notifications_if_needed(&self, event: &Event) -> NResult<()> {
        log::debug!(
            "Checking if notifications need to be sent for event: {}",
            event.id
        );
        let event_started = match event.kind {
            // Accept starts tag as when the stream started, otherwise use created_at timestamp
            Kind::LiveEvent => event
                .tags
                .find(TagKind::Starts)
                .and_then(|t| t.content())
                .and_then(|t| Timestamp::from_str(t).ok())
                .unwrap_or(event.created_at),
            _ => event.created_at,
        };
        let one_week_ago = Timestamp::now() - 7 * 24 * 60 * 60;
        if event_started < one_week_ago {
            log::debug!("Event is older than a week, not sending notifications");
            return Ok(());
        }

        // Allow notes that are created no more than 3 seconds in the future
        // to account for natural clock skew between sender and receiver.
        if event_started > Timestamp::now() + 3 {
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
            match self
                .send_event_notifications_to_pubkey(event, &pubkey)
                .await
            {
                Ok(_) => {
                    let db_mutex_guard = self.db.lock().await;
                    db_mutex_guard.get()?.execute(
                        "INSERT OR REPLACE INTO notifications (id, event_id, pubkey, received_notification, sent_at)
                    VALUES (?, ?, ?, ?, ?)",
                        params![
                            format!("{}:{}", event.id, pubkey),
                            event.notification_id(),
                            pubkey.to_sql_string(),
                            true,
                            Timestamp::now().to_sql_string(),
                        ],
                    )?;
                }
                Err(e) => log::warn!(
                    "Error while sending notifications to {}: {}",
                    pubkey.to_hex(),
                    e.to_string()
                ),
            }
        }
        Ok(())
    }

    pub fn supported_kinds() -> Vec<Kind> {
        vec![
            Kind::TextNote,
            Kind::EncryptedDirectMessage,
            Kind::Repost,
            Kind::GenericRepost,
            Kind::Reaction,
            Kind::ZapPrivateMessage,
            Kind::ZapReceipt,
            Kind::LiveEvent,
            Kind::LongFormTextNote,
        ]
    }

    fn is_event_kind_supported(event_kind: Kind) -> bool {
        Self::supported_kinds().contains(&event_kind)
    }

    async fn pubkeys_to_notify_for_event(&self, event: &Event) -> NResult<HashSet<PublicKey>> {
        let notification_status = self.get_notification_status(event).await?;
        let relevant_pubkeys = self.pubkeys_relevant_to_event(event).await?;
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
    ) -> NResult<Option<MuteList>> {
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

    async fn pubkeys_relevant_to_event(&self, event: &Event) -> NResult<HashSet<PublicKey>> {
        let mut direct_keys = event.relevant_pubkeys();
        let fake_target = match event.kind {
            // accept p tagged host as target for notifications in live event
            Kind::LiveEvent => event
                .tags
                .iter()
                .find(|t| {
                    t.kind() == TagKind::p()
                        && t.as_slice().len() > 3
                        && t.as_slice()[3] == "host"
                })
                .and_then(|t| PublicKey::from_hex(t.content()?).ok())
                .unwrap_or(event.pubkey),
            _ => event.pubkey,
        };
        let notify_keys = self.get_notify_from_target(&fake_target).await?;
        direct_keys.extend(
            notify_keys
                .into_iter()
                .filter(|k| k.1.contains(&event.kind))
                .map(|k| k.0),
        );

        Ok(direct_keys)
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
    ) -> NResult<()> {
        let user_device_tokens = self.get_user_device_tokens(pubkey).await?;
        for (device_token, backend) in user_device_tokens {
            let wants_notif = self
                .user_wants_notification(pubkey, &device_token, event)
                .await?;
            if !wants_notif {
                continue;
            }
            // as long as we send to at least one device for each user we consider it a success
            if let Err(e) = self
                .send_event_notification_to_device_token(event, &device_token, backend)
                .await
            {
                log::warn!("Failed to send notification to {}: {}", &device_token, e);
            }
        }
        Ok(())
    }

    async fn user_wants_notification(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
        event: &Event,
    ) -> NResult<bool> {
        let notification_preferences = self
            .get_user_notification_settings(pubkey, device_token)
            .await?;
        if notification_preferences.only_notifications_from_following_enabled
            && !self
                .nostr_network_helper
                .does_pubkey_follow_pubkey(pubkey, &event.pubkey)
                .await
        {
            return Ok(false);
        }
        if notification_preferences.hellthread_notifications_disabled
            && self.is_hellthread_eligible(event.kind)
        {
            if let Ok(pubkeys_count) = i8::try_from(self.pubkeys_referenced_by_event(event).len()) {
                if pubkeys_count > notification_preferences.hellthread_notifications_max_pubkeys {
                    return Ok(false);
                }
            }
        }
        Ok(notification_preferences.merge_kinds().contains(&event.kind))
    }

    async fn is_pubkey_registered(&self, pubkey: &PublicKey) -> NResult<bool> {
        Ok(!self.get_user_device_tokens(pubkey).await?.is_empty())
    }

    async fn get_notify_from_target(
        &self,
        target: &PublicKey,
    ) -> NResult<Vec<(PublicKey, Vec<Kind>)>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection
            .prepare("SELECT user_pubkey,kinds FROM notify_keys WHERE target_pubkey = ?")?;
        let pubkeys = stmt
            .query_map([target.to_sql_string()], |row| {
                let key = PublicKey::from_hex(row.get::<_, String>(0)?.as_str())
                    .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;
                let kinds: String = row.get(1)?;
                let kinds: Vec<Kind> = kinds
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .map_while(|s| Kind::from_str(&s).ok())
                    .collect();
                Ok((key, kinds))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(pubkeys)
    }

    async fn get_user_device_tokens(
        &self,
        pubkey: &PublicKey,
    ) -> NResult<Vec<(String, NotificationBackend)>> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt =
            connection.prepare("SELECT device_token,backend FROM user_info WHERE pubkey = ?")?;
        let device_tokens = stmt
            .query_map([pubkey.to_sql_string()], |row| {
                Ok((
                    row.get::<usize, String>(0)?,
                    row.get::<usize, u8>(1)?.into(),
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(device_tokens)
    }

    async fn get_notification_status(&self, event: &Event) -> NResult<NotificationStatus> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT pubkey, received_notification FROM notifications WHERE event_id = ?",
        )?;

        let rows: HashMap<PublicKey, bool> = stmt
            .query_map([event.notification_id()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .filter_map(|r| r.ok())
            .filter_map(|r| {
                let pubkey = PublicKey::from_sql_string(r.0).ok()?;
                let received_notification = r.1;
                Some((pubkey, received_notification))
            })
            .collect();

        let mut status_info = HashMap::new();
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
        backend: NotificationBackend,
    ) -> NResult<()> {
        log::debug!("Sending notification to device token: {}", device_token);

        match &backend {
            NotificationBackend::APNS => {
                self.send_event_notification_apns(event, device_token)
                    .await?;
            }
            NotificationBackend::FCM => {
                self.send_event_notification_fcm(event, device_token)
                    .await?;
            }
        }

        log::info!("Notification sent to device token: {}", device_token);
        Ok(())
    }

    async fn send_event_notification_apns(&self, event: &Event, device_token: &str) -> NResult<()> {
        let client = self
            .apns_client
            .as_ref()
            .ok_or(NotificationManagerError::APNSMissing)?;

        let (title, body) = self.format_notification_message(event);
        let mut payload = DefaultNotificationBuilder::new()
            .set_title(&title)
            .set_body(&body)
            .set_mutable_content()
            .set_content_available()
            .build(device_token, Default::default());

        if let Some(t) = self.apns_topic.as_ref() {
            payload.options.apns_topic = Some(t);
        }
        payload.data.insert(
            "nostr_event",
            serde_json::Value::String(event.try_as_json()?),
        );

        let apns_client_mutex_guard = client.lock().await;
        match apns_client_mutex_guard.send(payload).await {
            Ok(_response) => {}
            Err(e) => log::error!(
                "Failed to send notification to device token '{}': {}",
                device_token,
                e
            ),
        }

        Ok(())
    }

    async fn send_event_notification_fcm(&self, event: &Event, device_token: &str) -> NResult<()> {
        let client = self
            .fcm_client
            .as_ref()
            .ok_or(NotificationManagerError::FCMMissing)?;

        let (title, body) = self.format_notification_message(event);

        let client = client.lock().await;
        let mut msg = FcmMessage::new();
        let mut notification = FcmNotification::new();
        notification.set_title(title);
        notification.set_body(body);
        msg.set_notification(Some(notification));
        msg.set_target(Target::Token(device_token.into()));
        msg.set_data(Some(HashMap::from([(
            "nostr_event".to_string(),
            event.as_json(),
        )])));
        client
            .send_notification(msg)
            .await
            .map_err(|e| e.to_string())?;

        Ok(())
    }

    fn format_notification_message(&self, event: &Event) -> (String, String) {
        // NOTE: This is simple because the client will handle formatting. These are just fallbacks.
        match event.kind {
            Kind::TextNote => ("New activity".to_string(), event.content.clone()),
            Kind::EncryptedDirectMessage => (
                "New direct message".to_string(),
                "Contents are encrypted".to_string(),
            ),
            Kind::Repost => ("Someone reposted".to_string(), event.content.clone()),
            Kind::Reaction => {
                let content_text = event.content.clone();
                let formatted_text = match content_text.as_str() {
                    "" => "❤️",
                    "+" => "❤️",
                    "-" => "👎",
                    _ => content_text.as_str(),
                };
                ("New reaction".to_string(), formatted_text.to_string())
            }
            Kind::ZapPrivateMessage => (
                "New zap private message".to_string(),
                "Contents are encrypted".to_string(),
            ),
            Kind::ZapReceipt => ("Someone zapped you".to_string(), "".to_string()),
            Kind::LiveEvent => (
                "Stream is Live".to_string(),
                event
                    .tags
                    .iter()
                    .find(|t| t.kind() == TagKind::Title)
                    .and_then(|t| t.content())
                    .unwrap_or("")
                    .to_string(),
            ),
            _ => ("New activity".to_string(), "".to_string()),
        }
    }

    // MARK: - User device info and settings

    pub async fn save_user_device_info(
        &self,
        pubkey: PublicKey,
        device_token: &str,
        backend: NotificationBackend,
    ) -> NResult<()> {
        let current_time_unix = Timestamp::now();
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "INSERT OR IGNORE INTO user_info (id, pubkey, device_token, added_at, backend) VALUES (?, ?, ?, ?, ?)",
            params![
                format!("{}:{}", pubkey.to_sql_string(), device_token),
                pubkey.to_sql_string(),
                device_token,
                current_time_unix.to_sql_string(),
                <NotificationBackend as Into<u8>>::into(backend)
            ],
        )?;
        Ok(())
    }

    pub async fn remove_user_device_info(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> NResult<()> {
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "DELETE FROM user_info WHERE pubkey = ? AND device_token = ?",
            params![pubkey.to_sql_string(), device_token],
        )?;
        Ok(())
    }

    pub async fn get_notify_keys(&self, pubkey: PublicKey) -> NResult<Vec<PublicKey>> {
        let db_mutex_guard = self.db.lock().await;
        let conn = db_mutex_guard.get()?;
        let mut stmt =
            conn.prepare("select target_pubkey from notify_keys where user_pubkey = ?")?;
        let pubkeys = stmt
            .query_map([pubkey.to_sql_string()], |row| {
                Ok(PublicKey::from_hex(row.get::<_, String>(0)?.as_str()))
            })?
            .flatten()
            .filter_map(|r| r.ok())
            .collect();
        Ok(pubkeys)
    }

    pub async fn save_notify_key(
        &self,
        pubkey: PublicKey,
        target: PublicKey,
        kinds: Vec<Kind>,
    ) -> NResult<()> {
        let current_time_unix = Timestamp::now();
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "INSERT OR REPLACE INTO notify_keys (user_pubkey, target_pubkey, created_at, kinds) VALUES (?, ?, ?, ?)",
            params![
                pubkey.to_sql_string(),
                target.to_sql_string(),
                current_time_unix.to_sql_string(),
                kinds.iter().map(|k| k.as_u16().to_string()).collect::<Vec<String>>().join(",")
            ],
        )?;
        Ok(())
    }

    pub async fn delete_notify_key(&self, pubkey: PublicKey, target: PublicKey) -> NResult<()> {
        let db_mutex_guard = self.db.lock().await;
        db_mutex_guard.get()?.execute(
            "delete from notify_keys where user_pubkey = ? and target_pubkey = ?",
            params![pubkey.to_sql_string(), target.to_sql_string()],
        )?;
        Ok(())
    }

    pub async fn get_user_notification_settings(
        &self,
        pubkey: &PublicKey,
        device_token: &str,
    ) -> NResult<UserNotificationSettings> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        let mut stmt = connection.prepare(
            "SELECT kinds, only_notifications_from_following_enabled, hellthread_notifications_disabled, hellthread_notifications_max_pubkeys FROM user_info WHERE pubkey = ? AND device_token = ?",
        )?;
        let settings = stmt.query_row([pubkey.to_sql_string().as_str(), device_token], |row| {
            let kinds: String = row.get(0)?;
            let kinds: Vec<Kind> = kinds
                .split(',')
                .map(|s| s.trim().to_string())
                .map_while(|s| Kind::from_str(&s).ok())
                .collect();
            Ok(UserNotificationSettings {
                zap_notifications_enabled: kinds.contains(&Kind::ZapReceipt),
                mention_notifications_enabled: kinds.contains(&Kind::TextNote),
                repost_notifications_enabled: kinds.contains(&Kind::Repost)
                    || kinds.contains(&Kind::GenericRepost),
                reaction_notifications_enabled: kinds.contains(&Kind::Reaction),
                dm_notifications_enabled: kinds.contains(&Kind::EncryptedDirectMessage),
                only_notifications_from_following_enabled: row.get(1)?,
                hellthread_notifications_disabled: row.get::<_, Option<bool>>(2)?.unwrap_or(false),
                hellthread_notifications_max_pubkeys: row
                    .get::<_, Option<i8>>(3)?
                    .unwrap_or(DEFAULT_HELLTHREAD_MAX_PUBKEYS),
                kinds,
            })
        })?;

        Ok(settings)
    }

    pub async fn save_user_notification_settings(
        &self,
        pubkey: &PublicKey,
        device_token: String,
        settings: UserNotificationSettings,
    ) -> NResult<()> {
        let db_mutex_guard = self.db.lock().await;
        let connection = db_mutex_guard.get()?;
        connection.execute(
            "UPDATE user_info SET kinds = ?, only_notifications_from_following_enabled = ?, hellthread_notifications_disabled = ?, hellthread_notifications_max_pubkeys = ? WHERE pubkey = ? AND device_token = ?",
            params![
                settings.merge_kinds().iter()
                    .map(|k| k.as_u16().to_string())
                    .collect::<Vec<String>>().join(","),
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
pub struct KindsSettings {
    #[serde(default)]
    pub kinds: Vec<Kind>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct UserNotificationSettings {
    #[serde(default)]
    zap_notifications_enabled: bool,
    #[serde(default)]
    mention_notifications_enabled: bool,
    #[serde(default)]
    repost_notifications_enabled: bool,
    #[serde(default)]
    reaction_notifications_enabled: bool,
    #[serde(default)]
    dm_notifications_enabled: bool,
    #[serde(default)]
    only_notifications_from_following_enabled: bool,

    #[serde(default)]
    hellthread_notifications_disabled: bool,

    #[serde(default = "default_hellthread_max_pubkeys")]
    hellthread_notifications_max_pubkeys: i8,

    /// Generic nostr event kinds
    #[serde(default)]
    kinds: Vec<Kind>,
}

impl UserNotificationSettings {
    /// Merge kinds array will boolean flags
    pub fn merge_kinds(&self) -> Vec<Kind> {
        let mut ret: HashSet<Kind> = self.kinds.clone().into_iter().collect();
        if (self.zap_notifications_enabled) {
            ret.insert(Kind::ZapReceipt);
            ret.insert(Kind::ZapPrivateMessage);
        }
        if (self.mention_notifications_enabled) {
            ret.insert(Kind::TextNote);
        }
        if (self.repost_notifications_enabled) {
            ret.insert(Kind::Repost);
            ret.insert(Kind::GenericRepost);
        }
        if (self.reaction_notifications_enabled) {
            ret.insert(Kind::Reaction);
        }
        if (self.dm_notifications_enabled) {
            ret.insert(Kind::EncryptedDirectMessage);
        }
        ret.into_iter().collect()
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
            .map(|(pubkey, _)| *pubkey)
            .collect()
    }
}
