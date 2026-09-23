import AppKit
import Foundation
import AIMemoryMenuCore
import Observation

@MainActor
@Observable
final class AppModel {
    var settings: AppSettings
    var report: StatusReport?
    var launchd: LaunchdState = .notInstalled
    var icon: IconState = .unknown
    var lastError: String?
    var statusOutput: String = ""
    var isBusy = false
    var runtimeMissing = false
    var tokenConfigured = false
    var lastHTTPStatus: Int?

    @ObservationIgnored private var tokenStore: any TokenStore
    @ObservationIgnored private var health = HealthClient()
    @ObservationIgnored private var runner: any CommandRunning
    @ObservationIgnored private var pollTask: Task<Void, Never>?
    @ObservationIgnored private var fetchFailed = false
    @ObservationIgnored private var startingDeadline: Date?

    init(
        settings: AppSettings = .load(),
        tokenStore: any TokenStore = KeychainTokenStore(),
        runner: any CommandRunning = ProcessRunner()
    ) {
        self.settings = settings
        self.tokenStore = tokenStore
        self.runner = runner
        self.tokenConfigured = tokenStore.read()?.isEmpty == false
        startPolling()
    }

    var headlineVersion: String {
        switch icon {
        case .ok:
            if let report {
                return "Server running · v\(report.version)"
            }
            return "Server running"
        case .degraded:
            return "Server running · warnings"
        case .authRequired:
            return "Server running · auth required"
        case .starting:
            return "Server is starting…"
        case .unreachable:
            return launchd == .running ? "LaunchAgent up · server unreachable" : "Server down"
        case .notInstalled:
            return "LaunchAgent not installed"
        case .unknown:
            return "Checking server…"
        }
    }

    var statisticLines: [String] {
        guard let report else {
            return []
        }
        return report.statisticLines
    }

    var showsServerStatus: Bool {
        report != nil
    }

    var dataDir: URL {
        settings.resolvedDataDir
    }

    var configURL: URL {
        dataDir.appending(path: "config.toml")
    }

