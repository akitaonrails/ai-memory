import Foundation
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
}

private func repoRoot(file: String = #filePath) -> URL {
    URL(fileURLWithPath: file)
        .deletingLastPathComponent() // Tests/AIMemoryMenuTests
        .deletingLastPathComponent() // Tests
        .deletingLastPathComponent() // companions/ai-memory-macos
        .deletingLastPathComponent() // companions
        .deletingLastPathComponent() // repo
}
