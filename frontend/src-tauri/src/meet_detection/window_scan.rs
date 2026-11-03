// Heuristic detection of an active Google Meet call via on-screen browser window titles.
//
// There is no public API to know "a Google Meet call is active" — this reads window titles
// via CGWindowListCopyWindowInfo (Quartz Window Services) and pattern-matches them. It can
// both miss real calls (e.g. a browser that doesn't put the call state in the title) and
// false-positive (e.g. a Meet tab left open on the pre-join lobby). Treat it as a best-effort
// signal, not a guarantee — this is why it's gated behind an opt-in settings toggle.

#[cfg(target_os = "macos")]
const BROWSER_OWNER_NAMES: &[&str] = &[
    "Google Chrome",
    "Safari",
    "Firefox",
    "Microsoft Edge",
    "Arc",
    "Brave Browser",
];

#[cfg(target_os = "macos")]
fn looks_like_meet_call(title: &str) -> bool {
    // Observed live: Chrome titles an active Meet call "Meet – <call-code> 🔊" (en-dash, not a
    // hyphen). Accept known separator variants without matching unrelated titles such as
    // "Meeting notes - Google Docs".
    let normalized = title.trim().to_lowercase();
    let Some(suffix) = normalized.strip_prefix("meet") else {
        return false;
    };
    suffix.starts_with(" – ") || suffix.starts_with(" - ") || suffix.starts_with(": ")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::looks_like_meet_call;

    #[test]
    fn recognizes_active_meet_title_variants() {
        assert!(looks_like_meet_call("Meet – abc-defg-hij 🔊"));
        assert!(looks_like_meet_call("Meet - Weekly Sync"));
        assert!(looks_like_meet_call("meet: Product Team"));
    }

    #[test]
    fn ignores_unrelated_browser_titles() {
        assert!(!looks_like_meet_call("Google Calendar"));
        assert!(!looks_like_meet_call("Meeting notes - Google Docs"));
        assert!(!looks_like_meet_call("Zoom Workplace"));
    }
}

/// Returns `true` if a browser window's title currently looks like an active Google Meet call.
/// Requires the real macOS Screen Recording permission (not the Core Audio "Audio Capture"
/// permission this app already requests elsewhere) — without it, CGWindowListCopyWindowInfo
/// returns window entries with no `kCGWindowName`, so this silently sees nothing and returns
/// `false` rather than erroring.
#[cfg(target_os = "macos")]
pub fn scan_for_active_meet_call() -> bool {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::window::{
        kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
        kCGWindowName, kCGWindowOwnerName, CGWindowListCopyWindowInfo,
    };
    use std::os::raw::c_void;

    // Reads a CFString value out of an untyped (void*, void*) dictionary by a known CFStringRef
    // key. Dictionary entries from CGWindowListCopyWindowInfo aren't independently retained, so
    // any string pulled out is re-wrapped with `wrap_under_get_rule` (borrow + retain) rather
    // than treated as owned.
    unsafe fn string_value(
        dict: &CFDictionary<*const c_void, *const c_void>,
        key: CFStringRef,
    ) -> Option<String> {
        dict.find(key as *const c_void)
            .map(|value_ptr| CFString::wrap_under_get_rule(*value_ptr as CFStringRef).to_string())
    }

    let array_ref = unsafe {
        CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID,
        )
    };
    if array_ref.is_null() {
        return false;
    }
    let windows: CFArray<CFType> = unsafe { TCFType::wrap_under_create_rule(array_ref) };

    for item in windows.iter() {
        let Some(dict) = item.downcast::<CFDictionary<*const c_void, *const c_void>>() else {
            continue;
        };

        let owner_name = unsafe { string_value(&dict, kCGWindowOwnerName) };
        let Some(owner_name) = owner_name else {
            continue;
        };
        if !BROWSER_OWNER_NAMES.iter().any(|b| *b == owner_name) {
            continue;
        }

        let window_title = unsafe { string_value(&dict, kCGWindowName) };
        log::debug!(
            "[meet-detect] browser window: owner={:?} title={:?}",
            owner_name,
            window_title
        );
        if let Some(title) = window_title {
            if looks_like_meet_call(&title) {
                return true;
            }
        }
    }

    false
}

#[cfg(not(target_os = "macos"))]
pub fn scan_for_active_meet_call() -> bool {
    // Auto-detect is macOS-only in this iteration; the settings toggle is a no-op elsewhere.
    false
}
