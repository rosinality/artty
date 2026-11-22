//! A UTF-8 locale for panes. A server started without one (by an app over
//! ssh: GUI apps have no `LANG` to send, and a non-interactive login often
//! sets none) would start shells in the C locale, where typed Korean or an
//! accented letter shows as `???` or `<ffffffff>`. The terminal is always
//! UTF-8, so its programs should be too.

use std::sync::OnceLock;

/// What to add to panes' environment for a UTF-8 locale: nothing when the
/// server's own is one (or `LC_ALL` was set on purpose), else `LANG` (none
/// set) or `LC_CTYPE` (another `LANG` kept for messages and the rest).
pub(crate) fn pane_env() -> Option<(&'static str, String)> {
    static ENV: OnceLock<Option<(&'static str, String)>> = OnceLock::new();
    ENV.get_or_init(|| {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        let (all, ctype, lang) = (var("LC_ALL"), var("LC_CTYPE"), var("LANG"));
        // Most servers have one: no need to ask what's installed.
        if all.is_some() || ctype.as_deref().or(lang.as_deref()).is_some_and(is_utf8) {
            return None;
        }
        let installed = installed()?;
        let system = ["/etc/default/locale", "/etc/locale.conf"]
            .iter()
            .find_map(|path| system_lang(&std::fs::read_to_string(path).ok()?));
        let chosen = choose(
            all.as_deref(),
            ctype.as_deref(),
            lang.as_deref(),
            system.as_deref(),
            &installed,
        );
        if let Some((_, locale)) = &chosen {
            log::info!("panes get the locale {locale}: the server has no UTF-8 one");
        }
        chosen
    })
    .clone()
}

/// The locales `locale -a` lists; `None` if it can't be asked.
fn installed() -> Option<Vec<String>> {
    let output = std::process::Command::new("locale").arg("-a").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

/// `LANG` from `/etc/default/locale` or `/etc/locale.conf`.
fn system_lang(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.trim().strip_prefix("LANG=")?.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn is_utf8(locale: &str) -> bool {
    let codeset = locale.split_once('.').map_or("", |(_, rest)| rest);
    let codeset = codeset.split('@').next().unwrap_or("").to_ascii_lowercase().replace('-', "");
    codeset == "utf8"
}

/// See [`pane_env`]; `installed` as `locale -a` lists them.
fn choose(
    all: Option<&str>,
    ctype: Option<&str>,
    lang: Option<&str>,
    system: Option<&str>,
    installed: &[String],
) -> Option<(&'static str, String)> {
    // LC_ALL overrides everything: someone chose it.
    if all.is_some() {
        return None;
    }
    if ctype.or(lang).is_some_and(is_utf8) {
        return None;
    }
    let key = |name: &str| name.to_ascii_lowercase().replace('-', "");
    let find = |wanted: &str| installed.iter().find(|name| key(name) == key(wanted)).cloned();
    // The same language and region as what's set (`ko_KR.EUC-KR` → `ko_KR.UTF-8`).
    let same = ctype.or(lang).map(|set| {
        let base = set.split(['.', '@']).next().unwrap_or(set);
        format!("{base}.UTF-8")
    });
    let locale = system
        .filter(|s| is_utf8(s))
        .and_then(find)
        .or_else(|| same.as_deref().and_then(find))
        .or_else(|| find("en_US.UTF-8"))
        .or_else(|| find("C.UTF-8"))?;
    Some((if lang.is_none() { "LANG" } else { "LC_CTYPE" }, locale))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.to_owned()).collect()
    }

    #[test]
    fn leaves_a_utf8_locale_alone() {
        let all = installed(&["C", "C.utf8", "en_US.utf8"]);
        assert_eq!(choose(None, None, Some("en_US.UTF-8"), None, &all), None);
        assert_eq!(choose(None, Some("ko_KR.utf8"), Some("C"), None, &all), None);
        assert_eq!(choose(Some("C"), None, None, None, &all), None);
    }

    #[test]
    fn picks_an_installed_utf8_locale() {
        // DevPod: nothing set, the system default installed.
        let all = installed(&["C", "C.utf8", "POSIX", "en_US.utf8"]);
        assert_eq!(
            choose(None, None, None, Some("en_US.UTF-8"), &all),
            Some(("LANG", "en_US.utf8".to_owned()))
        );
        // No system default: en_US, else C.
        assert_eq!(choose(None, None, None, None, &all), Some(("LANG", "en_US.utf8".to_owned())));
        let c_only = installed(&["C", "C.utf8", "POSIX"]);
        assert_eq!(choose(None, None, None, None, &c_only), Some(("LANG", "C.utf8".to_owned())));
        // A LANG of another codeset keeps its language, and itself.
        let korean = installed(&["C", "ko_KR.euckr", "ko_KR.utf8", "en_US.utf8"]);
        assert_eq!(
            choose(None, None, Some("ko_KR.EUC-KR"), None, &korean),
            Some(("LC_CTYPE", "ko_KR.utf8".to_owned()))
        );
        // Nothing UTF-8 there: nothing to do.
        assert_eq!(choose(None, None, None, None, &installed(&["C", "POSIX"])), None);
    }

    #[test]
    fn reads_the_system_default() {
        assert_eq!(system_lang("# comment\nLANG=\"en_GB.UTF-8\"\n"), Some("en_GB.UTF-8".into()));
        assert_eq!(system_lang("LANGUAGE=en\n"), None);
    }
}
