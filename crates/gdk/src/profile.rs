//! Who and what the runtime reports: the one local user, the title and its package, the console.
//! The lead fills it from `rebuild.toml` (gamertag) and the title's manifest (title id, store id) at
//! start-up, before the title runs; until then the defaults stand.

use std::sync::RwLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Console {
    SeriesX,
    SeriesS,
}

impl Console {
    /// `XSystemAnalyticsInfo::form`.
    pub fn form(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The signed-in user's gamertag: 1-15 bytes, so it fits every gamertag component's buffer
    /// (classic: 16 bytes with the NUL).
    pub gamertag: String,
    /// The user's Xbox user id (`XUserGetId`).
    pub xuid: u64,
    /// `XGameGetXboxTitleId`, from the manifest.
    pub title_id: u32,
    /// The game's Store id (`MicrosoftGame.config` `StoreId`), empty if unknown.
    pub store_id: String,
    /// The installation's package identifier as the runtime hands it out (at most 32 bytes).
    pub package_id: String,
    /// Bytes in the package (the game folder), for installation progress.
    pub package_bytes: u64,
    pub console: Console,
    /// `XSystemGetXboxLiveSandboxId`.
    pub sandbox: String,
}

impl Default for Profile {
    fn default() -> Profile {
        Profile {
            gamertag: "Player".into(),
            xuid: 0x0009_0000_0000_0001,
            title_id: 0,
            store_id: String::new(),
            package_id: "00000000".into(),
            package_bytes: 0,
            console: Console::SeriesX,
            sandbox: "RETAIL".into(),
        }
    }
}

impl Profile {
    /// Every rule the runtime's buffers impose; problems by name.
    pub fn check(&self) -> Result<(), String> {
        let gt = self.gamertag.as_bytes();
        if gt.is_empty() || gt.len() > 15 || gt.contains(&0) {
            return Err(format!("gamertag must be 1-15 bytes without NUL: {:?}", self.gamertag));
        }
        if self.package_id.is_empty() || self.package_id.len() > 32 || self.package_id.contains('\0') {
            return Err(format!("package id must be 1-32 bytes: {:?}", self.package_id));
        }
        if self.sandbox.is_empty() || self.sandbox.len() > 15 {
            return Err(format!("sandbox must be 1-15 bytes: {:?}", self.sandbox));
        }
        if self.xuid == 0 {
            return Err("xuid must not be 0".into());
        }
        Ok(())
    }
}

static PROFILE: RwLock<Option<Profile>> = RwLock::new(None);

/// Sets what the runtime reports from now on.
pub fn configure(profile: Profile) -> Result<(), String> {
    profile.check()?;
    *PROFILE.write().unwrap_or_else(|e| e.into_inner()) = Some(profile);
    Ok(())
}

/// What the runtime reports.
pub fn current() -> Profile {
    PROFILE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks() {
        assert!(Profile::default().check().is_ok());
        let long = Profile { gamertag: "a".repeat(16), ..Profile::default() };
        assert!(long.check().is_err());
        let empty = Profile { gamertag: String::new(), ..Profile::default() };
        assert!(configure(empty).is_err());
        assert!(Profile { package_id: "x".repeat(33), ..Profile::default() }.check().is_err());
    }
}
