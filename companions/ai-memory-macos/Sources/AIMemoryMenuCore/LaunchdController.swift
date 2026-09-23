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
            .replacingOccurrences(of: "__AI_MEMORY_BIN__", with: binary.path)
            .replacingOccurrences(of: "__HOME__", with: home.path)
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
        // First <string> after ProgramArguments is the executable.
        guard let argsRange = plist.range(of: "<key>ProgramArguments</key>") else {
            return nil
        }
        let rest = plist[argsRange.upperBound...]
        guard let start = rest.range(of: "<string>") else {
            return nil
        }
        let after = rest[start.upperBound...]
        guard let end = after.range(of: "</string>") else {
            return nil
        }
        return String(after[..<end.lowerBound])
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
