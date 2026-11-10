import Foundation
import Testing
@testable import AIMemoryMenuCore

struct AppSettingsTests {
    private func url(_ string: String) -> URL {
        URL(string: string)!
    }

    private func origin(_ string: String) -> ServerOrigin {
        ServerOrigin.of(URL(string: string)!)!
    }

    @Test func loadAndSaveRoundTrip() throws {
        let name = "ai-memory-menu-settings-test-\(UUID().uuidString)"
        let suite = try #require(UserDefaults(suiteName: name))
        defer { suite.removePersistentDomain(forName: name) }

        var settings = AppSettings.load(defaults: suite)
        #expect(settings.serverURL == AppSettings.defaultServerURL)
        #expect(settings.dataDirOverride == nil)
        #expect(settings.tokenOrigin == nil)

        settings.serverURL = URL(string: "http://127.0.0.1:8080")!
        settings.dataDirOverride = URL(fileURLWithPath: "/tmp/custom-memory")
        settings.tokenOrigin = origin("http://127.0.0.1:8080")
        settings.save(defaults: suite)

        let loaded = AppSettings.load(defaults: suite)
        #expect(loaded.serverURL.absoluteString == "http://127.0.0.1:8080")
        #expect(loaded.dataDirOverride?.path == "/tmp/custom-memory")
        #expect(loaded.tokenOrigin == origin("http://127.0.0.1:8080"))
    }

    @Test func changingOnlyTheURLDoesNotCarryTheToken() throws {
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )
        settings.applyEdit(serverURLString: "https://b.example.com", dataDirOverride: "", tokenInEdit: nil)
        #expect(settings.serverURL.absoluteString == "https://b.example.com")
        #expect(settings.tokenOrigin == origin("https://a.example.com"))

        // Switching back to the bound server finds the token again.
        settings.applyEdit(serverURLString: "https://a.example.com", dataDirOverride: "", tokenInEdit: nil)
        #expect(settings.tokenOrigin == origin("https://a.example.com"))
    }

    @Test func aTokenTypedWithANewURLBindsToThatURL() throws {
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )
        settings.applyEdit(
            serverURLString: "https://b.example.com",
            dataDirOverride: "",
            tokenInEdit: "a-new-token"
        )
        #expect(settings.serverURL.absoluteString == "https://b.example.com")
        #expect(settings.tokenOrigin == origin("https://b.example.com"))
    }

    @Test func clearingForgetsTheBinding() throws {
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )
        settings.clearTokenBinding()
        #expect(settings.tokenOrigin == nil)
    }

    @Test func anUnusableURLLeavesTheServerAndTokenAlone() throws {
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )
        // A non-http(s) URL is not a server origin, so the server stays put.
        settings.applyEdit(serverURLString: "ftp://host", dataDirOverride: "", tokenInEdit: "x")
        #expect(settings.serverURL.absoluteString == "https://a.example.com")
        #expect(settings.tokenOrigin == origin("https://a.example.com"))
    }

    @Test func anUnboundTokenFromAnOlderVersionStaysWithTheConfiguredServer() throws {
        var settings = AppSettings(serverURL: url("https://a.example.com"), tokenOrigin: nil)
        settings.adoptUnboundToken()
        #expect(settings.tokenOrigin == origin("https://a.example.com"))

        // Once bound, a later URL change does not re-adopt it onto the new host.
        settings.serverURL = url("https://b.example.com")
        settings.adoptUnboundToken()
        #expect(settings.tokenOrigin == origin("https://a.example.com"))
    }
}
