import Foundation

// DTOs mirror the web API's JSON. The decoder uses .convertFromSnakeCase, so
// `memory_mb` lands in `memoryMb`. Keep these in step with web/src/api.ts.

struct Me: Codable {
    let id: String
    let username: String
    let nickname: String
    let avatar: String?
    let admin: Bool?
    let privs: [String]?
}

struct AuthStatus: Codable {
    let needsSetup: Bool
    let authDisabled: Bool
}

struct Connection: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    let url: String
    let fingerprint: String?
    let hasToken: Bool?
    let hasCert: Bool?
    let online: Bool?
    let offlineSinceMs: Double?
    let offlineError: String?

    var isOnline: Bool { online ?? false }
}

struct Guest: Codable, Identifiable, Hashable {
    /// The manager's UUID, which the proxy and the console path name it by.
    let name: String
    let uuid: String?
    let title: String?
    var state: String
    let active: Bool?
    let vcpus: Int?
    let memoryMb: Int?
    let ownerName: String?
    let mine: Bool?
    let suspended: Bool?
    /// Containers only.
    let privileged: Bool?
    let autostart: Bool?

    var id: String { name }
    var label: String {
        if let title, !title.isEmpty { return title }
        return name
    }
    var running: Bool { state == "running" || state == "paused" }
    var stateLabel: String {
        switch state {
        case "running": return "Running"
        case "paused": return "Paused"
        case "stopped": return "Stopped"
        default: return state.capitalized
        }
    }
}

struct Graphics: Codable, Hashable {
    let kind: String
    let host: String
    let port: Int
}

struct GraphicsEnvelope: Codable {
    let name: String
    let graphics: Graphics?
}

struct HostStats: Codable {
    let cpuTotal: Double?
    let cpuIdle: Double?
    let memTotalKib: Double?
    let memAvailableKib: Double?
    let cpus: Int?
    let sampledAtMs: Double?
}

struct GuestStats: Codable {
    let cpuTimeNs: Double?
    let vcpus: Int?
    let memoryKib: Double?
    let maxMemoryKib: Double?
    let rssKib: Double?
    let sampledAtMs: Double?
}

struct GuestState: Codable {
    let state: String
}

struct ActivityEntry: Codable, Identifiable {
    let id: String
    let userId: String?
    let username: String?
    let label: String
    let status: String
    let message: String?
    let atMs: Double?
}

struct ActivityEnvelope: Codable {
    let entries: [ActivityEntry]
    let allUsers: Bool?
}

struct ApiKeyResponse: Codable {
    let id: String?
    let name: String?
    let key: String?
}

/// Any JSON object, for endpoints whose body we do not use.
struct Empty: Codable {}

enum GuestKind: Equatable {
    case vm
    case container
}
