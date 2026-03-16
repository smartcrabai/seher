use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserType {
    Chrome,
    Edge,
    Brave,
    Chromium,
    Vivaldi,
    Comet,
    Dia,
    Atlas,
    Firefox,
    Safari,
}

impl BrowserType {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            BrowserType::Chrome => "Chrome",
            BrowserType::Edge => "Edge",
            BrowserType::Brave => "Brave",
            BrowserType::Chromium => "Chromium",
            BrowserType::Vivaldi => "Vivaldi",
            BrowserType::Comet => "Comet",
            BrowserType::Dia => "Dia",
            BrowserType::Atlas => "Atlas",
            BrowserType::Firefox => "Firefox",
            BrowserType::Safari => "Safari",
        }
    }

    #[must_use]
    pub fn is_chromium_based(&self) -> bool {
        matches!(
            self,
            BrowserType::Chrome
                | BrowserType::Edge
                | BrowserType::Brave
                | BrowserType::Chromium
                | BrowserType::Vivaldi
                | BrowserType::Comet
                | BrowserType::Dia
                | BrowserType::Atlas
        )
    }
}

impl std::str::FromStr for BrowserType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "chrome" => Ok(BrowserType::Chrome),
            "edge" => Ok(BrowserType::Edge),
            "brave" => Ok(BrowserType::Brave),
            "chromium" => Ok(BrowserType::Chromium),
            "vivaldi" => Ok(BrowserType::Vivaldi),
            "comet" => Ok(BrowserType::Comet),
            "dia" => Ok(BrowserType::Dia),
            "atlas" => Ok(BrowserType::Atlas),
            "firefox" => Ok(BrowserType::Firefox),
            "safari" => Ok(BrowserType::Safari),
            other => Err(format!("Unknown browser: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub path: PathBuf,
    pub browser_type: BrowserType,
}

impl Profile {
    #[must_use]
    pub fn new(name: String, path: PathBuf, browser_type: BrowserType) -> Self {
        Self {
            name,
            path,
            browser_type,
        }
    }

    #[must_use]
    pub fn cookies_path(&self) -> PathBuf {
        match self.browser_type {
            BrowserType::Firefox => self.path.join("cookies.sqlite"),
            BrowserType::Safari => self.path.clone(),
            _ => self.path.join("Cookies"),
        }
    }

    #[must_use]
    pub fn local_state_path(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("Local State"), |p| p.join("Local State"))
    }
}

#[derive(Debug, Clone)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub expires_utc: i64,
    pub is_secure: bool,
    pub is_httponly: bool,
    pub same_site: i32,
}

impl Cookie {
    #[must_use]
    pub fn is_expired(&self) -> bool {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .cast_signed();

        // Chrome uses microseconds since Windows epoch (1601-01-01)
        // Convert to Unix timestamp
        let unix_timestamp = (self.expires_utc / 1_000_000) - 11_644_473_600;
        unix_timestamp > 0 && unix_timestamp < now
    }
}
