import SwiftUI

struct DaemonView: View {
    let server: ServerRecord
    @EnvironmentObject private var store: ServerStore

    @State private var connections: [Connection] = []
    @State private var error: String?
    @State private var loading = false

    private var client: APIClient? { store.client(for: server) }

    var body: some View {
        List {
            if let error {
                Section {
                    Text(error)
                        .foregroundStyle(.red)
                }
            }
            if connections.isEmpty && error == nil {
                Section {
                    if loading {
                        ProgressView()
                    } else {
                        Text("No servers are visible to this account.")
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Section("Daemons") {
                ForEach(connections) { connection in
                    NavigationLink(value: connection) {
                        row(connection)
                    }
                }
            }
        }
        .navigationTitle(server.name)
        .refreshable { await load() }
        .task { await load() }
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                NavigationLink {
                    ActivityView(server: server)
                } label: {
                    Label("Activity", systemImage: "clock.arrow.circlepath")
                }
            }
        }
        .navigationDestination(for: Connection.self) { connection in
            GuestsView(server: server, connection: connection)
        }
    }

    private func row(_ connection: Connection) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Circle()
                    .fill(connection.isOnline ? Color.green : Color.secondary)
                    .frame(width: 8, height: 8)
                Text(connection.name)
                if connection.isOnline == nil {
                    Text("unknown")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Text(connection.url)
                .font(.caption)
                .foregroundStyle(.secondary)
            if let offline = connection.offlineError, !connection.isOnline {
                Text(offline)
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
    }

    private func load() async {
        guard let client else {
            error = "The API key for this server is missing. Delete it and add it again."
            return
        }
        loading = true
        defer { loading = false }
        do {
            connections = try await client.connections()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }
}
