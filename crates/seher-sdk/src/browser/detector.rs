use super::types::{BrowserType, Profile};
use std::path::{Path, PathBuf};

pub struct BrowserDetector {
    home_dir: PathBuf,
}

impl BrowserDetector {
    #[must_use]
    pub fn new() -> Self {
        let home_dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self { home_dir }
    }

    #[must_use]
    pub fn detect_browsers(&self) -> Vec<BrowserType> {
        let mut browsers = Vec::new();

        for browser_type in [
            BrowserType::Chrome,
            BrowserType::Edge,
            BrowserType::Brave,
            BrowserType::Chromium,
            BrowserType::Vivaldi,
            BrowserType::Comet,
            BrowserType::Dia,
            BrowserType::Atlas,
            BrowserType::Firefox,
            BrowserType::Safari,
        ] {
            if self.is_browser_installed(browser_type) {
                browsers.push(browser_type);
            }
        }

        browsers
    }

    fn is_browser_installed(&self, browser_type: BrowserType) -> bool {
        self.get_browser_base_path(browser_type)
            .is_some_and(|p| p.exists())
    }

    #[must_use]
    pub fn get_browser_base_path(&self, browser_type: BrowserType) -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            let path = match browser_type {
                BrowserType::Chrome => {
                    self.home_dir.join("Library/Application Support/Google/Chrome")
                }
                BrowserType::Edge => {
                    self.home_dir.join("Library/Application Support/Microsoft Edge")
                }
                BrowserType::Brave => {
                    self.home_dir.join("Library/Application Support/BraveSoftware/Brave-Browser")
                }
                BrowserType::Chromium => {
                    self.home_dir.join("Library/Application Support/Chromium")
                }
                BrowserType::Vivaldi => {
                    self.home_dir.join("Library/Application Support/Vivaldi")
                }
                BrowserType::Comet => {
                    self.home_dir.join("Library/Application Support/Comet")
                }
                BrowserType::Dia => {
                    self.home_dir.join("Library/Application Support/Dia")
                }
                BrowserType::Atlas => {
                    self.home_dir.join("Library/Application Support/Atlas")
                }
                BrowserType::Firefox => {
                    self.home_dir.join("Library/Application Support/Firefox")
                }
                BrowserType::Safari => {
                    self.home_dir.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies")
                }
            };
            Some(path)
        }

        #[cfg(target_os = "linux")]
        {
            let config_dir = self.home_dir.join(".config");
            let path = match browser_type {
                BrowserType::Chrome => config_dir.join("google-chrome"),
                BrowserType::Edge => config_dir.join("microsoft-edge"),
                BrowserType::Brave => config_dir.join("BraveSoftware/Brave-Browser"),
                BrowserType::Chromium => config_dir.join("chromium"),
                BrowserType::Vivaldi => config_dir.join("vivaldi"),
                BrowserType::Comet => config_dir.join("comet"),
                BrowserType::Dia => config_dir.join("dia"),
                BrowserType::Atlas => config_dir.join("atlas"),
                BrowserType::Firefox => self.home_dir.join(".mozilla/firefox"),
                BrowserType::Safari => return None,
            };
            Some(path)
        }

        #[cfg(target_os = "windows")]
        {
            let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
            let app_data = std::env::var("APPDATA").ok()?;
            let base = PathBuf::from(local_app_data);
            let roaming_base = PathBuf::from(app_data);
            let path = match browser_type {
                BrowserType::Chrome => base.join("Google\\Chrome\\User Data"),
                BrowserType::Edge => base.join("Microsoft\\Edge\\User Data"),
                BrowserType::Brave => base.join("BraveSoftware\\Brave-Browser\\User Data"),
                BrowserType::Chromium => base.join("Chromium\\User Data"),
                BrowserType::Vivaldi => base.join("Vivaldi\\User Data"),
                BrowserType::Comet => base.join("Comet\\User Data"),
                BrowserType::Dia => base.join("Dia\\User Data"),
                BrowserType::Atlas => base.join("Atlas\\User Data"),
                BrowserType::Firefox => roaming_base.join("Mozilla\\Firefox"),
                BrowserType::Safari => return None,
            };
            Some(path)
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        None
    }

    #[must_use]
    pub fn list_profiles(&self, browser_type: BrowserType) -> Vec<Profile> {
        let base_path = match self.get_browser_base_path(browser_type) {
            Some(p) if p.exists() => p,
            _ => return Vec::new(),
        };

        if browser_type == BrowserType::Firefox {
            return Self::list_firefox_profiles(&base_path);
        }

        if browser_type == BrowserType::Safari {
            return vec![Profile::new(
                "Default".to_string(),
                base_path,
                BrowserType::Safari,
            )];
        }

        let mut profiles = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&base_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }

                let file_name = entry.file_name();
                let name = file_name.to_string_lossy();

                if name == "Default" || name.starts_with("Profile ") {
                    let cookies_path = path.join("Cookies");
                    if cookies_path.exists() {
                        let profile_name = if name == "Default" {
                            "Default".to_string()
                        } else {
                            name.to_string()
                        };
                        profiles.push(Profile::new(profile_name, path, browser_type));
                    }
                }
            }
        }

        profiles.sort_by(|a, b| {
            if a.name == "Default" {
                std::cmp::Ordering::Less
            } else if b.name == "Default" {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });

        profiles
    }

    fn list_firefox_profiles(base_path: &Path) -> Vec<Profile> {
        let profiles_ini = base_path.join("profiles.ini");
        if !profiles_ini.exists() {
            return Vec::new();
        }

        let mut profiles = Vec::new();

        if let Ok(content) = std::fs::read_to_string(&profiles_ini) {
            let mut current_profile_name = None;
            let mut current_profile_path = None;
            let mut current_is_relative = true;

            for line in content.lines() {
                let line = line.trim();

                if line.starts_with("[Profile") {
                    if let (Some(name), Some(path)) =
                        (current_profile_name.take(), current_profile_path.take())
                    {
                        let full_path = if current_is_relative {
                            base_path.join(path)
                        } else {
                            PathBuf::from(path)
                        };

                        if full_path.join("cookies.sqlite").exists() {
                            profiles.push(Profile::new(name, full_path, BrowserType::Firefox));
                        }
                    }
                    current_is_relative = true;
                } else if let Some(stripped) = line.strip_prefix("Name=") {
                    current_profile_name = Some(stripped.to_string());
                } else if let Some(stripped) = line.strip_prefix("Path=") {
                    current_profile_path = Some(stripped.to_string());
                } else if let Some(stripped) = line.strip_prefix("IsRelative=") {
                    current_is_relative = stripped.trim() == "1";
                }
            }

            if let (Some(name), Some(path)) = (current_profile_name, current_profile_path) {
                let full_path = if current_is_relative {
                    base_path.join(path)
                } else {
                    PathBuf::from(path)
                };

                if full_path.join("cookies.sqlite").exists() {
                    profiles.push(Profile::new(name, full_path, BrowserType::Firefox));
                }
            }
        }

        profiles
    }

    #[must_use]
    pub fn get_profile(
        &self,
        browser_type: BrowserType,
        profile_name: Option<&str>,
    ) -> Option<Profile> {
        let profiles = self.list_profiles(browser_type);

        match profile_name {
            Some(name) => profiles.into_iter().find(|p| p.name == name),
            None => profiles.into_iter().next(),
        }
    }
}

impl Default for BrowserDetector {
    fn default() -> Self {
        Self::new()
    }
}
