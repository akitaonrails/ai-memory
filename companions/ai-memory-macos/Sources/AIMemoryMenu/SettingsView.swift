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

    var body: some View {
        Form {
            Section("Server") {
                // Edits apply on Return or when the window closes, never while
                // typing, so a half-typed URL never briefly becomes the server
                // the token is sent to.
                TextField("URL", text: $serverURL)
                    .onSubmit { persist() }
                SecureField(
                    model.tokenConfigured
                        ? "Bearer token (saved in Keychain)"
                        : model.dataDirTokenPresent
                            ? "Bearer token (using auth-tokens/default)"
                            : "Bearer token (optional)",
                    text: $token
                )
                .onSubmit { persist() }
                if model.savedTokenIsForAnotherServer {
                    Text("The saved token was set for another server, so it is not being sent to this one. Enter a token for this server, or clear it.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
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

    private func persist() {
        model.saveSettings(
            serverURLString: serverURL,
            dataDirOverride: dataDir,
            token: token.isEmpty ? nil : token,
            clearToken: false
        )
        // Forget the typed token once it is saved so it cannot be re-bound to
        // the next URL the field is submitted with.
        token = ""
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
