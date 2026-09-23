import Foundation

/// Wire shape of `GET /admin/status`. Unknown fields are ignored so an
/// older or newer server still decodes the headlines this wrapper shows.
public struct StatusReport: Decodable, Equatable, Sendable {
    public var version: String
    public var dataDir: String?
    public var bind: String?
    public var counts: StatusCounts
    public var writeQueue: WriteQueue?
    public var providers: ProviderHealthSnapshot?
    public var ingest: IngestSnapshot?

    public init(
        version: String,
        dataDir: String? = nil,
        bind: String? = nil,
        counts: StatusCounts,
        writeQueue: WriteQueue? = nil,
        providers: ProviderHealthSnapshot? = nil,
        ingest: IngestSnapshot? = nil
    ) {
        self.version = version
        self.dataDir = dataDir
        self.bind = bind
        self.counts = counts
        self.writeQueue = writeQueue
        self.providers = providers
        self.ingest = ingest
    }

    enum CodingKeys: String, CodingKey {
        case version
        case bind
        case counts
        case providers
        case ingest
        case dataDir = "data_dir"
        case writeQueue = "write_queue"
    }

    public var isDegraded: Bool {
        if let queue = writeQueue, queue.queued > 0 {
            return true
        }
        if providers?.llm.status == "error" {
            return true
        }
        if providers?.embedding.status == "error" {
            return true
        }
        return false
    }

    public var llmHeadline: String {
        roleHeadline(label: "LLM", role: providers?.llm)
    }

    public var embeddingHeadline: String {
        roleHeadline(label: "Embed", role: providers?.embedding)
    }

    /// Lines shown in the menu extra. Counts come from `GET /admin/status`.
    public var statisticLines: [String] {
        var lines: [String] = []
        if let bind, !bind.isEmpty {
            lines.append("Bind \(bind)")
        }
        lines.append(contentsOf: [
            "Pages \(counts.pagesLatest)  (all versions \(counts.pagesAll))",
            "Sessions \(counts.sessions)",
            "Observations \(counts.observations)",
            llmHeadline,
            embeddingHeadline,
        ])
        if let queue = writeQueue, queue.queued > 0 {
            lines.append("Write queue \(queue.queued)/\(queue.capacity)")
        }
        if let ingest {
            lines.append("Ingest accepted \(ingest.accepted)")
            if ingest.droppedByPolicy > 0 {
                lines.append("Dropped by policy \(ingest.droppedByPolicy)")
            }
            lines.append("Last write \(Self.lastWriteLabel(ingest.lastPersistedMs))")
        }
        return lines
    }

    private func roleHeadline(label: String, role: ProviderRoleHealth?) -> String {
        guard let role else {
            return "\(label) unknown"
        }
        var text = "\(label) \(role.status)"
        if let provider = role.provider, !provider.isEmpty {
            if let model = role.model, !model.isEmpty {
                text += "  \(provider)/\(model)"
            } else {
                text += "  \(provider)"
            }
        }
        return text
    }

    public static func lastWriteLabel(_ unixMs: UInt64?) -> String {
        guard let unixMs else {
            return "—"
        }
        let nowMs = UInt64(max(0, Date().timeIntervalSince1970 * 1000))
        let ageMs = nowMs > unixMs ? nowMs - unixMs : 0
        let secs = ageMs / 1000
        if secs < 60 {
            return "\(secs)s ago"
        }
        if secs < 3600 {
            return "\(secs / 60)m ago"
        }
        if secs < 86_400 {
            return "\(secs / 3600)h ago"
        }
        return "\(secs / 86_400)d ago"
    }
}

public struct IngestSnapshot: Decodable, Equatable, Sendable {
    public var accepted: UInt64
    public var droppedByPolicy: UInt64
    public var shedSaturated: UInt64
    public var shedRateLimited: UInt64
    public var lastPersistedMs: UInt64?

    public init(
        accepted: UInt64 = 0,
        droppedByPolicy: UInt64 = 0,
        shedSaturated: UInt64 = 0,
        shedRateLimited: UInt64 = 0,
        lastPersistedMs: UInt64? = nil
    ) {
        self.accepted = accepted
        self.droppedByPolicy = droppedByPolicy
        self.shedSaturated = shedSaturated
        self.shedRateLimited = shedRateLimited
        self.lastPersistedMs = lastPersistedMs
    }

