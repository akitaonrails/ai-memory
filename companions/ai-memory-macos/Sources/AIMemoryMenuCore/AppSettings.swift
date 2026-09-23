import Foundation

public struct AppSettings: Equatable, Sendable {
    public var serverURL: URL
    public var dataDirOverride: URL?

    public static let defaultServerURL = URL(string: "http://127.0.0.1:49374")!

    public init(serverURL: URL = defaultServerURL, dataDirOverride: URL? = nil) {
        self.serverURL = serverURL
        self.dataDirOverride = dataDirOverride
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
        return AppSettings(serverURL: url, dataDirOverride: override)
    }

    public func save(defaults: UserDefaults = .standard) {
        defaults.set(serverURL.absoluteString, forKey: Keys.serverURL)
        if let dataDirOverride {
            defaults.set(dataDirOverride.path, forKey: Keys.dataDir)
        } else {
            defaults.removeObject(forKey: Keys.dataDir)
        }
    }

    private enum Keys {
        static let serverURL = "serverURL"
        static let dataDir = "dataDirOverride"
    }
}
