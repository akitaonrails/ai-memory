import Foundation
import Security

public protocol TokenStore: Sendable {
    func read() -> String?
    func save(_ token: String) throws
    func clear() throws
}

public struct MemoryTokenStore: TokenStore, Sendable {
    private let box: LockingBox<String?>

    public init(_ initial: String? = nil) {
        box = LockingBox(initial)
    }

    public func read() -> String? {
        box.value
    }

    public func save(_ token: String) throws {
        box.value = token
    }

    public func clear() throws {
        box.value = nil
    }
}

/// Tiny mutex so `MemoryTokenStore` can be `Sendable` in tests.
final class LockingBox<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: Value

    init(_ value: Value) {
        storage = value
    }

    var value: Value {
        get {
            lock.lock()
            defer { lock.unlock() }
            return storage
        }
        set {
            lock.lock()
            defer { lock.unlock() }
            storage = newValue
        }
    }
}

public struct KeychainTokenStore: TokenStore, Sendable {
    public static let service = "com.github.akitaonrails.ai-memory-menu"
    public static let account = "bearer"

    public init() {}

    public func read() -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: Self.account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess, let data = item as? Data else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    public func save(_ token: String) throws {
        try clear()
        let data = Data(token.utf8)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: Self.account,
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
        ]
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw KeychainError.unhandled(status)
        }
    }

    public func clear() throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: Self.account,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.unhandled(status)
        }
    }
}

public enum KeychainError: Error, Equatable, Sendable {
    case unhandled(OSStatus)
}

/// The root bearer `ai-memory init` writes on a fresh data dir
/// (`<data_dir>/auth-tokens/default`, owner-only). The app reads it when the
/// Keychain holds no token, so an install it initialized is authorized
/// without pasting anything; a token saved in Settings still wins.
public enum DataDirToken {
    public static func path(dataDir: URL) -> URL {
        dataDir.appending(path: "auth-tokens/default")
    }

    public static func read(dataDir: URL, fileManager: FileManager = .default) -> String? {
        guard let data = fileManager.contents(atPath: path(dataDir: dataDir).path),
              let text = String(data: data, encoding: .utf8)
        else {
            return nil
        }
        let token = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return token.isEmpty ? nil : token
    }

    /// The token to send to `serverURL`, bound to the server it belongs to.
    ///
    /// The saved Keychain token is returned only when its recorded origin is the
    /// server's origin, so changing the URL in Settings does not carry it to a
    /// different host. Otherwise the data dir's root bearer is used, but only for
    /// a loopback server (this Mac), because that file is the local server's
    /// token. A non-loopback server with no matching saved token receives
    /// nothing.
    public static func effective(
        saved: String?,
        savedOrigin: ServerOrigin?,
        serverURL: URL,
        dataDir: URL,
        fileManager: FileManager = .default
    ) -> String? {
        let target = ServerOrigin.of(serverURL)
        if let saved, !saved.isEmpty, let target, let savedOrigin, savedOrigin == target {
            return saved
        }
        if let target, target.isLoopback {
            return read(dataDir: dataDir, fileManager: fileManager)
        }
        return nil
    }
}
