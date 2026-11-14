import Foundation
import Darwin
import Testing
@testable import AIMemoryMenuCore

struct LaunchdControllerTests {
    @Test func substitutesPlaceholders() {
        let template = """
        <string>__AI_MEMORY_BIN__</string>
        <string>__HOME__/Library/Logs/ai-memory/stderr.log</string>
        """
        let rendered = LaunchdController.renderTemplate(
            template,
            binary: URL(fileURLWithPath: "/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"),
            home: URL(fileURLWithPath: "/Users/ada"),
            dataDir: nil
        )
        #expect(rendered.contains("/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"))
        #expect(rendered.contains("/Users/ada/Library/Logs/ai-memory/stderr.log"))
        #expect(!rendered.contains("__AI_MEMORY_BIN__"))
        #expect(!rendered.contains("EnvironmentVariables"))
    }

    @Test func injectsDataDirEnvironment() {
        let template = """
        <dict>
          <key>Label</key>
          <string>com.github.akitaonrails.ai-memory</string>
        </dict>
        """
        let rendered = LaunchdController.renderTemplate(
            template,
            binary: URL(fileURLWithPath: "/bin/ai-memory"),
            home: URL(fileURLWithPath: "/Users/ada"),
            dataDir: URL(fileURLWithPath: "/Users/ada/.ai-memory")
        )
        #expect(rendered.contains("<key>AI_MEMORY_DATA_DIR</key>"))
        #expect(rendered.contains("<string>/Users/ada/.ai-memory</string>"))
        #expect(rendered.contains("<key>EnvironmentVariables</key>"))
    }

    @Test func rendersCheckedInLaunchdTemplate() throws {
        let templateURL = repoRoot()
            .appending(path: "packaging/launchd/com.github.akitaonrails.ai-memory.plist")
        let template = try String(contentsOf: templateURL, encoding: .utf8)
        let binary = URL(fileURLWithPath: "/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory")
        let home = URL(fileURLWithPath: "/Users/ada")
        let rendered = LaunchdController.renderTemplate(template, binary: binary, home: home, dataDir: nil)
        #expect(LaunchdController.programArgumentsBinary(inPlist: rendered) == binary.path)
        #expect(rendered.contains("/Users/ada/Library/Logs/ai-memory/stderr.log"))
        #expect(rendered.contains("serve"))
        #expect(rendered.contains("--enable-web"))
        #expect(!rendered.contains("__HOME__"))
    }

    @Test func xmlEscapesDataDir() {
        let template = "<dict></dict>"
        let rendered = LaunchdController.renderTemplate(
            template,
            binary: URL(fileURLWithPath: "/bin/ai-memory"),
            home: URL(fileURLWithPath: "/Users/ada"),
            dataDir: URL(fileURLWithPath: "/tmp/a&b<c>")
        )
        #expect(rendered.contains("/tmp/a&amp;b&lt;c&gt;"))
    }

