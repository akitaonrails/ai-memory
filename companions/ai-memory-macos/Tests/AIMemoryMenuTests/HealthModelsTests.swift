import Foundation
import Testing
@testable import AIMemoryMenuCore

struct HealthModelsTests {
    @Test func decodesStatusFixture() throws {
        let url = try #require(Bundle.module.url(forResource: "status", withExtension: "json", subdirectory: "fixtures"))
        let data = try Data(contentsOf: url)
        let report = try JSONDecoder().decode(StatusReport.self, from: data)
        #expect(report.version == "2.3.2")
        #expect(report.counts.pagesLatest == 138)
        #expect(report.counts.sessions == 27)
        #expect(report.writeQueue == WriteQueue(queued: 0, capacity: 128))
        #expect(report.providers?.llm.status == "ok")
        #expect(report.providers?.embedding.status == "disabled")
        #expect(report.ingest?.accepted == 4198)
        #expect(!report.isDegraded)
        #expect(report.llmHeadline.contains("ok"))
        let stats = report.statisticLines
        #expect(stats.contains { $0.hasPrefix("Pages 138") })
        #expect(stats.contains { $0.hasPrefix("Sessions 27") })
        #expect(stats.contains { $0.hasPrefix("Observations 4198") })
        #expect(stats.contains { $0.hasPrefix("Bind ") })
        #expect(stats.contains { $0.hasPrefix("Ingest accepted 4198") })
    }

    @Test func decodesOlderPayloadWithoutWriteQueueOrProviders() throws {
        let json = """
        {
          "version": "1.0.0",
          "counts": {
            "pages_latest": 1,
            "pages_all": 1,
            "sessions": 0,
            "observations": 0
          }
        }
        """.data(using: .utf8)!
        let report = try JSONDecoder().decode(StatusReport.self, from: json)
        #expect(report.writeQueue == nil)
        #expect(report.providers == nil)
        #expect(!report.isDegraded)
    }

    @Test func degradedWhenProviderErrorsOrQueueIsBusy() {
        let ok = StatusReport(
            version: "2.3.2",
            counts: StatusCounts(pagesLatest: 1, pagesAll: 1, sessions: 0, observations: 0),
            writeQueue: WriteQueue(queued: 0, capacity: 8),
            providers: ProviderHealthSnapshot(
                llm: ProviderRoleHealth(status: "ok"),
                embedding: ProviderRoleHealth(status: "disabled")
            )
        )
        #expect(!ok.isDegraded)

        var queued = ok
        queued.writeQueue = WriteQueue(queued: 1, capacity: 8)
        #expect(queued.isDegraded)

        var llmError = ok
        llmError.writeQueue = WriteQueue(queued: 0, capacity: 8)
        llmError.providers = ProviderHealthSnapshot(
            llm: ProviderRoleHealth(status: "error"),
            embedding: ProviderRoleHealth(status: "ok")
        )
        #expect(llmError.isDegraded)

        var embedError = ok
        embedError.providers = ProviderHealthSnapshot(
            llm: ProviderRoleHealth(status: "ok"),
            embedding: ProviderRoleHealth(status: "error")
        )
        #expect(embedError.isDegraded)
    }
}

struct IconStateTests {
    @Test func derivedStates() {
        let report = StatusReport(
            version: "2.3.2",
            counts: StatusCounts(pagesLatest: 1, pagesAll: 1, sessions: 0, observations: 0)
        )
        #expect(IconState.derived(launchd: .running, report: report, fetchFailed: false) == .ok)

        var degraded = report
        degraded.writeQueue = WriteQueue(queued: 3, capacity: 8)
        #expect(IconState.derived(launchd: .running, report: degraded, fetchFailed: false) == .degraded)

        #expect(IconState.derived(launchd: .notInstalled, report: nil, fetchFailed: true) == .notInstalled)
        #expect(IconState.derived(launchd: .stopped, report: nil, fetchFailed: true) == .unreachable)
        #expect(IconState.derived(launchd: .running, report: nil, fetchFailed: true) == .unreachable)
        #expect(IconState.derived(launchd: .notInstalled, report: nil, fetchFailed: false) == .unknown)
        #expect(
            IconState.derived(
                launchd: .running,
                report: nil,
                fetchFailed: true,
                httpStatus: 401
            ) == .authRequired
        )
        #expect(
            IconState.applyingStartGrace(.unreachable, isStarting: true) == .starting
        )
        #expect(IconState.applyingStartGrace(.ok, isStarting: true) == .ok)
        #expect(IconState.applyingStartGrace(.unreachable, isStarting: false) == .unreachable)
    }
}
