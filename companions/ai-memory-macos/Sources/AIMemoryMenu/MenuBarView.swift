import AppKit
import SwiftUI
import AIMemoryMenuCore

struct MenuBarView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Text(model.headlineVersion)
            .disabled(true)
            .onAppear {
                Task { await model.poll() }
            }
        ForEach(Array(model.statisticLines.enumerated()), id: \.offset) { _, line in
            Text(line)
                .font(.system(.body, design: .monospaced))
                .disabled(true)
        }
        if model.runtimeMissing {
            Text("Runtime not bundled — run build.sh")
                .disabled(true)
        }
        Divider()
        serviceButtons
        Divider()
        Button("Open Web UI") {
            model.openWebUI()
        }
        if model.showsServerStatus {
            Button("Show Status…") {
                Task {
                    await model.refreshStatusOutput()
                    NSApp.activate(ignoringOtherApps: true)
                    openWindow(id: "status")
                }
            }
        }
        Button("Open Config") {
            model.openConfig()
        }
        Button("Open Data Directory") {
            model.openDataDirectory()
        }
        Button("Open Logs") {
            model.openLogs()
        }
        Divider()
        SettingsLink {
            Text("Settings…")
        }
        Button("Quit") {
            NSApp.terminate(nil)
        }
    }

    @ViewBuilder
    private var serviceButtons: some View {
        switch model.launchd {
        case .notInstalled:
            Button("Install & Start Server") {
                Task { await model.installAndStart() }
            }
            .disabled(model.isBusy || model.icon == .starting)
        case .stopped:
            Button("Start Server") {
                Task { await model.start() }
            }
            .disabled(model.isBusy || model.icon == .starting)
        case .running:
            Button("Stop Server") {
                Task { await model.stop() }
            }
            .disabled(model.isBusy)
            Button("Restart Server") {
                Task { await model.restart() }
            }
            .disabled(model.isBusy || model.icon == .starting)
        }
    }
}

struct MenuBarLabel: View {
    var icon: IconState

    var body: some View {
        // Menu extras flatten SwiftUI tint to a template image, so a
        // non-template NSImage is what actually changes with server state.
        Image(nsImage: StatusDot.image(for: icon))
            .accessibilityLabel(icon.accessibilityLabel)
    }
}

enum StatusDot {
    static func image(for icon: IconState) -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let image = NSImage(size: size, flipped: false) { rect in
            let inset = rect.insetBy(dx: 3, dy: 3)
            color(for: icon).setFill()
            NSBezierPath(ovalIn: inset).fill()
            if icon == .unknown || icon == .notInstalled {
                NSColor.windowBackgroundColor.setStroke()
                let stroke = NSBezierPath(ovalIn: inset.insetBy(dx: 0.5, dy: 0.5))
                stroke.lineWidth = 1
                stroke.stroke()
            }
            return true
        }
        image.isTemplate = false
        return image
    }

    private static func color(for icon: IconState) -> NSColor {
        switch icon {
        case .ok:
            NSColor.systemGreen
        case .starting:
            NSColor.systemOrange
        case .degraded, .authRequired:
            NSColor.systemYellow
        case .unreachable:
            NSColor.systemRed
        case .notInstalled, .unknown:
            NSColor.systemGray
        }
    }
}
