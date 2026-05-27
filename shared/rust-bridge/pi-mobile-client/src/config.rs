//! Mobile-side configuration builder for the in-process pi runtime.
//!
//! Resolves pi's [`Config::global_dir()`], working directory, and TLS
//! environment so that pi behaves correctly inside the iOS/Android
//! sandbox. The output is a [`PiSandboxConfig`] holding the values the
//! `pi-mobile-client` runtime needs to pass into pi's session builder
//! (working dir + envs) and the [`pi::sdk::Config`] pi loads internally.
//!
//! Today only the iOS branch is exercised by tests; the Android branch
//! mirrors the same shape and is filled in by a sibling feature.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Which mobile platform we are building a sandbox config for.
///
/// Injected by callers (rather than being inferred from `cfg!`) so the
/// builder is unit-testable on the host.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobilePlatform {
    Ios,
    Android,
}

/// Inputs the host platform must supply when building a sandbox config.
///
/// On iOS these are derived from `FileManager.default.urls(for:in:)`
/// and `Bundle.main.url(forResource:"cacert", withExtension:"pem")`.
/// On Android they come from `Context.getFilesDir()` and an extracted
/// `assets/cacert.pem`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SandboxInputs {
    pub platform: MobilePlatform,
    /// User home dir as the platform sees it. On iOS this is the value
    /// of `NSHomeDirectory()` (a.k.a. the app container root). On
    /// Android it is the app's filesDir parent.
    pub home_dir: PathBuf,
    /// Absolute path to a bundled `cacert.pem` file. Exported via
    /// `SSL_CERT_FILE` so pi's reqwest stack (and any transitive TLS
    /// consumer) trusts the system root set we ship with the app.
    pub cacert_pem_path: PathBuf,
}

/// Resolved sandbox configuration the in-process runtime hands to pi.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PiSandboxConfig {
    /// Value pi will see as `Config::global_dir()`. Exposed both as a
    /// path (for direct callers) and via the `PI_CODING_AGENT_DIR`
    /// environment variable (pi reads it through
    /// `global_dir_from_env`).
    pub global_dir: PathBuf,
    /// Working directory the agent session opens in. The agent's
    /// `cwd`-relative file tools (read/write/edit/grep/find/ls) all
    /// operate under this path.
    pub working_dir: PathBuf,
    /// Absolute path to the bundled `cacert.pem` file, surfaced as the
    /// `SSL_CERT_FILE` env var below.
    pub cacert_pem_path: PathBuf,
    /// Environment variables that must be exported into the process
    /// before pi loads its TLS stack.
    ///
    /// On iOS this is at minimum `SSL_CERT_FILE` (bundled `cacert.pem`)
    /// and `PI_CODING_AGENT_DIR` (so pi's `Config::global_dir()`
    /// resolves to `~/Library/Application Support/pi/` inside the
    /// sandbox).
    pub env: HashMap<String, String>,
}

/// Build a [`PiSandboxConfig`] for the supplied platform inputs.
///
/// iOS layout (matches the architecture doc):
///   * `global_dir` -> `~/Library/Application Support/pi/`
///   * `working_dir` -> `~/Documents/home/pi/`
///   * `SSL_CERT_FILE` -> `inputs.cacert_pem_path`
///
/// The function does not create the directories; the caller is
/// responsible for `fs::create_dir_all` at app startup. We only resolve
/// paths here so the builder stays pure and unit-testable.
#[allow(dead_code)]
pub fn build_sandbox_config(inputs: SandboxInputs) -> PiSandboxConfig {
    match inputs.platform {
        MobilePlatform::Ios => build_ios(inputs),
        MobilePlatform::Android => build_android(inputs),
    }
}

#[allow(dead_code)]
fn build_ios(inputs: SandboxInputs) -> PiSandboxConfig {
    // iOS sandbox paths. Strings are intentionally exact so the
    // VAL-IOS-PI-010 grep ("Application Support/pi") matches.
    let global_dir: PathBuf = inputs.home_dir.join("Library/Application Support/pi/");
    let working_dir: PathBuf = inputs.home_dir.join("Documents/home/pi/");

    let mut env = HashMap::new();
    // pi reads PI_CODING_AGENT_DIR via Config::global_dir; pinning the
    // value here keeps pi from falling back to dirs::home_dir() (which
    // points into the iOS sandbox in unpredictable ways).
    env.insert(
        "PI_CODING_AGENT_DIR".to_string(),
        path_to_str_lossy(&global_dir),
    );
    // SSL_CERT_FILE points at the bundled cacert.pem so pi's reqwest
    // stack trusts the system roots we ship.
    env.insert(
        "SSL_CERT_FILE".to_string(),
        path_to_str_lossy(&inputs.cacert_pem_path),
    );

    PiSandboxConfig {
        global_dir,
        working_dir,
        cacert_pem_path: inputs.cacert_pem_path,
        env,
    }
}

