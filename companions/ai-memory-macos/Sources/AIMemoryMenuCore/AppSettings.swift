import Foundation

public struct AppSettings: Equatable, Sendable {
    public var serverURL: URL
    public var dataDirOverride: URL?
    /// The server origin the Keychain bearer was saved for. The token is sent
    /// only when this matches the current server, so a later URL change does
    /// not carry it to another host. `nil` means no bound token.
    public var tokenOrigin: ServerOrigin?

    public static let defaultServerURL = URL(string: "http://127.0.0.1:49374")!

    public init(
        serverURL: URL = defaultServerURL,
        dataDirOverride: URL? = nil,
        tokenOrigin: ServerOrigin? = nil
    ) {
        self.serverURL = serverURL
        self.dataDirOverride = dataDirOverride
        self.tokenOrigin = tokenOrigin
    }

    public var resolvedDataDir: URL {
        dataDirOverride ?? FirstRun.defaultDataDir()
    }

    public static func load(defaults: UserDefaults = .standard) -> AppSettings {
        let url: URL
        if let stored = defaults.string(forKey: Keys.serverURL),
           let parsed = URL(string: stored)
        {
            url = parsed
        } else {
            url = defaultServerURL
        }
        let override: URL?
        if let path = defaults.string(forKey: Keys.dataDir), !path.isEmpty {
            override = URL(fileURLWithPath: path)
        } else {
            override = nil
        }
        let tokenOrigin = defaults.string(forKey: Keys.tokenOrigin)
            .flatMap { ServerOrigin.of(canonical: $0) }
        return AppSettings(serverURL: url, dataDirOverride: override, tokenOrigin: tokenOrigin)
    }

    public func save(defaults: UserDefaults = .standard) {
        defaults.set(serverURL.absoluteString, forKey: Keys.serverURL)
        if let dataDirOverride {
            defaults.set(dataDirOverride.path, forKey: Keys.dataDir)
        } else {
            defaults.removeObject(forKey: Keys.dataDir)
        }
        if let tokenOrigin {
            defaults.set(tokenOrigin.canonical, forKey: Keys.tokenOrigin)
        } else {
            defaults.removeObject(forKey: Keys.tokenOrigin)
        }
    }

    /// Apply an edit from Settings. A usable new URL updates the server; a
    /// non-empty token in the edit binds to that URL's origin; a URL-only edit
    /// leaves the token bound to its old server, so switching the URL back
    /// finds it again. A URL that is not `http(s)` with a host leaves the
    /// server, and therefore the token binding, unchanged.
    public mutating func applyEdit(
        serverURLString: String,
        dataDirOverride: String,
        tokenInEdit: String?
    ) {
        if let url = URL(string: serverURLString), ServerOrigin.of(url) != nil {
            serverURL = url
        }
        let trimmed = dataDirOverride.trimmingCharacters(in: .whitespacesAndNewlines)
        self.dataDirOverride = trimmed.isEmpty ? nil : URL(fileURLWithPath: trimmed)
        if let tokenInEdit,
           !tokenInEdit.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            tokenOrigin = ServerOrigin.of(serverURL)
        }
    }

    /// Bind a token saved by an earlier version, which recorded no origin, to
    /// the server configured at first launch — where it was already being
    /// sent — so existing installs keep working and only a later URL change
    /// stops carrying it.
    public mutating func adoptUnboundToken() {
        if tokenOrigin == nil {
            tokenOrigin = ServerOrigin.of(serverURL)
        }
    }

    /// Forget the token binding when the saved token is cleared.
    public mutating func clearTokenBinding() {
        tokenOrigin = nil
    }

    /// Commit a token edit to `store` and this settings' binding together, so a
    /// failed keychain write never leaves a retained bearer the app would still
    /// send. Call it after ``applyEdit(serverURLString:dataDirOverride:tokenInEdit:)``
    /// has set the new server and (for a non-empty token) the matching binding.
    ///
    /// On a failed `set`, the old token is still in the keychain while the
    /// binding now points at the configured server, so ``DataDirToken/effective``
    /// would send that stale bearer and ``adoptUnboundToken()`` would re-bind it
    /// on the next launch. This clears both the keychain token and the binding
    /// so nothing stale is sent or re-adopted. A failed `clear` leaves the token
    /// and its existing binding untouched (it stays exactly as before the edit).
    /// Returns the keychain error, or `nil` on success or a no-op.
    @discardableResult
    public mutating func commitTokenEdit(
        clearToken: Bool,
        token: String?,
        to store: any TokenStore
    ) -> Error? {
        if clearToken {
            do {
                try store.clear()
                clearTokenBinding()
                return nil
            } catch {
                return error
            }
        }
        guard let token else { return nil }
        let value = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return nil }
        do {
            try store.save(value)
            return nil
        } catch {
            try? store.clear()
            clearTokenBinding()
            return error
        }
    }

    private enum Keys {
        static let serverURL = "serverURL"
        static let dataDir = "dataDirOverride"
        static let tokenOrigin = "tokenOrigin"
    }
}
