import Foundation
import Testing
@testable import AIMemoryMenuCore

struct AppSettingsTests {
    @Test func loadAndSaveRoundTrip() throws {
        let name = "ai-memory-menu-settings-test-\(UUID().uuidString)"
        let suite = try #require(UserDefaults(suiteName: name))
        defer { suite.removePersistentDomain(forName: name) }

        var settings = AppSettings.load(defaults: suite)
        #expect(settings.serverURL == AppSettings.defaultServerURL)
        #expect(settings.dataDirOverride == nil)

        settings.serverURL = URL(string: "http://127.0.0.1:8080")!
        settings.dataDirOverride = URL(fileURLWithPath: "/tmp/custom-memory")
        settings.save(defaults: suite)

        let loaded = AppSettings.load(defaults: suite)
        #expect(loaded.serverURL.absoluteString == "http://127.0.0.1:8080")
        #expect(loaded.dataDirOverride?.path == "/tmp/custom-memory")
    }
}
