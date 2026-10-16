//! macOS integration: notifications and the system locale.

/// Prepare notifications (asks for permission the first time, when running
/// as an app bundle).
pub fn init() {
    #[cfg(target_os = "macos")]
    native::init();
}

/// Post a notification: natively from the app bundle, or through
/// `osascript` when running as a bare binary (shown as Script Editor).
pub fn post(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    if native::bundled() {
        native::post(title, body);
        return;
    }
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", quote(body), quote(title));
    let _ = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `LANG` for shells when the app wasn't given one (launched from Finder):
/// the system locale with UTF-8, e.g. `ko_KR.UTF-8`.
pub fn default_lang() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let (language, region) = native::locale()?;
        Some(pick_locale(&language, region.as_deref(), |name| {
            std::path::Path::new("/usr/share/locale").join(name).is_dir()
        }))
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// `language_REGION.UTF-8` if the system has it, else any installed UTF-8
/// locale for the language (Korean with a UK region → `ko_KR.UTF-8`), else
/// `en_US.UTF-8`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pick_locale(language: &str, region: Option<&str>, exists: impl Fn(&str) -> bool) -> String {
    if let Some(region) = region {
        let exact = format!("{language}_{region}.UTF-8");
        if exists(&exact) {
            return exact;
        }
    }
    let installed = std::fs::read_dir("/usr/share/locale").into_iter().flatten().flatten();
    let mut same_language: Vec<String> = installed
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&format!("{language}_")) && name.ends_with(".UTF-8"))
        .collect();
    same_language.sort();
    same_language.into_iter().next().unwrap_or_else(|| "en_US.UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use super::pick_locale;

    #[test]
    fn locales() {
        let installed = |name: &str| ["en_GB.UTF-8", "ko_KR.UTF-8"].contains(&name);
        assert_eq!(pick_locale("en", Some("GB"), installed), "en_GB.UTF-8");
        // Not on this test's list, but on every Mac: the language's locale.
        assert!(pick_locale("ko", Some("GB"), installed).starts_with("ko_"));
        assert_eq!(pick_locale("xx", None, installed), "en_US.UTF-8");
    }
}

#[cfg(target_os = "macos")]
mod native {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError, NSLocale, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNNotificationSound, UNUserNotificationCenter,
    };

    /// Notification Center only works for apps with a bundle identifier.
    pub fn bundled() -> bool {
        NSBundle::mainBundle().bundleIdentifier().is_some()
    }

    pub fn init() {
        if !bundled() {
            return;
        }
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let done = RcBlock::new(|_granted: Bool, _error: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &done,
        );
    }

    pub fn post(title: &str, body: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        let id = NSString::from_str(&format!("artty-{}-{}", std::process::id(), now_micros()));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, None);
    }

    fn now_micros() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros())
    }

    /// Language and region codes, e.g. `("ko", Some("GB"))`.
    pub fn locale() -> Option<(String, Option<String>)> {
        let locale = NSLocale::currentLocale();
        let language = locale.languageCode().to_string();
        // `regionCode` replaces this on macOS 13+, but the app supports 12.
        #[allow(deprecated)]
        let region = locale.countryCode().map(|code| code.to_string());
        Some((language, region))
    }
}
