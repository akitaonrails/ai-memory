import AIMemoryMenuCore
import Foundation
import Testing
@testable import AIMemoryMenu

struct HealthPollingTests {
    @Test(arguments: [false, true], [false, true])
    @MainActor
    func latePollDoesNotReplaceCurrentServerStatus(oldRequestFails: Bool, changesServer: Bool) async throws {
        let oldServer = try #require(URL(string: "http://old.example.invalid"))
        let newServer = try #require(URL(string: "http://new.example.invalid"))
        let counts = StatusCounts(pagesLatest: 1, pagesAll: 1, sessions: 0, observations: 0)
        let (started, oldStarted) = AsyncStream<Void>.makeStream()
        let (release, releaseOld) = AsyncStream<Void>.makeStream()
        let ordering = PollOrder()
        let model = AppModel(
            settings: AppSettings(
                serverURL: oldServer,
                dataDirOverride: FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
            ),
            tokenStore: MemoryTokenStore(),
            runner: FakeRunningAgent(),
            fetchHealth: { _, _ in
                if await ordering.isFirst() {
                    oldStarted.yield(())
                    for await _ in release {}
                    if oldRequestFails { throw HealthError.http(503) }
                    return StatusReport(version: "old", counts: counts)
                }
                return StatusReport(version: "new", counts: counts)
            },
            pollAutomatically: false
        )

        let oldPoll = Task { await model.poll() }
        var iterator = started.makeAsyncIterator()
        await iterator.next()
        #expect(model.launchd == .running)

        if changesServer {
            model.settings.serverURL = newServer
        }
        await model.poll()
        #expect(model.report?.version == "new")

        releaseOld.finish()
        await oldPoll.value
        #expect(model.report?.version == "new")
        #expect(model.lastHTTPStatus == nil)
        #expect(model.lastError == nil)
        #expect(model.icon == .ok)
    }
}

private actor PollOrder {
    private var received = false

    func isFirst() -> Bool {
        defer { received = true }
        return !received
    }
}

private struct FakeRunningAgent: CommandRunning {
    func run(binary: URL, arguments: [String], extraEnv: [String: String]) throws -> ProcessResult {
        ProcessResult(exitCode: 0, stdout: "state = running", stderr: "")
    }
}
