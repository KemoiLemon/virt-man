import Foundation
import SwiftUI

struct ActivityView: View {
    let server: ServerRecord
    @EnvironmentObject private var store: ServerStore

    @State private var entries: [ActivityEntry] = []
    @State private var error: String?
    @State private var loading = false

    private var client: APIClient? { store.client(for: server) }

    var body: some View {
        List {
            if let error {
                Section {
                    Text(error).foregroundStyle(.red)
                }
            }
            if entries.isEmpty && error == nil {
                Section {
                    if loading {
                        ProgressView()
                    } else {
                        Text("No activity yet.")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            ForEach(entries) { entry in
                VStack(alignment: .leading, spacing: 3) {
                    HStack {
                        Text(entry.label)
                        Spacer()
                        Text(entry.status)
                            .font(.caption)
                            .foregroundStyle(entry.status == "ok" ? Color.secondary : Color.orange)
                    }
                    if let message = entry.message, !message.isEmpty {
                        Text(message)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    HStack(spacing: 6) {
                        if let user = entry.username, !user.isEmpty { Text(user) }
                        if let at = entry.atMs { Text(Self.date(at)) }
                    }
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                }
            }
        }
        .navigationTitle("Activity")
        .navigationBarTitleDisplayMode(.inline)
        .refreshable { await load() }
        .task { await load() }
    }

    private func load() async {
        guard let client else {
            error = "The API key for this server is missing. Delete it and add it again."
            return
        }
        loading = true
        defer { loading = false }
        do {
            entries = try await client.activity(limit: 100)
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private static func date(_ milliseconds: Double) -> String {
        let formatter = DateFormatter()
        formatter.dateStyle = .short
        formatter.timeStyle = .short
        return formatter.string(from: Date(timeIntervalSince1970: milliseconds / 1000))
    }
}
