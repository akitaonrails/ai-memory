import Foundation
import Testing
@testable import AIMemoryMenuCore

struct DataDirTokenTests {
    private func makeDataDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory
            .appending(path: "ai-memory-menu-token-\(UUID().uuidString)")
        try FileManager.default.createDirectory(
            at: dir.appending(path: "auth-tokens"),
            withIntermediateDirectories: true
        )
        return dir
    }

    private func url(_ string: String) -> URL {
        URL(string: string)!
    }

    private func origin(_ string: String) -> ServerOrigin {
        ServerOrigin.of(URL(string: string)!)!
    }

    private let loopback = URL(string: "http://127.0.0.1:49374")!

    @Test func readsTheTokenInitWroteAndTrimsIt() throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        try Data("aim-root-test-token\n".utf8).write(to: DataDirToken.path(dataDir: dir))

        #expect(DataDirToken.read(dataDir: dir) == "aim-root-test-token")
        // With no saved token, a loopback server gets the data dir's root token.
        #expect(
            DataDirToken.effective(saved: nil, savedOrigin: nil, serverURL: loopback, dataDir: dir)
                == "aim-root-test-token"
        )
        #expect(
            DataDirToken.effective(saved: "", savedOrigin: nil, serverURL: loopback, dataDir: dir)
                == "aim-root-test-token"
        )
    }

    @Test func aTokenSavedForTheServerWinsOverTheFile() throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        try Data("from-file".utf8).write(to: DataDirToken.path(dataDir: dir))

        #expect(
            DataDirToken.effective(
                saved: "from-keychain",
                savedOrigin: origin("http://127.0.0.1:49374"),
                serverURL: loopback,
                dataDir: dir
            ) == "from-keychain"
        )
    }

    @Test func aMissingOrBlankFileGivesNoToken() throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }

        #expect(DataDirToken.read(dataDir: dir) == nil)
        try Data("  \n".utf8).write(to: DataDirToken.path(dataDir: dir))
        #expect(DataDirToken.read(dataDir: dir) == nil)
        #expect(
            DataDirToken.effective(saved: nil, savedOrigin: nil, serverURL: loopback, dataDir: dir)
                == nil
        )
    }

    /// A token saved for one server is never sent to a different one, whichever
    /// way the server differs. The data dir holds no file here, so for a
    /// loopback target the data-dir fallback returns nil too and the result is
    /// cleanly "not the saved token".
    @Test func aSavedTokenIsNeverSentToAnotherServer() throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }

        let savedOrigin = origin("https://memory.example.com")
        for target in [
            url("https://other.example.com"), // another host
            url("http://memory.example.com"), // another scheme
            url("https://memory.example.com:8443"), // another port
            url("http://127.0.0.1:49374"), // a loopback address
            url("http://localhost:49374"), // a loopback name
        ] {
            #expect(
                DataDirToken.effective(
                    saved: "secret",
                    savedOrigin: savedOrigin,
                    serverURL: target,
                    dataDir: dir
                ) != "secret",
                "token saved for \(savedOrigin.canonical) must not go to \(target.absoluteString)"
            )
        }
        // A token with no recorded origin is never sent either.
        #expect(
            DataDirToken.effective(
                saved: "secret",
                savedOrigin: nil,
                serverURL: url("https://memory.example.com"),
                dataDir: dir
            ) == nil
        )
    }

    @Test func theDataDirTokenGoesOnlyToALoopbackServer() throws {
        let dir = try makeDataDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        try Data("root-bearer".utf8).write(to: DataDirToken.path(dataDir: dir))

        for target in [
            url("http://127.0.0.1:49374"),
            url("http://localhost:49374"),
            url("http://[::1]:49374"),
        ] {
            #expect(
                DataDirToken.effective(saved: nil, savedOrigin: nil, serverURL: target, dataDir: dir)
                    == "root-bearer",
                "loopback \(target.absoluteString) may use the data dir token"
            )
        }
        for target in [
            url("https://memory.example.com"),
            url("http://10.0.0.5:49374"),
        ] {
            #expect(
                DataDirToken.effective(saved: nil, savedOrigin: nil, serverURL: target, dataDir: dir)
                    == nil,
                "a remote server \(target.absoluteString) never receives the data dir token"
            )
        }
    }
}
