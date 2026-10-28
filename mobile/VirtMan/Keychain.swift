import Foundation
import Security

/// API keys, one per stored server. The keychain is the right place, but an
/// unsigned app inside LiveContainer may run without a keychain entitlement
/// (`errSecMissingEntitlement`), so a successfully saved key is also readable
/// from a protected file in Application Support.
///
/// shortcut: the file fallback protects the key with the app sandbox and
/// data-protection class, not the keychain; upgrade if LiveContainer stops
/// denying keychain access.
enum SecretStore {
    private static let service = "top.kemoi.virtman.apikey"

    static func save(_ value: String, account: String) {
        delete(account: account)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecValueData as String: Data(value.utf8),
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecAttrSynchronizable as String: kCFBooleanFalse as Any,
        ]
        let status = SecItemAdd(query as CFDictionary, nil)
        if status != errSecSuccess {
            writeFile(value, account: account)
        } else {
            removeFile(account: account)
        }
    }

    static func load(account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        if SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess,
           let data = item as? Data,
           let value = String(data: data, encoding: .utf8) {
            return value
        }
        return readFile(account: account)
    }

    static func delete(account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
        removeFile(account: account)
    }

    // MARK: - Protected-file fallback

    private static func directory() -> URL? {
        guard let root = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
        else { return nil }
        let dir = root.appendingPathComponent("VirtMan/keys", isDirectory: true)
        try? FileManager.default.createDirectory(
            at: dir, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication]
        )
        return dir
    }

    private static func fileURL(account: String) -> URL? {
        directory()?.appendingPathComponent(account)
    }

    private static func writeFile(_ value: String, account: String) {
        guard let url = fileURL(account: account) else { return }
        try? FileManager.default.removeItem(at: url)
        FileManager.default.createFile(
            atPath: url.path,
            contents: Data(value.utf8),
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication]
        )
    }

    private static func readFile(account: String) -> String? {
        guard let url = fileURL(account: account) else { return nil }
        return try? String(contentsOf: url, encoding: .utf8)
    }

    private static func removeFile(account: String) {
        guard let url = fileURL(account: account) else { return }
        try? FileManager.default.removeItem(at: url)
    }
}