    var logURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appending(path: "Library/Logs/ai-memory/stderr.log")
    }

    func startPolling() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                await self.poll()
                let interval: Duration = self.icon == .starting ? .seconds(1) : .seconds(15)
                try? await Task.sleep(for: interval)
            }
        }
    }

    func poll() async {
        let controller = launchdController()
        launchd = controller.state()
        lastHTTPStatus = nil
        do {
            report = try await health.fetch(baseURL: settings.serverURL, token: tokenStore.read())
            fetchFailed = false
            lastError = nil
        } catch {
            report = nil
            fetchFailed = true
            if let health = error as? HealthError, case let .http(code) = health {
                lastHTTPStatus = code
                fetchFailed = code != 401 && code != 403
            }
            lastError = (error as? HealthError).map(Self.describe) ?? error.localizedDescription
        }
        let derived = IconState.derived(
            launchd: launchd,
            report: report,
            fetchFailed: fetchFailed,
            httpStatus: lastHTTPStatus
        )
        let starting = startingDeadline.map { Date() < $0 } ?? false
        icon = IconState.applyingStartGrace(derived, isStarting: starting)
        if icon != .starting {
            startingDeadline = nil
        }
        runtimeMissing = RuntimeLayout.resolve() == nil
        tokenConfigured = tokenStore.read()?.isEmpty == false
    }

    func installAndStart() async {
        markStarting()
        await runBusy {
            let runtime = try self.requireRuntime()
            let controller = self.launchdController()
            if FirstRun.needsInit(dataDir: self.dataDir) {
                _ = try BundledCLI(runtime: runtime, runner: self.runner).initDataDir(self.dataDir)
            }
            let template = try String(contentsOf: runtime.plistTemplate, encoding: .utf8)
            if controller.state() == .running {
                try? controller.bootout()
            }
            try controller.writePlist(
                template: template,
                binary: runtime.binary,
                dataDir: self.settings.dataDirOverride
            )
            try controller.bootstrap()
        }
        clearStartingIfFailed()
        await poll()
    }

    func start() async {
        markStarting()
        await runBusy {
            let runtime = try self.requireRuntime()
            let controller = self.launchdController()
            let template = try String(contentsOf: runtime.plistTemplate, encoding: .utf8)
            try controller.writePlist(
                template: template,
                binary: runtime.binary,
                dataDir: self.settings.dataDirOverride
            )
            try controller.bootstrap()
        }
        clearStartingIfFailed()
        await poll()
    }

    func stop() async {
        startingDeadline = nil
        await runBusy {
            try self.launchdController().bootout()
        }
        await poll()
    }

    func restart() async {
        markStarting()
        await runBusy {
            let controller = self.launchdController()
            let runtime = try self.requireRuntime()
            let installed = (try? String(contentsOf: controller.plistDestination, encoding: .utf8)) ?? ""
            if LaunchdController.programArgumentsBinary(inPlist: installed) != runtime.binary.path {
                try? controller.bootout()
                let template = try String(contentsOf: runtime.plistTemplate, encoding: .utf8)
                try controller.writePlist(
                    template: template,
                    binary: runtime.binary,
                    dataDir: self.settings.dataDirOverride
                )
                try controller.bootstrap()
            } else {
                try controller.kickstart()
            }
        }
        clearStartingIfFailed()
        await poll()
    }

    func refreshStatusOutput() async {
        await runBusy {
            let runtime = try self.requireRuntime()
            let result = try BundledCLI(runtime: runtime, runner: self.runner).status(
                serverURL: self.settings.serverURL,
                token: self.tokenStore.read(),
                dataDir: self.settings.dataDirOverride
            )
            self.statusOutput = result.combinedOutput
        }
    }

    func openWebUI() {
        NSWorkspace.shared.open(HealthURL.webUI(from: settings.serverURL))
    }

    func openConfig() {
        revealOrOpen(configURL)
    }

    func openDataDirectory() {
        NSWorkspace.shared.open(dataDir)
    }

    func openLogs() {
        revealOrOpen(logURL)
    }

    func saveSettings(
        serverURLString: String,
        dataDirOverride: String,
        token: String?,
        clearToken: Bool
    ) {
        if let url = URL(string: serverURLString), url.scheme != nil {
            settings.serverURL = url
        }
        let trimmed = dataDirOverride.trimmingCharacters(in: .whitespacesAndNewlines)
        settings.dataDirOverride = trimmed.isEmpty ? nil : URL(fileURLWithPath: trimmed)
        settings.save()
        if clearToken {
            try? tokenStore.clear()
        } else if let token {
            let value = token.trimmingCharacters(in: .whitespacesAndNewlines)
            if !value.isEmpty {
                try? tokenStore.save(value)
            }
        }
        tokenConfigured = tokenStore.read()?.isEmpty == false
        Task { await poll() }
    }

    private func markStarting() {
        startingDeadline = Date().addingTimeInterval(45)
        icon = .starting
        report = nil
        lastError = nil
        pollTask?.cancel()
        pollTask = nil
        startPolling()
    }

    private func clearStartingIfFailed() {
        if lastError != nil {
            startingDeadline = nil
        }
    }

    private func launchdController() -> LaunchdController {
        LaunchdController(home: FileManager.default.homeDirectoryForCurrentUser, runner: runner)
    }

    private func requireRuntime() throws -> RuntimeLayout {
        guard let runtime = RuntimeLayout.resolve() else {
            runtimeMissing = true
            throw CliError.missingRuntime
        }
        return runtime
    }

    private func runBusy(_ work: () throws -> Void) async {
        isBusy = true
        defer { isBusy = false }
        do {
            try work()
            lastError = nil
        } catch {
            lastError = error.localizedDescription
            if let cli = error as? CliError, case .failed(_, let output) = cli {
                statusOutput = output
                lastError = output
            }
        }
    }

    private func revealOrOpen(_ url: URL) {
        if FileManager.default.fileExists(atPath: url.path) {
            NSWorkspace.shared.activateFileViewerSelecting([url])
        } else {
            NSWorkspace.shared.open(url.deletingLastPathComponent())
        }
    }

    private static func describe(_ error: HealthError) -> String {
        switch error {
        case .badURL:
            "Invalid server URL"
        case .http(let code) where code == 401 || code == 403:
            "Server is up but this app is not authorized (HTTP \(code)). Add a bearer in Settings."
        case .http(let code):
            "Server returned HTTP \(code)"
        case .decode:
            "Could not read /admin/status"
        case .transport(let message):
            message
        }
    }
}

extension CliError: LocalizedError {
    public var errorDescription: String? {
        switch self {
        case .timeout:
            "Timed out running ai-memory"
        case .missingRuntime:
            "Bundled ai-memory runtime is missing. Build with companions/ai-memory-macos/build.sh"
        case .failed(_, let output):
            output.isEmpty ? "ai-memory command failed" : output
        }
    }
}
