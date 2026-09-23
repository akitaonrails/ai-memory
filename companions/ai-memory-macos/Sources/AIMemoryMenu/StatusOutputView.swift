import AppKit
import SwiftUI

struct StatusOutputView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("ai-memory status")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    Task { await model.refreshStatusOutput() }
                }
                .disabled(model.isBusy)
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(model.statusOutput, forType: .string)
                }
                .disabled(model.statusOutput.isEmpty)
            }
            ScrollView {
                Text(model.statusOutput.isEmpty ? "Running ai-memory status…" : model.statusOutput)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .background(Color(nsColor: .textBackgroundColor))
        }
        .padding()
        .frame(minWidth: 560, minHeight: 360)
    }
}
