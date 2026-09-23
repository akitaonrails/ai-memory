import Foundation

/// Layout of `Contents/Resources/runtime/`: the release-tarball sibling pair
/// (`ai-memory` + `hooks/`) plus the launchd template.
public struct RuntimeLayout: Equatable, Sendable {
    public var root: URL

    public init(root: URL) {
        self.root = root
    }

    public var binary: URL {
        root.appending(path: "ai-memory")
    }

    public var hooks: URL {
        root.appending(path: "hooks")
    }

    public var plistTemplate: URL {
        root.appending(path: "packaging/launchd/\(LaunchdController.label).plist")
    }

    public func validate(fileManager: FileManager = .default) -> Bool {
        fileManager.isExecutableFile(atPath: binary.path)
            && fileManager.fileExists(atPath: hooks.path)
            && fileManager.fileExists(atPath: plistTemplate.path)
    }

    /// Prefers the staged bundle resource; `AI_MEMORY_MENU_RUNTIME` is a
    /// developer override for `swift run` without wrapping an `.app`.
    public static func resolve(
        bundle: Bundle = .main,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        fileManager: FileManager = .default
    ) -> RuntimeLayout? {
        if let env = environment["AI_MEMORY_MENU_RUNTIME"], !env.isEmpty {
            let layout = RuntimeLayout(root: URL(fileURLWithPath: env))
            if layout.validate(fileManager: fileManager) {
                return layout
            }
        }
        if let resourceRoot = bundle.resourceURL {
            let layout = RuntimeLayout(root: resourceRoot.appending(path: "runtime"))
            if layout.validate(fileManager: fileManager) {
                return layout
            }
        }
        return nil
    }
}

public enum FirstRun {
    public static func needsInit(dataDir: URL, fileManager: FileManager = .default) -> Bool {
        !fileManager.fileExists(atPath: dataDir.appending(path: "config.toml").path)
    }

    public static func defaultDataDir(fileManager: FileManager = .default) -> URL {
        let base = fileManager.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory())
                .appending(path: "Library/Application Support")
        return base.appending(path: "ai-memory")
    }
}

public struct BundledCLI: Sendable {
    public var runtime: RuntimeLayout
    public var runner: any CommandRunning

    public init(runtime: RuntimeLayout, runner: any CommandRunning = ProcessRunner()) {
        self.runtime = runtime
        self.runner = runner
    }

    public func initDataDir(_ dataDir: URL) throws -> ProcessResult {
        try run(arguments: ["--data-dir", dataDir.path, "init"], extraEnv: [:])
    }

    public func status(serverURL: URL, token: String?, dataDir: URL?) throws -> ProcessResult {
        var args: [String] = []
        var env: [String: String] = [
            "AI_MEMORY_SERVER_URL": serverURL.absoluteString,
        ]
        if let dataDir {
            args.append(contentsOf: ["--data-dir", dataDir.path])
        }
        if let token, !token.isEmpty {
            env["AI_MEMORY_AUTH_TOKEN"] = token
        }
        args.append("status")
        return try run(arguments: args, extraEnv: env)
    }

    private func run(arguments: [String], extraEnv: [String: String]) throws -> ProcessResult {
        let result = try runner.run(binary: runtime.binary, arguments: arguments, extraEnv: extraEnv)
        if result.exitCode != 0 {
            throw CliError.failed(result.exitCode, result.combinedOutput)
        }
        return result
    }
}
