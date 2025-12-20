pub mod notification_manager;
pub mod ntfy_client;
pub mod server_keys;
pub mod utils;

/// Device platform for push notification routing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Platform {
    /// iOS/macOS devices using APNs
    #[default]
    Ios,
    /// Android devices using ntfy
    Android,
}

impl Platform {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "android" => Platform::Android,
            "ios" | "apple" | "macos" => Platform::Ios,
            _ => Platform::Ios, // Default to iOS for backwards compatibility
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }
}
