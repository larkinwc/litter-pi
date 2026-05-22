//! System-prompt preamble fragments injected into the pi agent on each
//! mobile platform.
//!
//! These strings get appended to pi's system prompt (via the session
//! `append_system_prompt` knob) so the model has accurate context about
//! the sandboxed runtime it is operating in. Per the architecture doc
//! the preamble must explain that the shell tool runs inside an Alpine
//! fakefs and that host filesystem paths are unreachable from the
//! tool surface.

/// Pi-on-iOS system preamble.
///
/// Tells the model:
/// * the shell tool executes inside an Alpine Linux **fakefs** running
///   under the iSH user-mode x86 emulator,
/// * the working directory is `/root` inside that fakefs,
/// * host paths (`~/Documents/...`, `/var/mobile/...`) are NOT visible
///   from the shell tool and must not be passed to it.
pub const IOS_PI_PREAMBLE: &str = concat!(
    "You are running inside Litter on iOS. The shell tool executes commands ",
    "inside an Alpine Linux fakefs hosted by the iSH user-mode x86 emulator. ",
    "The shell starts in /root and only sees the Alpine fakefs filesystem: ",
    "host iOS paths (for example ~/Documents/... or /var/mobile/...) are NOT ",
    "reachable from the shell tool. Do not pass host filesystem paths to the ",
    "shell tool; instead, work entirely within the Alpine fakefs rooted at /root.",
);

/// Pi-on-Android system preamble.
///
/// Symmetric placeholder for the Android branch; filled out alongside
/// the `ProotToolFactory` feature. Keeping it here (rather than in a
/// separate file) means both platforms share the same preamble policy
/// surface.
pub const ANDROID_PI_PREAMBLE: &str = concat!(
    "You are running inside Litter on Android. The shell tool executes ",
    "commands inside a proot Linux environment rooted at the app's private ",
    "storage. Host Android paths outside the proot root are not reachable.",
);

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_mentions_alpine_fakefs_root() {
        // VAL-IOS-PI-009: the iOS-pi preamble must mention Alpine, the
        // fakefs concept, and the /root cwd so the model knows what
        // environment it is operating in.
        assert!(
            IOS_PI_PREAMBLE.contains("Alpine"),
            "iOS pi preamble must mention Alpine: {IOS_PI_PREAMBLE}"
        );
        assert!(
            IOS_PI_PREAMBLE.contains("fakefs"),
            "iOS pi preamble must mention fakefs: {IOS_PI_PREAMBLE}"
        );
        assert!(
            IOS_PI_PREAMBLE.contains("/root"),
            "iOS pi preamble must mention /root cwd: {IOS_PI_PREAMBLE}"
        );
        // The no-host-path warning is essential; assert at least one of
        // the obvious markers is present.
        assert!(
            IOS_PI_PREAMBLE.contains("NOT") || IOS_PI_PREAMBLE.contains("not reachable"),
            "iOS pi preamble must warn that host paths are unreachable: {IOS_PI_PREAMBLE}"
        );
    }
}