    enum CodingKeys: String, CodingKey {
        case accepted
        case droppedByPolicy = "dropped_by_policy"
        case shedSaturated = "shed_saturated"
        case shedRateLimited = "shed_rate_limited"
        case lastPersistedMs = "last_persisted_ms"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        accepted = try container.decodeIfPresent(UInt64.self, forKey: .accepted) ?? 0
        droppedByPolicy = try container.decodeIfPresent(UInt64.self, forKey: .droppedByPolicy) ?? 0
        shedSaturated = try container.decodeIfPresent(UInt64.self, forKey: .shedSaturated) ?? 0
        shedRateLimited = try container.decodeIfPresent(UInt64.self, forKey: .shedRateLimited) ?? 0
        lastPersistedMs = try container.decodeIfPresent(UInt64.self, forKey: .lastPersistedMs)
    }
}

public struct StatusCounts: Decodable, Equatable, Sendable {
    public var pagesLatest: UInt64
    public var pagesAll: UInt64
    public var sessions: UInt64
    public var observations: UInt64

    public init(pagesLatest: UInt64, pagesAll: UInt64, sessions: UInt64, observations: UInt64) {
        self.pagesLatest = pagesLatest
        self.pagesAll = pagesAll
        self.sessions = sessions
        self.observations = observations
    }

    enum CodingKeys: String, CodingKey {
        case pagesLatest = "pages_latest"
        case pagesAll = "pages_all"
        case sessions
        case observations
    }
}

/// JSON encoding of the server's `(queued, capacity)` tuple.
public struct WriteQueue: Decodable, Equatable, Sendable {
    public var queued: Int
    public var capacity: Int

    public init(queued: Int, capacity: Int) {
        self.queued = queued
        self.capacity = capacity
    }

    public init(from decoder: Decoder) throws {
        var container = try decoder.unkeyedContainer()
        queued = try container.decode(Int.self)
        capacity = try container.decode(Int.self)
    }
}

public struct ProviderHealthSnapshot: Decodable, Equatable, Sendable {
    public var llm: ProviderRoleHealth
    public var embedding: ProviderRoleHealth

    public init(llm: ProviderRoleHealth, embedding: ProviderRoleHealth) {
        self.llm = llm
        self.embedding = embedding
    }
}

public struct ProviderRoleHealth: Decodable, Equatable, Sendable {
    public var status: String
    public var provider: String?
    public var model: String?

    public init(status: String, provider: String? = nil, model: String? = nil) {
        self.status = status
        self.provider = provider
        self.model = model
    }
}

public enum IconState: Equatable, Sendable {
    case unknown
    case notInstalled
    case unreachable
    case starting
    case authRequired
    case ok
    case degraded

    public static func derived(
        launchd: LaunchdState,
        report: StatusReport?,
        fetchFailed: Bool,
        httpStatus: Int? = nil
    ) -> IconState {
        if let report {
            return report.isDegraded ? .degraded : .ok
        }
        if let httpStatus, httpStatus == 401 || httpStatus == 403 {
            return .authRequired
        }
        if fetchFailed {
            return launchd == .notInstalled ? .notInstalled : .unreachable
        }
        return .unknown
    }

    /// Keep the "starting" overlay until `/admin/status` answers or the grace expires.
    public static func applyingStartGrace(_ derived: IconState, isStarting: Bool) -> IconState {
        guard isStarting else {
            return derived
        }
        switch derived {
        case .ok, .degraded, .authRequired:
            return derived
        default:
            return .starting
        }
    }

    public var accessibilityLabel: String {
        switch self {
        case .unknown:
            "ai-memory, status unknown"
        case .notInstalled:
            "ai-memory, not installed"
        case .unreachable:
            "ai-memory, server down"
        case .starting:
            "ai-memory, server is starting"
        case .authRequired:
            "ai-memory, running, authentication required"
        case .ok:
            "ai-memory, server running"
        case .degraded:
            "ai-memory, running with warnings"
        }
    }
}
