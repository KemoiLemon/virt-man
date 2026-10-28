import SwiftUI

struct ServersView: View {
    @EnvironmentObject private var store: ServerStore
    @State private var adding = false

    var body: some View {
        NavigationStack {
            Group {
                if store.servers.isEmpty {
                    VStack(spacing: 12) {
                        Image(systemName: "server.rack")
                            .font(.system(size: 44, weight: .light))
                        Text("No servers yet")
                            .font(.headline)
                        Text("Add the address of your virt-man web UI.")
                            .foregroundStyle(.secondary)
                        Button("Add Server") { adding = true }
                            .buttonStyle(.borderedProminent)
                    }
                    .padding()
                } else {
                    List {
                        ForEach(store.servers) { server in
                            NavigationLink(value: server) {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(server.name)
                                    Text(server.baseURL)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                            .swipeActions {
                                Button(role: .destructive) {
                                    store.remove(server)
                                } label: {
                                    Label("Delete", systemImage: "trash")
                                }
                            }
                        }
                    }
                }
            }
            .navigationTitle("Virt-Man")
            .toolbar {
                Button {
                    adding = true
                } label: {
                    Label("Add Server", systemImage: "plus")
                }
            }
            .sheet(isPresented: $adding) {
                AddServerView()
                    .environmentObject(store)
            }
            .navigationDestination(for: ServerRecord.self) { server in
                DaemonView(server: server)
                    .environmentObject(store)
            }
        }
    }
}