#[allow(dead_code)]
fn build_android(inputs: SandboxInputs) -> PiSandboxConfig {
    // Android: <filesDir>/pi and <filesDir>/home/pi. The mobile
    // platform passes home_dir = filesDir (its parent for iOS is
    // NSHomeDirectory()).
    let global_dir: PathBuf = inputs.home_dir.join("pi/");
    let working_dir: PathBuf = inputs.home_dir.join("home/pi/");

    let mut env = HashMap::new();
    env.insert(
        "PI_CODING_AGENT_DIR".to_string(),
        path_to_str_lossy(&global_dir),
    );
    env.insert(
        "SSL_CERT_FILE".to_string(),
        path_to_str_lossy(&inputs.cacert_pem_path),
    );

    PiSandboxConfig {
        global_dir,
        working_dir,
        cacert_pem_path: inputs.cacert_pem_path,
        env,
    }
}

#[allow(dead_code)]
fn path_to_str_lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ios_sandbox_config_paths() {
        let home = PathBuf::from("/var/mobile/Containers/Data/Application/TEST");
        let cacert = PathBuf::from("/var/mobile/.../Litter.app/cacert.pem");

        let cfg = build_sandbox_config(SandboxInputs {
            platform: MobilePlatform::Ios,
            home_dir: home.clone(),
            cacert_pem_path: cacert.clone(),
        });

        assert_eq!(
            cfg.global_dir,
            home.join("Library/Application Support/pi/"),
            "iOS global_dir must point at Application Support/pi/"
        );
        assert_eq!(
            cfg.working_dir,
            home.join("Documents/home/pi/"),
            "iOS working_dir must point at Documents/home/pi/"
        );

        // SSL_CERT_FILE must be exported and point at the bundled
        // cacert.pem the caller supplied.
        let ssl = cfg
            .env
            .get("SSL_CERT_FILE")
            .expect("SSL_CERT_FILE exported");
        assert_eq!(ssl, &cacert.to_string_lossy().into_owned());

        // PI_CODING_AGENT_DIR mirrors global_dir so pi's loader picks it up.
        let pi_dir = cfg
            .env
            .get("PI_CODING_AGENT_DIR")
            .expect("PI_CODING_AGENT_DIR exported");
        assert_eq!(pi_dir, &cfg.global_dir.to_string_lossy().into_owned());

        assert_eq!(cfg.cacert_pem_path, cacert);
    }

    #[test]
    fn android_global_dir_under_files_dir() {
        let files_dir = PathBuf::from("/data/user/0/com.sigkitten.litter.android/files");
        let cacert = files_dir.join("cacert.pem");

        let cfg = build_sandbox_config(SandboxInputs {
            platform: MobilePlatform::Android,
            home_dir: files_dir.clone(),
            cacert_pem_path: cacert,
        });

        assert_eq!(
            cfg.global_dir,
            files_dir.join("pi/"),
            "Android global_dir must point at <filesDir>/pi/"
        );
        assert_eq!(
            cfg.working_dir,
            files_dir.join("home/pi/"),
            "Android working_dir must point at <filesDir>/home/pi/"
        );

        // Both resolved paths must start with the injected filesDir prefix.
        assert!(
            cfg.global_dir.starts_with(&files_dir),
            "global_dir must start with injected filesDir"
        );
        assert!(
            cfg.working_dir.starts_with(&files_dir),
            "working_dir must start with injected filesDir"
        );
    }

    #[test]
    fn android_ssl_cert_file_set() {
        let files_dir = PathBuf::from("/data/user/0/com.sigkitten.litter.android/files");
        let cacert = PathBuf::from("/data/.../assets/cacert.pem");

        let cfg = build_sandbox_config(SandboxInputs {
            platform: MobilePlatform::Android,
            home_dir: files_dir,
            cacert_pem_path: cacert.clone(),
        });

        let ssl = cfg
            .env
            .get("SSL_CERT_FILE")
            .expect("SSL_CERT_FILE exported on Android");
        assert_eq!(
            ssl,
            &cacert.to_string_lossy().into_owned(),
            "SSL_CERT_FILE must point at caller-provided cacert.pem"
        );
        assert_eq!(cfg.cacert_pem_path, cacert);

        let pi_dir = cfg
            .env
            .get("PI_CODING_AGENT_DIR")
            .expect("PI_CODING_AGENT_DIR exported on Android");
        assert_eq!(pi_dir, &cfg.global_dir.to_string_lossy().into_owned());
    }
}
