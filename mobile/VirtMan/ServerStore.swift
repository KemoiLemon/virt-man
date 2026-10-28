import Combine
import Foundation

/// A web UI the app knows how to reach. The API key is not in here; it lives
/// in the keychain (or its protected-file fallback) under `id`.
struct ServerRecord: Codable, Identifiable, Hashable {
    var id: UUID
    var name: String
    var baseURL: String
    var pin: String
}

final class ServerStore: ObservableObject {
    @Published private(set) var servers: [ServerRecord] = []

    private let defaultsKey = "virtman.servers"
    /// One client (and one URLSession) per server, kept for the store's life:
    /// a session is only released when invalidated, so making a fresh one per
    /// poll would leak a session every few seconds.
    private var clients: [UUID: APIClient] = [:]

    init() {
        if let data = UserDefaults.standard.data(forKey: defaultsKey),
           let saved = try? JSONDecoder().decode([ServerRecord].self, from: data) {
            servers = saved
        }
    }

    func add(_ server: ServerRecord, key: String) {
        servers.append(server)
        SecretStore.save(key, account: server.id.uuidString)
        clients[server.id] = nil
        save()
    }

    func remove(_ server: ServerRecord) {
        servers.removeAll { $0.id == server.id }
        SecretStore.delete(account: server.id.uuidString)
        clients[server.id] = nil
        save()
    }

    func key(for server: ServerRecord) -> String? {
        SecretStore.load(account: server.id.uuidString)
    }

    func client(for server: ServerRecord) -> APIClient? {
        if let client = clients[server.id] { return client }
        guard let key = key(for: server), let base = URL(string: server.baseURL) else { return nil }
        let client = APIClient(base: base, pin: server.pin, key: key)
        clients[server.id] = client
        return client
    }

    private func save() {
        if let data = try? JSONEncoder().encode(servers) {
            UserDefaults.standard.set(data, forKey: defaultsKey)
        }
    }
}