    @Test func pathsRoundTripThroughTheInstalledPlist() throws {
        let templateURL = repoRoot()
            .appending(path: "packaging/launchd/com.github.akitaonrails.ai-memory.plist")
        let template = try String(contentsOf: templateURL, encoding: .utf8)
        let root = FileManager.default.temporaryDirectory
            .appending(path: "ai-memory-plist-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: root) }
        let home = root.appending(path: "Ada & Bob <home>")
        let binary = root.appending(path: "R&D/AI Memory.app/Contents/Resources/runtime/ai-memory")
        let dataDir = home.appending(path: "Memory & Notes")
        let controller = LaunchdController(home: home)

        try controller.writePlist(template: template, binary: binary, dataDir: dataDir)

        let data = try Data(contentsOf: controller.plistDestination)
        let plist = try #require(
            PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any]
        )
        #expect(plist["ProgramArguments"] as? [String] == [binary.path, "serve", "--transport", "http", "--enable-web"])
        #expect(plist["StandardOutPath"] as? String == home.appending(path: "Library/Logs/ai-memory/stdout.log").path)
        #expect(plist["StandardErrorPath"] as? String == home.appending(path: "Library/Logs/ai-memory/stderr.log").path)
        #expect(plist["EnvironmentVariables"] as? [String: String] == ["AI_MEMORY_DATA_DIR": dataDir.path])
        #expect(LaunchdController.programArgumentsBinary(inPlist: String(decoding: data, as: UTF8.self)) == binary.path)
    }

    @Test func readsDecodedProgramArgumentsFromPlist() {
        let plist = """
        <plist version="1.0"><dict><key>ProgramArguments</key><array>
        <string>/Applications/R&amp;D/AI Memory.app/ai-memory</string><string>serve</string>
        </array></dict></plist>
        """
        #expect(LaunchdController.programArgumentsBinary(inPlist: plist) == "/Applications/R&D/AI Memory.app/ai-memory")
        #expect(LaunchdController.programArgumentsBinary(inPlist: "not a plist") == nil)
        #expect(LaunchdController.programArgumentsBinary(inPlist: "<plist version=\"1.0\"><dict/></plist>") == nil)
    }

    @Test func restartReloadsChangedOrClearedDataDirectory() throws {
        let original = URL(fileURLWithPath: "/tmp/old-memory")
        let changed = URL(fileURLWithPath: "/tmp/Memory & Notes")
        for (previous, desired) in [(nil, changed), (original, changed), (original, nil)] {
            let fixture = try RestartFixture()
            defer { fixture.remove() }
            try fixture.controller.writePlist(template: fixture.template, binary: fixture.binary, dataDir: previous)

            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: desired)

            #expect(fixture.runner.calls == fixture.reloadCalls)
            let values = try fixture.installedValues()
            #expect((values["EnvironmentVariables"] as? [String: String])?["AI_MEMORY_DATA_DIR"] == desired?.path)
        }
    }

    @Test func restartKeepsUnchangedConfigurationAndUnmanagedFields() throws {
        for dataDir in [URL(fileURLWithPath: "/tmp/custom-memory"), nil] {
            let fixture = try RestartFixture()
            defer { fixture.remove() }
            try fixture.controller.writePlist(template: fixture.template, binary: fixture.binary, dataDir: dataDir)
            var values = try fixture.installedValues()
            values["Nice"] = 5
            var environment = values["EnvironmentVariables"] as? [String: String] ?? [:]
            environment["UNMANAGED_FIXTURE_SETTING"] = "keep this value"
            values["EnvironmentVariables"] = environment
            let original = try PropertyListSerialization.data(fromPropertyList: values, format: .xml, options: 0)
            try original.write(to: fixture.controller.plistDestination)

            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: dataDir)

            #expect(fixture.runner.calls == [["kickstart", "-k", fixture.domainService]])
            #expect(try Data(contentsOf: fixture.controller.plistDestination) == original)
        }
    }

    @Test func restartReloadsChangedBinaryOrArguments() throws {
        for changeBinary in [false, true] {
            let fixture = try RestartFixture()
            defer { fixture.remove() }
            let oldBinary = changeBinary ? URL(fileURLWithPath: "/Applications/Old AI Memory.app/ai-memory") : fixture.binary
            let oldTemplate = changeBinary ? fixture.template : fixture.template.replacingOccurrences(of: "<string>--enable-web</string>", with: "<string>--no-watcher</string>")
            try fixture.controller.writePlist(template: oldTemplate, binary: oldBinary, dataDir: nil)

            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: nil)

            #expect(fixture.runner.calls == fixture.reloadCalls)
            #expect(try fixture.installedValues()["ProgramArguments"] as? [String] == [fixture.binary.path, "serve", "--transport", "http", "--enable-web"])
        }
    }

    @Test func restartRebuildsMissingOrMalformedPlist() throws {
        for malformed in [false, true] {
            let fixture = try RestartFixture()
            defer { fixture.remove() }
            if malformed {
                try FileManager.default.createDirectory(at: fixture.controller.plistDestination.deletingLastPathComponent(), withIntermediateDirectories: true)
                try "not a plist".write(to: fixture.controller.plistDestination, atomically: true, encoding: .utf8)
            }
            fixture.runner.results = [ProcessResult(exitCode: 3, stdout: "", stderr: "not loaded"), ProcessResult(exitCode: 0, stdout: "", stderr: "")]

            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: nil)

            #expect(fixture.runner.calls == fixture.reloadCalls)
            #expect(try fixture.installedValues()["ProgramArguments"] as? [String] == [fixture.binary.path, "serve", "--transport", "http", "--enable-web"])
        }
    }

    @Test func restartSurfacesBootstrapFailure() throws {
        let fixture = try RestartFixture()
        defer { fixture.remove() }
        fixture.runner.results = [ProcessResult(exitCode: 3, stdout: "", stderr: "not loaded"), ProcessResult(exitCode: 5, stdout: "", stderr: "bootstrap refused")]

        #expect(throws: CliError.failed(5, "bootstrap refused")) {
            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: nil)
        }
        #expect(fixture.runner.calls == fixture.reloadCalls)
        #expect(!FileManager.default.fileExists(atPath: fixture.controller.plistDestination.path))
    }

    @Test func failedReloadRestoresThePreviousPlistAndRetriesTheReload() throws {
        let fixture = try RestartFixture()
        defer { fixture.remove() }
        let oldDataDir = URL(fileURLWithPath: "/tmp/old-memory")
        let newDataDir = URL(fileURLWithPath: "/tmp/new-memory")
        try fixture.controller.writePlist(template: fixture.template, binary: fixture.binary, dataDir: oldDataDir)
        let previous = try Data(contentsOf: fixture.controller.plistDestination)
        fixture.runner.results = [
            ProcessResult(exitCode: 5, stdout: "", stderr: "bootout refused"),
            ProcessResult(exitCode: 5, stdout: "", stderr: "bootstrap refused"),
            ProcessResult(exitCode: 0, stdout: "", stderr: ""),
            ProcessResult(exitCode: 0, stdout: "", stderr: ""),
        ]

        #expect(throws: CliError.failed(5, "bootstrap refused")) {
            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: newDataDir)
        }
        #expect(try Data(contentsOf: fixture.controller.plistDestination) == previous)

        try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: newDataDir)

        #expect(fixture.runner.calls == fixture.reloadCalls + fixture.reloadCalls)
        #expect((try fixture.installedValues()["EnvironmentVariables"] as? [String: String])?["AI_MEMORY_DATA_DIR"] == newDataDir.path)
    }

    @Test func invalidDesiredTemplateLeavesTheInstalledJobAlone() throws {
        let fixture = try RestartFixture()
        defer { fixture.remove() }
        try fixture.controller.writePlist(template: fixture.template, binary: fixture.binary, dataDir: nil)
        let previous = try Data(contentsOf: fixture.controller.plistDestination)

        for template in [
            "not a plist",
            fixture.template.replacingOccurrences(of: LaunchdController.label, with: "invalid.label"),
            fixture.template.replacingOccurrences(of: "__AI_MEMORY_BIN__", with: "/bin/unexpected"),
        ] {
            #expect(throws: CocoaError.self) {
                try fixture.controller.restart(template: template, binary: fixture.binary, dataDir: nil)
            }
            #expect(fixture.runner.calls.isEmpty)
            #expect(try Data(contentsOf: fixture.controller.plistDestination) == previous)
        }
    }

    @Test func failedRollbackIsReported() throws {
        let fixture = try RestartFixture()
        defer { fixture.remove() }
        try fixture.controller.writePlist(template: fixture.template, binary: fixture.binary, dataDir: nil)
        fixture.runner.onCall = { arguments in
            if arguments.first == "bootstrap" {
                let parent = fixture.controller.plistDestination.deletingLastPathComponent()
                try FileManager.default.removeItem(at: parent)
                try Data("blocked".utf8).write(to: parent)
            }
        }
        fixture.runner.results = [
            ProcessResult(exitCode: 0, stdout: "", stderr: ""),
            ProcessResult(exitCode: 5, stdout: "", stderr: "bootstrap refused"),
        ]

        do {
            try fixture.controller.restart(template: fixture.template, binary: fixture.binary, dataDir: URL(fileURLWithPath: "/tmp/new-memory"))
            Issue.record("expected the restart and rollback to fail")
        } catch {
            #expect(error.localizedDescription.contains("could not be restored"))
        }
        #expect(fixture.runner.calls == fixture.reloadCalls)
    }
}

