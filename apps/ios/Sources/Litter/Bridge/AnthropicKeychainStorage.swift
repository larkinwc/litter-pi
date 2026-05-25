import Foundation
import Security

/// Keychain wrapper for the Anthropic OAuth refresh token used by the
/// in-process pi runtime.
///
/// VAL-AUTH-004 requires the iOS Swift bridge to persist the OAuth
/// refresh token through the iOS Keychain (`SecItemAdd` /
/// `kSecClassGenericPassword`) rather than `UserDefaults` or any other
/// plaintext store. This wrapper is the single place that touches
/// `Security`-framework APIs for the Anthropic OAuth credential and is
/// invoked from `AnthropicOAuthBridge` whenever the Rust
/// `AuthStorage` would otherwise need a platform-supplied refresh token
/// (e.g. after a successful PKCE handshake or on refresh).
///
/// The service identifier mirrors the bundle id used by the rest of the
/// app (`com.sigkitten.litter`) so the stored entry is namespaced under
/// the host application and trivially discoverable in Keychain Access
/// during debugging.
enum AnthropicKeychainStorageError: Error, LocalizedError {
    case unexpectedItemFormat
    case status(OSStatus)

    var errorDescription: String? {
        switch self {
        case .unexpectedItemFormat:
            return "Keychain item did not contain a UTF-8 refresh token blob."
        case .status(let status):
            return "Keychain operation failed (OSStatus=\(status))."
        }
    }
}

/// Thin wrapper around `SecItem*` for the Anthropic OAuth refresh
/// token. Synchronous because every call hops directly into the
/// `Security` framework; callers that need to avoid blocking the main
/// thread should dispatch to a background queue.
final class AnthropicKeychainStorage {
    /// Shared instance the bridge uses by default. Tests can construct
    /// their own `AnthropicKeychainStorage(service:account:)` against a
    /// throwaway service id to keep the system keychain clean.
    static let shared = AnthropicKeychainStorage()

    /// `kSecAttrService` value pinned by VAL-AUTH-004 evidence. Keep in
    /// sync with the Android `MasterKey` alias documented in
    /// `architecture.md` so cross-platform debugging can correlate
    /// stored credentials.
    static let defaultService = "com.sigkitten.litter.pi.oauth"

    private let service: String
    private let account: String
    private let accessibility: CFString

    init(
        service: String = AnthropicKeychainStorage.defaultService,
        account: String = "refresh-token",
        accessibility: CFString = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
    ) {
        self.service = service
        self.account = account
        self.accessibility = accessibility
    }

    /// Persist `refreshToken`. Uses `SecItemAdd` on first write and
    /// falls back to `SecItemUpdate` if the entry already exists so
    /// repeated calls (e.g. silent refresh) do not produce
    /// `errSecDuplicateItem`.
    func saveRefreshToken(_ refreshToken: String) throws {
        let data = Data(refreshToken.utf8)
        let baseQuery = self.baseQuery()
        let addAttributes: [String: Any] = baseQuery.merging([
            kSecAttrAccessible as String: accessibility,
            kSecValueData as String: data
        ]) { _, new in new }

        let addStatus = SecItemAdd(addAttributes as CFDictionary, nil)
        switch addStatus {
        case errSecSuccess:
            return
        case errSecDuplicateItem:
            let updateAttributes: [String: Any] = [
                kSecAttrAccessible as String: accessibility,
                kSecValueData as String: data
            ]
            let updateStatus = SecItemUpdate(
                baseQuery as CFDictionary,
                updateAttributes as CFDictionary
            )
            guard updateStatus == errSecSuccess else {
                throw AnthropicKeychainStorageError.status(updateStatus)
            }
        default:
            throw AnthropicKeychainStorageError.status(addStatus)
        }
    }

    /// Fetch the stored refresh token if any. Returns `nil` when no
    /// entry exists (the standard "user has not signed in yet" state).
    func loadRefreshToken() throws -> String? {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch status {
        case errSecSuccess:
            guard let data = item as? Data,
                  let value = String(data: data, encoding: .utf8) else {
                throw AnthropicKeychainStorageError.unexpectedItemFormat
            }
            return value
        case errSecItemNotFound:
            return nil
        default:
            throw AnthropicKeychainStorageError.status(status)
        }
    }

    /// Remove the stored refresh token. Idempotent — a missing entry
    /// is not treated as an error since callers commonly invoke this
    /// during sign-out without first checking for presence.
    func deleteRefreshToken() throws {
        let status = SecItemDelete(baseQuery() as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw AnthropicKeychainStorageError.status(status)
        }
    }

    private func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account
        ]
    }
}
