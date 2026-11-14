import Darwin
import Foundation

public enum LaunchdState: Equatable, Sendable {
    case notInstalled
    case stopped
    case running
}

public struct LaunchdController: Sendable {
    public static let label = "com.github.akitaonrails.ai-memory"

    public var home: URL
    public var runner: any CommandRunning

    public init(home: URL, runner: any CommandRunning = ProcessRunner()) {
        self.home = home
        self.runner = runner
    }

    public var plistDestination: URL {
        home.appending(path: "Library/LaunchAgents")
            .appending(path: "\(Self.label).plist")
    }

    public var logDirectory: URL {
        home.appending(path: "Library/Logs/ai-memory")
    }

    public static func renderTemplate(
        _ template: String,
        binary: URL,
        home: URL,
        dataDir: URL?
    ) -> String {
        var rendered = template
            .replacingOccurrences(of: "__AI_MEMORY_BIN__", with: xmlEscape(binary.path))
            .replacingOccurrences(of: "__HOME__", with: xmlEscape(home.path))
        if let dataDir {
            let env = """
              <key>EnvironmentVariables</key>
              <dict>
                <key>AI_MEMORY_DATA_DIR</key>
                <string>\(xmlEscape(dataDir.path))</string>
              </dict>

            """
            if let range = rendered.range(of: "</dict>", options: .backwards) {
                rendered.replaceSubrange(range, with: env + "</dict>")
            }
        }
        return rendered
    }

    public static func programArgumentsBinary(inPlist plist: String) -> String? {
        guard let values = try? PropertyListSerialization.propertyList(
            from: Data(plist.utf8), format: nil
        ) as? [String: Any],
            let arguments = values["ProgramArguments"] as? [String]
        else {
            return nil
        }
        return arguments.first
    }

    public func writePlist(template: String, binary: URL, dataDir: URL?) throws {
        let fm = FileManager.default
        try fm.createDirectory(
            at: plistDestination.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try fm.createDirectory(at: logDirectory, withIntermediateDirectories: true)
        let body = Self.renderTemplate(template, binary: binary, home: home, dataDir: dataDir)
        try body.write(to: plistDestination, atomically: true, encoding: .utf8)
    }

    public func state() -> LaunchdState {
        let plistExists = FileManager.default.fileExists(atPath: plistDestination.path)
        let result = try? runner.run(
            binary: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: ["print", domainService],
            extraEnv: [:]
        )
        if let result, result.exitCode == 0 {
            if result.stdout.contains("state = running") {
                return .running
            }
            return .stopped
        }
        return plistExists ? .stopped : .notInstalled
    }

    public func bootstrap() throws {
        try runLaunchctl(["bootstrap", domain, plistDestination.path])
    }

    public func bootout() throws {
        try runLaunchctl(["bootout", domainService])
    }

    public func kickstart() throws {
        try runLaunchctl(["kickstart", "-k", domainService])
    }

    public func restart(template: String, binary: URL, dataDir: URL?) throws {
        let desired = Self.renderTemplate(template, binary: binary, home: home, dataDir: dataDir)
        guard let desired = Self.managedConfiguration(inPlist: desired),
              desired.arguments.first == binary.path
        else {
            throw CocoaError(.propertyListReadCorrupt, userInfo: [
                NSLocalizedDescriptionKey: "The bundled LaunchAgent template is invalid.",
            ])
        }
        let fm = FileManager.default
        let previous = fm.fileExists(atPath: plistDestination.path) ? try Data(contentsOf: plistDestination) : nil
        let installed = previous.flatMap { String(data: $0, encoding: .utf8) } ?? ""
        // kickstart reuses launchd's loaded configuration, including its data
        // directory. Reload when the managed arguments or override changed.
        // See launchctl(1), bootstrap/bootout versus kickstart, and Apple's guide:
        // https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html
        if let installed = Self.managedConfiguration(inPlist: installed),
           installed == desired
        {
            try kickstart()
        } else {
            try? bootout()
            do {
                try writePlist(template: template, binary: binary, dataDir: dataDir)
                try bootstrap()
            } catch {
                let failure = error
                do {
                    if let previous {
                        try previous.write(to: plistDestination, options: .atomic)
                    } else if fm.fileExists(atPath: plistDestination.path) {
                        try fm.removeItem(at: plistDestination)
                    }
                } catch {
                    throw RestartRollbackFailure(failure: failure, rollback: error)
                }
                throw failure
            }
        }
    }

    private struct RestartRollbackFailure: LocalizedError {
        let failure: any Error
        let rollback: any Error

        var errorDescription: String? {
            "The LaunchAgent could not be restored after a failed restart (\(failure.localizedDescription)): \(rollback.localizedDescription)"
        }
    }

    private struct ManagedConfiguration: Equatable {
        let arguments: [String]
        let dataDir: String?
    }

    private static func managedConfiguration(inPlist plist: String) -> ManagedConfiguration? {
        guard let values = try? PropertyListSerialization.propertyList(
            from: Data(plist.utf8), format: nil
        ) as? [String: Any],
            values["Label"] as? String == Self.label,
            let arguments = values["ProgramArguments"] as? [String],
            !arguments.isEmpty
        else {
            return nil
        }
        guard let rawEnvironment = values["EnvironmentVariables"] else {
            return ManagedConfiguration(arguments: arguments, dataDir: nil)
        }
        guard let environment = rawEnvironment as? [String: String] else {
            return nil
        }
        return ManagedConfiguration(arguments: arguments, dataDir: environment["AI_MEMORY_DATA_DIR"])
    }

    private func runLaunchctl(_ arguments: [String]) throws {
        let result = try runner.run(
            binary: URL(fileURLWithPath: "/bin/launchctl"),
            arguments: arguments,
            extraEnv: [:]
        )
        if result.exitCode != 0 {
            throw CliError.failed(result.exitCode, result.combinedOutput)
        }
    }

    private var domain: String {
        "gui/\(getuid())"
    }

    private var domainService: String {
        "\(domain)/\(Self.label)"
    }

    private static func xmlEscape(_ value: String) -> String {
        value
            .replacingOccurrences(of: "&", with: "&amp;")
            .replacingOccurrences(of: "<", with: "&lt;")
            .replacingOccurrences(of: ">", with: "&gt;")
            .replacingOccurrences(of: "\"", with: "&quot;")
    }
}
