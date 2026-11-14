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

    private let noDataDir = URL(string: "file:///nonexistent")!

    @Test func aFailedTokenSaveDoesNotLeaveTheOldBearerBoundOrResent() {
        // An old token is saved and bound; the user retargets the app at a new
        // server and enters a new token, so applyEdit binds the new origin.
        let store = FakeTokenStore(value: "old-secret")
        store.failSave = true
        var settings = AppSettings(
            serverURL: url("https://old.example.com"),
            tokenOrigin: origin("https://old.example.com")
        )
        settings.applyEdit(
            serverURLString: "https://new.example.com",
            dataDirOverride: "",
            tokenInEdit: "new-secret"
        )

        let err = settings.commitTokenEdit(clearToken: false, token: "new-secret", to: store)

        #expect(err != nil) // the failure is reported, not swallowed
        // The retained old token is gone and unbound, so nothing stale is sent
        // to the new server...
        #expect(store.read() == nil)
        #expect(settings.tokenOrigin == nil)
        #expect(
            DataDirToken.effective(
                saved: store.read(),
                savedOrigin: settings.tokenOrigin,
                serverURL: url("https://new.example.com"),
                dataDir: noDataDir
            ) == nil
        )
        // ...and a later launch does not re-adopt it. AppModel.init only calls
        // adoptUnboundToken() under its `tokenConfigured` guard, and the failed
        // save cleared the keychain, so that guard now skips adoption instead of
        // re-binding the stale bearer to the new server.
        let tokenConfigured = store.read()?.isEmpty == false
        #expect(tokenConfigured == false)
        if tokenConfigured, settings.tokenOrigin == nil {
            settings.adoptUnboundToken()
        }
        #expect(settings.tokenOrigin == nil)
    }

    @Test func aSuccessfulTokenSaveBindsAndSendsTheNewBearer() {
        let store = FakeTokenStore(value: "old-secret")
        var settings = AppSettings(
            serverURL: url("https://old.example.com"),
            tokenOrigin: origin("https://old.example.com")
        )
        settings.applyEdit(
            serverURLString: "https://new.example.com",
            dataDirOverride: "",
            tokenInEdit: "new-secret"
        )

        let err = settings.commitTokenEdit(clearToken: false, token: "new-secret", to: store)

        #expect(err == nil)
        #expect(store.read() == "new-secret")
        #expect(settings.tokenOrigin == origin("https://new.example.com"))
        #expect(
            DataDirToken.effective(
                saved: store.read(),
                savedOrigin: settings.tokenOrigin,
                serverURL: url("https://new.example.com"),
                dataDir: noDataDir
            ) == "new-secret"
        )
    }

    @Test func aFailedClearLeavesTheTokenAndBindingUntouched() {
        let store = FakeTokenStore(value: "secret")
        store.failClear = true
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )

        let err = settings.commitTokenEdit(clearToken: true, token: nil, to: store)

        #expect(err != nil)
        #expect(store.read() == "secret") // not lost on a failed clear
        #expect(settings.tokenOrigin == origin("https://a.example.com")) // binding intact
    }

    @Test func aSuccessfulClearForgetsTheTokenAndBinding() {
        let store = FakeTokenStore(value: "secret")
        var settings = AppSettings(
            serverURL: url("https://a.example.com"),
            tokenOrigin: origin("https://a.example.com")
        )

        let err = settings.commitTokenEdit(clearToken: true, token: nil, to: store)

        #expect(err == nil)
        #expect(store.read() == nil)
        #expect(settings.tokenOrigin == nil)
    }
}

private enum FakeTokenStoreError: Error { case induced }

private final class FakeTokenStore: TokenStore, @unchecked Sendable {
    private var value: String?
    var failSave = false
    var failClear = false

    init(value: String? = nil) { self.value = value }

    func read() -> String? { value }

    func save(_ token: String) throws {
        if failSave { throw FakeTokenStoreError.induced }
        value = token
    }

    func clear() throws {
        if failClear { throw FakeTokenStoreError.induced }
        value = nil
    }
}
