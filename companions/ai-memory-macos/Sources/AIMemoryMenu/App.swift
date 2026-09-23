import AppKit
import SwiftUI
import AIMemoryMenuCore

@main
struct AIMemoryMenuApp: App {
    @State private var model = AppModel()

    init() {
        NSApplication.shared.setActivationPolicy(.accessory)
    }

    var body: some Scene {
        MenuBarExtra {
            MenuBarView()
                .environment(model)
        } label: {
            MenuBarLabel(icon: model.icon)
        }
        .menuBarExtraStyle(.menu)

        Window("Status", id: "status") {
            StatusOutputView()
                .environment(model)
        }

        Settings {
            SettingsView()
                .environment(model)
        }
    }
}
