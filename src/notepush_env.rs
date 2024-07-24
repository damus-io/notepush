use std::env;
use dotenv::dotenv;
use a2;

const DEFAULT_DB_PATH: &str = "./apns_notifications.db";
const DEFAULT_RELAY_HOST: &str = "0.0.0.0";
const DEFAULT_RELAY_PORT: &str = "9001";
const DEFAULT_RELAY_URL: &str = "wss://relay.damus.io";
const DEFAULT_API_HOST: &str = "0.0.0.0";
const DEFAULT_API_PORT: &str = "8000";

pub struct NotePushEnv {
    // The path to the Apple private key .p8 file
    pub apns_private_key_path: String,
    // The Apple private key ID
    pub apns_private_key_id: String,
    // The Apple team ID
    pub apns_team_id: String,
    // The APNS environment to send notifications to (Sandbox or Production)
    pub apns_environment: a2::client::Endpoint,
    // The topic to send notifications to (The Apple app bundle ID)
    pub apns_topic: String,
    // The path to the SQLite database file
    pub db_path: String,
    // The host and port to bind the relay server to
    pub relay_host: String,
    pub relay_port: String,
    // The host and port to bind the API server to
    pub api_host: String,
    pub api_port: String,
    pub api_base_url: String,   // The base URL of where the API server is hosted for NIP-98 auth checks
    // The URL of the Nostr relay server to connect to for getting mutelists
    pub relay_url: String,
}

impl NotePushEnv {
    pub fn load_env() -> Result<NotePushEnv, env::VarError> {
        dotenv().ok();
        let apns_private_key_path = env::var("APNS_AUTH_PRIVATE_KEY_FILE_PATH")?;
        let apns_private_key_id = env::var("APNS_AUTH_PRIVATE_KEY_ID")?;
        let apns_team_id = env::var("APPLE_TEAM_ID")?;
        let db_path = env::var("DB_PATH").unwrap_or(DEFAULT_DB_PATH.to_string());
        let relay_host = env::var("RELAY_HOST").unwrap_or(DEFAULT_RELAY_HOST.to_string());
        let relay_port = env::var("RELAY_PORT").unwrap_or(DEFAULT_RELAY_PORT.to_string());
        let relay_url = env::var("RELAY_URL").unwrap_or(DEFAULT_RELAY_URL.to_string());
        let apns_environment_string = env::var("APNS_ENVIRONMENT").unwrap_or("development".to_string());
        let api_host = env::var("API_HOST").unwrap_or(DEFAULT_API_HOST.to_string());
        let api_port = env::var("API_PORT").unwrap_or(DEFAULT_API_PORT.to_string());
        let api_base_url = env::var("API_BASE_URL").unwrap_or(format!("https://{}:{}", api_host, api_port));
        let apns_environment = match apns_environment_string.as_str() {
            "development" => a2::client::Endpoint::Sandbox,
            "production" => a2::client::Endpoint::Production,
            _ => a2::client::Endpoint::Sandbox,
        };
        let apns_topic = env::var("APNS_TOPIC")?;
        
        Ok(NotePushEnv {
            apns_private_key_path,
            apns_private_key_id,
            apns_team_id,
            apns_environment,
            apns_topic,
            db_path,
            relay_host,
            relay_port,
            api_host,
            api_port,
            api_base_url,
            relay_url,
        })
    }
    
    pub fn relay_address(&self) -> String {
        format!("{}:{}", self.relay_host, self.relay_port)
    }
}
