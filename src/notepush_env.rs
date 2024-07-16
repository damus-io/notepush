use std::env;
use dotenv::dotenv;
use a2;

const DEFAULT_DB_PATH: &str = "./apns_notifications.db";
const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: &str = "9001";
const DEFAULT_RELAY_URL: &str = "ws://localhost:7777";

pub struct NotePushEnv {
    pub apns_private_key_path: String,
    pub apns_private_key_id: String,
    pub apns_team_id: String,
    pub apns_environment: a2::client::Endpoint,
    pub apns_topic: String,
    pub db_path: String,
    pub host: String,
    pub port: String,
    pub relay_url: String,
}

impl NotePushEnv {
    pub fn load_env() -> Result<NotePushEnv, env::VarError> {
        dotenv().ok();
        let apns_private_key_path = env::var("APNS_AUTH_PRIVATE_KEY_FILE_PATH")?;
        let apns_private_key_id = env::var("APNS_AUTH_PRIVATE_KEY_ID")?;
        let apns_team_id = env::var("APPLE_TEAM_ID")?;
        let db_path = env::var("DB_PATH").unwrap_or(DEFAULT_DB_PATH.to_string());
        let host = env::var("HOST").unwrap_or(DEFAULT_HOST.to_string());
        let port = env::var("PORT").unwrap_or(DEFAULT_PORT.to_string());
        let relay_url = env::var("RELAY_URL").unwrap_or(DEFAULT_RELAY_URL.to_string());
        let apns_environment_string = env::var("APNS_ENVIRONMENT").unwrap_or("development".to_string());
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
            host,
            port,
            relay_url,
        })
    }
    
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
