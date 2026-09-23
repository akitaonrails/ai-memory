import ServiceManagement
import SwiftUI
import AIMemoryMenuCore

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var serverURL: String = ""
    @State private var dataDir: String = ""
    @State private var token: String = ""
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
    @State private var loginError: String?
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        Form {
            Section("Server") {
                TextField("URL", text: $serverURL)
                    .onChange(of: serverURL) { _, _ in scheduleSave() }
                    .onSubmit { persist() }
                SecureField(
                    model.tokenConfigured ? "Bearer token (saved in Keychain)" : "Bearer token (optional)",
                    text: $token
                )
                .onChange(of: token) { _, _ in scheduleSave() }
                .onSubmit { persist() }
                if model.tokenConfigured {
                    Button("Clear saved token") {
                        token = ""
                        model.saveSettings(
                            serverURLString: serverURL,
                            dataDirOverride: dataDir,
                            token: nil,
                            clearToken: true
                        )
                    }
                }
            }
            Section("Data") {
                TextField("Data directory override", text: $dataDir, prompt: Text(FirstRun.defaultDataDir().path))
                    .onChange(of: dataDir) { _, _ in scheduleSave() }
                    .onSubmit { persist() }
                Text("Empty keeps ~/Library/Application Support/ai-memory. Changing this does not move existing files.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section("Login") {
                Toggle("Launch menu bar app at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { _, enabled in
                        setLoginItem(enabled)
                    }
                if let loginError {
                    Text(loginError)
                        .font(.caption)
                        .foregroundStyle(.red)
                }
            }
        }
        .formStyle(.grouped)
        .frame(minWidth: 480, minHeight: 320)
        .onAppear {
            serverURL = model.settings.serverURL.absoluteString
            dataDir = model.settings.dataDirOverride?.path ?? ""
            launchAtLogin = SMAppService.mainApp.status == .enabled
        }
        .onDisappear {
            persist()
        }
    }

    private func scheduleSave() {
        saveTask?.cancel()
        saveTask = Task { @MainActor in
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled else { return }
            persist()
        }
    }

    private func persist() {
        saveTask?.cancel()
        model.saveSettings(
            serverURLString: serverURL,
            dataDirOverride: dataDir,
            token: token.isEmpty ? nil : token,
            clearToken: false
        )
    }

    private func setLoginItem(_ enabled: Bool) {
        let currentlyEnabled = SMAppService.mainApp.status == .enabled
        guard enabled != currentlyEnabled else { return }
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            loginError = nil
        } catch {
            loginError = error.localizedDescription
            launchAtLogin = SMAppService.mainApp.status == .enabled
        }
    }
}
