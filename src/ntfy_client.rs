//! ntfy.sh client for sending push notifications to Android devices
//!
//! This module provides a simple HTTP client for sending notifications
//! via ntfy (https://ntfy.sh), an open-source push notification service.
//!
//! Used as an alternative to APNs for Android devices running notedeck.

use serde::{Deserialize, Serialize};

/// ntfy client for sending push notifications
pub struct NtfyClient {
    client: reqwest::Client,
    server_url: String,
}

/// Payload sent to ntfy server
#[derive(Serialize, Debug)]
struct NtfyPayload {
    /// The notification topic (unique per device)
    topic: String,
    /// Notification title
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// Notification message body
    message: String,
    /// Priority (1-5, default 3)
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<u8>,
    /// Custom data payload (encrypted notification)
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<String>,
}

/// Response from ntfy server
#[derive(Deserialize, Debug)]
pub struct NtfyResponse {
    pub id: String,
    pub time: u64,
    pub event: String,
    pub topic: String,
}

/// Error types for ntfy operations
#[derive(Debug, thiserror::Error)]
pub enum NtfyError {
    #[error("HTTP request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("ntfy server returned error: {status} - {message}")]
    ServerError { status: u16, message: String },
}

impl NtfyClient {
    /// Create a new ntfy client
    ///
    /// # Arguments
    /// * `server_url` - Base URL of the ntfy server (e.g., "https://ntfy.damus.io")
    pub fn new(server_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self { client, server_url }
    }

    /// Send a notification to a topic
    ///
    /// # Arguments
    /// * `topic` - The notification topic (device-specific identifier)
    /// * `title` - Notification title
    /// * `body` - Notification body text
    /// * `data` - Optional JSON data payload (for encrypted notifications)
    pub async fn send(
        &self,
        topic: &str,
        title: &str,
        body: &str,
        data: Option<&str>,
    ) -> Result<NtfyResponse, NtfyError> {
        let url = format!("{}/{}", self.server_url, topic);

        // Build request with headers for data payload
        let mut request = self.client
            .post(&url)
            .header("Title", title);

        // If we have encrypted data, send it in the X-Data header
        // The client will parse this to get the encrypted notification
        if let Some(payload) = data {
            request = request
                .header("X-Data", payload)
                .header("X-Priority", "high");
        }

        let response = request
            .body(body.to_string())
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(NtfyError::ServerError { status, message });
        }

        let ntfy_response = response.json::<NtfyResponse>().await?;
        Ok(ntfy_response)
    }

    /// Send a raw JSON payload to a topic
    ///
    /// This is the preferred method for sending encrypted notifications,
    /// as it allows full control over the payload structure.
    ///
    /// # Arguments
    /// * `topic` - The notification topic
    /// * `json_payload` - Complete JSON payload to send
    pub async fn send_json(
        &self,
        topic: &str,
        json_payload: &serde_json::Value,
    ) -> Result<NtfyResponse, NtfyError> {
        let url = format!("{}/{}", self.server_url, topic);

        let response = self.client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(json_payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(NtfyError::ServerError { status, message });
        }

        let ntfy_response = response.json::<NtfyResponse>().await?;
        Ok(ntfy_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = NtfyClient::new("https://ntfy.damus.io".to_string());
        assert_eq!(client.server_url, "https://ntfy.damus.io");
    }
}