private struct RestartFixture {
    let home: URL
    let runner = LaunchdMockRunner()
    let binary = URL(fileURLWithPath: "/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory")
    let template: String

    init() throws {
        home = FileManager.default.temporaryDirectory.appending(path: "ai-memory-restart-\(UUID().uuidString)")
        template = try String(contentsOf: repoRoot().appending(path: "packaging/launchd/com.github.akitaonrails.ai-memory.plist"), encoding: .utf8)
    }

    var controller: LaunchdController { LaunchdController(home: home, runner: runner) }
    var domainService: String { "gui/\(getuid())/\(LaunchdController.label)" }
    var reloadCalls: [[String]] { [["bootout", domainService], ["bootstrap", "gui/\(getuid())", controller.plistDestination.path]] }

    func installedValues() throws -> [String: Any] {
        let data = try Data(contentsOf: controller.plistDestination)
        return try #require(PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any])
    }

    func remove() { try? FileManager.default.removeItem(at: home) }
}

private final class LaunchdMockRunner: CommandRunning, @unchecked Sendable {
    var calls: [[String]] = []
    var results: [ProcessResult] = []
    var onCall: (([String]) throws -> Void)?

    func run(binary: URL, arguments: [String], extraEnv: [String: String]) throws -> ProcessResult {
        #expect(binary.path == "/bin/launchctl")
        calls.append(arguments)
        try onCall?(arguments)
        return results.isEmpty ? ProcessResult(exitCode: 0, stdout: "", stderr: "") : results.removeFirst()
    }
}

private func repoRoot(file: String = #filePath) -> URL {
    URL(fileURLWithPath: file)
        .deletingLastPathComponent() // Tests/AIMemoryMenuTests
        .deletingLastPathComponent() // Tests
        .deletingLastPathComponent() // companions/ai-memory-macos
        .deletingLastPathComponent() // companions
        .deletingLastPathComponent() // repo
}
