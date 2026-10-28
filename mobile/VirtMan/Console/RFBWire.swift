import Foundation

enum RFBError: LocalizedError {
    case protocolError(String)
    case unsupported(String)
    case closed

    var errorDescription: String? {
        switch self {
        case let .protocolError(detail): return "Console protocol error: \(detail)"
        case let .unsupported(detail): return detail
        case .closed: return "Console connection closed."
        }
    }
}

extension Array where Element == UInt8 {
    func u16(_ offset: Int) -> UInt16 {
        (UInt16(self[offset]) << 8) | UInt16(self[offset + 1])
    }

    func u32(_ offset: Int) -> UInt32 {
        (UInt32(self[offset]) << 24)
            | (UInt32(self[offset + 1]) << 16)
            | (UInt32(self[offset + 2]) << 8)
            | UInt32(self[offset + 3])
    }

    func i32(_ offset: Int) -> Int32 {
        Int32(bitPattern: u32(offset))
    }
}

/// RFB is a byte stream; the console WebSocket delivers arbitrary chunks. This
/// actor buffers chunks and satisfies exact-length reads, one reader at a time.
actor ByteStream {
    private var buffer: [UInt8] = []
    private var waiter: CheckedContinuation<Void, Never>?
    private var failure: Error?
    private var closed = false

    func feed(_ bytes: [UInt8]) {
        guard !closed else { return }
        buffer.append(contentsOf: bytes)
        waiter?.resume()
        waiter = nil
    }

    func close(_ error: Error?) {
        if closed { return }
        closed = true
        failure = error
        waiter?.resume()
        waiter = nil
    }

    func read(_ count: Int) async throws -> [UInt8] {
        guard count >= 0 else { throw RFBError.protocolError("negative read") }
        while true {
            if let failure { throw failure }
            if buffer.count >= count {
                let out = Array(buffer.prefix(count))
                buffer.removeFirst(count)
                return out
            }
            if closed { throw RFBError.closed }
            await withCheckedContinuation { continuation in
                waiter = continuation
            }
        }
    }
}

enum Keysym {
    static let backspace: UInt32 = 0xff08
    static let tab: UInt32 = 0xff09
    static let enter: UInt32 = 0xff0d
    static let escape: UInt32 = 0xff1b
    static let delete: UInt32 = 0xffff
    static let home: UInt32 = 0xff50
    static let left: UInt32 = 0xff51
    static let up: UInt32 = 0xff52
    static let right: UInt32 = 0xff53
    static let down: UInt32 = 0xff54
    static let pageUp: UInt32 = 0xff55
    static let pageDown: UInt32 = 0xff56
    static let end: UInt32 = 0xff57
    static let control: UInt32 = 0xffe3
    static let shift: UInt32 = 0xffe1
    static let alt: UInt32 = 0xffe9

    /// Latin-1 is its own keysym; everything else uses X11's Unicode range.
    static func character(_ character: Character) -> UInt32 {
        guard let scalar = character.unicodeScalars.first else { return 0 }
        switch character {
        case "\n", "\r": return enter
        case "\t": return tab
        default:
            return scalar.value < 0x100 ? scalar.value : 0x0100_0000 + scalar.value
        }
    }
}
