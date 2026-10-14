//! The app's two languages, and how one of them is chosen.

/// The two languages this app ships. Anything else maps onto [`EN`].
pub const EN: &str = "en";
pub const ZH: &str = "zh-CN";

/// The system's language, as one of [`EN`] or [`ZH`].
pub fn system_language() -> &'static str {
    match os_language().or_else(sys_locale::get_locale) {
        Some(tag) if tag.to_ascii_lowercase().starts_with("zh") => ZH,
        _ => EN,
    }
}

/// The system's preferred language, from the OS itself.
#[cfg(target_os = "macos")]
fn os_language() -> Option<String> {
    use std::ffi::{CStr, c_char, c_void};

    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFPreferencesAnyApplication: *const c_void;
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *const c_void;
        fn CFPreferencesCopyAppValue(
            key: *const c_void,
            application: *const c_void,
        ) -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> u8;
        fn CFRelease(cf: *const c_void);
    }

    // SAFETY: the CoreFoundation C API, on the main thread. `key` and `languages`
    // are ours to release (Copy rule); the array's element is not (Get rule), and
    // it is read before the array is released.
    unsafe {
        let key = CFStringCreateWithCString(std::ptr::null(), c"AppleLanguages".as_ptr(), UTF8);
        if key.is_null() {
            return None;
        }
        let languages = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
        CFRelease(key);
        let language = (!languages.is_null() && CFArrayGetCount(languages) > 0).then(|| {
            let first = CFArrayGetValueAtIndex(languages, 0);
            let mut buffer = [0 as c_char; 64];
            (CFStringGetCString(first, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) != 0).then(
                || {
                    CStr::from_ptr(buffer.as_ptr())
                        .to_string_lossy()
                        .into_owned()
                },
            )
        });
        if !languages.is_null() {
            CFRelease(languages);
        }
        language.flatten()
    }
}

#[cfg(not(target_os = "macos"))]
fn os_language() -> Option<String> {
    None
}

/// The language for a `settings.toml` value: the named one when it is one this
/// app ships, and the system's otherwise. An empty or unknown value is not an
/// error — a hand-edited file should still open.
pub fn resolve(setting: Option<&str>) -> &'static str {
    match setting.map(str::trim) {
        Some(tag) if tag.eq_ignore_ascii_case(ZH) || tag.to_ascii_lowercase().starts_with("zh") => {
            ZH
        }
        Some(tag) if tag.eq_ignore_ascii_case(EN) => EN,
        _ => system_language(),
    }
}

/// Make the app speak `settings.language`. Called at start-up and when the
/// settings form changes it; the caller redraws.
pub fn apply(setting: Option<&str>) {
    rust_i18n::set_locale(resolve(setting));
}

#[cfg(test)]
mod tests {
    use super::{EN, ZH, resolve};
    use rust_i18n::t;

    #[test]
    fn a_named_language_is_the_one_that_was_named() {
        assert_eq!(resolve(Some("en")), EN);
        assert_eq!(resolve(Some("zh-CN")), ZH);
        assert_eq!(resolve(Some("zh-Hans-CN")), ZH);
        assert_eq!(resolve(Some("zh-TW")), ZH);
    }

    #[test]
    fn an_absent_or_unknown_language_follows_the_system() {
        // The only assertion that holds on every machine: the answer is one of
        // the two languages this app ships, and neither path panics.
        let system = super::system_language();
        assert!(system == EN || system == ZH);
        assert_eq!(resolve(None), system);
        assert_eq!(resolve(Some("")), system);
        assert_eq!(resolve(Some("klingon")), system);
    }

    /// The file was found and parsed: a `t!` whose key is not in it answers with
    /// the key, not the sentence.
    #[test]
    fn both_locales_are_loaded() {
        assert_eq!(t!("settings.language", locale = "en").as_ref(), "Language");
        assert_eq!(t!("settings.language", locale = "zh-CN").as_ref(), "语言");
    }

    /// The sidebar's status column is narrow, so what it draws is the shorthand —
    /// and a key the file does not have would be drawn as the key.
    #[test]
    fn the_status_shorthands_are_in_both_locales() {
        for key in [
            "status.working_short",
            "status.idle_short",
            "status.waiting_short",
            "status.stalled_short",
            "status.done_short",
            "status.failed_short",
        ] {
            for locale in [EN, ZH] {
                assert_ne!(t!(key, locale = locale).as_ref(), key, "{key} in {locale}");
            }
        }
    }

    /// The column names over a worktree's terminal rows are words too: a key the
    /// file does not have would be drawn as the key.
    #[test]
    fn the_column_names_are_in_both_locales() {
        for key in [
            "resource.column_status",
            "resource.column_cpu",
            "resource.column_rss",
            "resource.column_uptime",
        ] {
            for locale in [EN, ZH] {
                assert_ne!(t!(key, locale = locale).as_ref(), key, "{key} in {locale}");
            }
        }
    }

    /// Finder-launched apps have no `LANG`, so this is the path that has to work
    /// on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_a_preferred_language() {
        assert!(
            super::os_language().is_some_and(|tag| !tag.is_empty()),
            "CoreFoundation reported no AppleLanguages"
        );
    }
}
