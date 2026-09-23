import Foundation
import Testing
@testable import AIMemoryMenuCore

struct RuntimeAndCliTests {
    @Test func layoutPointsAtTarballSiblings() {
        let root = URL(fileURLWithPath: "/tmp/runtime")
        let layout = RuntimeLayout(root: root)
        #expect(layout.binary.path.hasSuffix("/runtime/ai-memory"))
        #expect(layout.hooks.path.hasSuffix("/runtime/hooks"))
        #expect(layout.plistTemplate.path.hasSuffix("/packaging/launchd/com.github.akitaonrails.ai-memory.plist"))
    }

    @Test func validateRequiresBinaryHooksAndPlist() throws {
        let dir = FileManager.default.temporaryDirectory
            .appending(path: "ai-memory-menu-runtime-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }

        let layout = RuntimeLayout(root: dir)
        #expect(!layout.validate())

        try FileManager.default.createDirectory(at: layout.hooks, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(
            at: layout.plistTemplate.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try Data().write(to: layout.binary)
        try Data().write(to: layout.plistTemplate)
        #expect(!layout.validate())

        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: layout.binary.path)
        #expect(layout.validate())
    }

    @Test func resolvePrefersEnvironmentOverride() throws {
        let dir = FileManager.default.temporaryDirectory
            .appending(path: "ai-memory-menu-env-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let layout = RuntimeLayout(root: dir)
        try FileManager.default.createDirectory(at: layout.hooks, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(
            at: layout.plistTemplate.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        try Data().write(to: layout.binary)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: layout.binary.path)
        try Data().write(to: layout.plistTemplate)

        let resolved = RuntimeLayout.resolve(
            bundle: Bundle.main,
            environment: ["AI_MEMORY_MENU_RUNTIME": dir.path]
        )
        #expect(resolved?.root.path == dir.path)
    }

    @Test func needsInitOnlyWhenConfigMissing() throws {
        let dir = FileManager.default.temporaryDirectory
            .appending(path: "ai-memory-menu-init-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        #expect(FirstRun.needsInit(dataDir: dir))
        try "bind = \"127.0.0.1:49374\"\n".write(
            to: dir.appending(path: "config.toml"),
            atomically: true,
            encoding: .utf8
        )
        #expect(!FirstRun.needsInit(dataDir: dir))
    }

    @Test func bundledCLIInvokesInitAndStatus() throws {
        let runner = MockRunner()
        runner.results.append(ProcessResult(exitCode: 0, stdout: "initialized\n", stderr: ""))
        runner.results.append(ProcessResult(exitCode: 0, stdout: "ai-memory 2.3.2 (server)\n", stderr: ""))
        let cli = BundledCLI(
            runtime: RuntimeLayout(root: URL(fileURLWithPath: "/tmp/runtime")),
            runner: runner
        )
        let dataDir = URL(fileURLWithPath: "/tmp/data")
        _ = try cli.initDataDir(dataDir)
        _ = try cli.status(
            serverURL: URL(string: "http://127.0.0.1:49374")!,
            token: "secret",
            dataDir: dataDir
        )
        #expect(runner.calls.count == 2)
        #expect(runner.calls[0].arguments == ["--data-dir", "/tmp/data", "init"])
        #expect(runner.calls[1].arguments == ["--data-dir", "/tmp/data", "status"])
        #expect(runner.calls[1].extraEnv["AI_MEMORY_SERVER_URL"] == "http://127.0.0.1:49374")
        #expect(runner.calls[1].extraEnv["AI_MEMORY_AUTH_TOKEN"] == "secret")
    }

    @Test func bundledCLISurfacesNonZeroExit() {
        let runner = MockRunner()
        runner.results.append(ProcessResult(exitCode: 2, stdout: "", stderr: "could not reach server\n"))
        let cli = BundledCLI(
            runtime: RuntimeLayout(root: URL(fileURLWithPath: "/tmp/runtime")),
            runner: runner
        )
        do {
            _ = try cli.status(
                serverURL: URL(string: "http://127.0.0.1:49374")!,
                token: nil,
                dataDir: nil
            )
            Issue.record("expected failure")
        } catch let CliError.failed(code, output) {
            #expect(code == 2)
            #expect(output.contains("could not reach server"))
        } catch {
            Issue.record("wrong error \(error)")
        }
    }
}

private final class MockRunner: CommandRunning, @unchecked Sendable {
    struct Call {
        var binary: URL
        var arguments: [String]
        var extraEnv: [String: String]
    }

    var calls: [Call] = []
    var results: [ProcessResult] = []

    func run(binary: URL, arguments: [String], extraEnv: [String: String]) throws -> ProcessResult {
        calls.append(Call(binary: binary, arguments: arguments, extraEnv: extraEnv))
        if results.isEmpty {
            return ProcessResult(exitCode: 0, stdout: "", stderr: "")
        }
        return results.removeFirst()
    }
}
