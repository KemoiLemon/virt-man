import Combine
import CoreGraphics
import Foundation

/// Minimal RFB 3.8 client for the daemon's VNC console. The web server splices
/// the WebSocket to QEMU's VNC socket, so the protocol is plain RFB over
/// WebSocket binary messages; the byte boundaries are not message boundaries.
final class RFBClient: ObservableObject {
    @Published private(set) var image: CGImage?
    @Published private(set) var status = "Connecting…"
    @Published private(set) var connected = false
    @Published private(set) var size = CGSize.zero

    private var session: URLSession?
    private var task: URLSessionWebSocketTask?
    private var writer: SocketWriter?
    private var runTask: Task<Void, Never>?
    private var currentStream: ByteStream?
    private var key: String?

    func start(url: URL, pin: String, key: String) {
        stop()
        self.key = key
        image = nil
        size = .zero
        connected = false
        status = "Connecting…"
        runTask = Task { [weak self] in
            guard let self else { return }
            await self.run(url: url, pin: pin)
        }
    }

    func stop() {
        let stream = currentStream
        currentStream = nil
        runTask?.cancel()
        runTask = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        session?.invalidateAndCancel()
        session = nil
        writer = nil
        connected = false
        Task { await stream?.close(CancellationError()) }
    }

    // MARK: - Input

    func pointer(x: Int, y: Int, mask: Int) async {
        guard let writer else { return }
        let bytes: [UInt8] = [
            5, UInt8(mask & 0xff),
            UInt8((x >> 8) & 0xff), UInt8(x & 0xff),
            UInt8((y >> 8) & 0xff), UInt8(y & 0xff),
        ]
        try? await writer.send(Data(bytes))
    }

    func key(down: Bool, sym: UInt32) async {
        guard let writer else { return }
        let bytes: [UInt8] = [
            4, down ? 1 : 0, 0, 0,
            UInt8((sym >> 24) & 0xff), UInt8((sym >> 16) & 0xff),
            UInt8((sym >> 8) & 0xff), UInt8(sym & 0xff),
        ]
        try? await writer.send(Data(bytes))
    }

    func type(_ text: String) async {
        for character in text {
            let sym = Keysym.character(character)
            await key(down: true, sym: sym)
            await key(down: false, sym: sym)
        }
    }

    func chord(_ keys: [UInt32]) async {
        for sym in keys { await key(down: true, sym: sym) }
        for sym in keys.reversed() { await key(down: false, sym: sym) }
    }

    // MARK: - Session

    private func run(url: URL, pin: String) async {
        // Every run gets its own stream and framebuffer: a retry must not let
        // the old socket's bytes or a stale framebuffer leak into the new one.
        let stream = ByteStream()
        let framebuffer = Framebuffer()
        currentStream = stream

        let trust = PinnedTrustDelegate(pin: pin)
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = 30
        let session = URLSession(configuration: config, delegate: trust, delegateQueue: nil)
        self.session = session

        var request = URLRequest(url: url)
        if let key { request.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization") }
        // No Sec-WebSocket-Protocol: URLSession cannot request one, and the
        // proxy only echoes `binary` when offered — it does not require it.

        let task = session.webSocketTask(with: request)
        let writer = SocketWriter(task: task)
        self.task = task
        self.writer = writer
        task.resume()
        receive(task, stream: stream)

        do {
            try await handshake(stream: stream, framebuffer: framebuffer, writer: writer)
            await MainActor.run {
                self.connected = true
                self.status = ""
            }
            try await loop(stream: stream, framebuffer: framebuffer, writer: writer)
            await MainActor.run {
                if self.connected { self.status = "Console disconnected." }
            }
        } catch {
            if !Task.isCancelled {
                await MainActor.run { self.status = error.localizedDescription }
            }
        }
        await MainActor.run { self.connected = false }
    }

    private func receive(_ task: URLSessionWebSocketTask, stream: ByteStream) {
        task.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case let .success(message):
                let bytes: [UInt8]
                switch message {
                case let .data(data): bytes = [UInt8](data)
                case let .string(text): bytes = [UInt8](text.utf8)
                @unknown default: bytes = []
                }
                Task { await stream.feed(bytes) }
                self.receive(task, stream: stream)
            case let .failure(error):
                Task { await stream.close(error) }
            }
        }
    }

    // MARK: - Wire

    private func read(_ count: Int, from stream: ByteStream) async throws -> [UInt8] {
        try await stream.read(count)
    }

    private func write(_ bytes: [UInt8], to writer: SocketWriter) async throws {
        try await writer.send(Data(bytes))
    }

    private func handshake(
        stream: ByteStream, framebuffer: Framebuffer, writer: SocketWriter
    ) async throws {
        let banner = try await read(12, from: stream)
        let text = String(bytes: banner, encoding: .ascii) ?? ""
        guard text.hasPrefix("RFB 003.") else {
            throw RFBError.protocolError("the console did not speak RFB")
        }
        let minor = Int(text.dropFirst(8).prefix(3)) ?? 8

        switch minor {
        case 8...:
            try await write(Array("RFB 003.008\n".utf8), to: writer)
        case 7:
            try await write(Array("RFB 003.007\n".utf8), to: writer)
        default:
            try await write(Array("RFB 003.003\n".utf8), to: writer)
        }

        if minor >= 7 {
            let countBytes = try await read(1, from: stream)
            let count = Int(countBytes[0])
            guard count > 0 else { throw RFBError.protocolError("no security types offered") }
            let types = try await read(count, from: stream)
            guard types.contains(1) else {
                throw RFBError.unsupported(
                    "This guest's VNC server asks for a password, which is not supported.")
            }
            try await write([1], to: writer)
            let result = try await read(4, from: stream)
            if result.u32(0) != 0 {
                let lengthBytes = try await read(4, from: stream)
                let reasonLength = Int(lengthBytes.u32(0))
                var reason: String?
                if reasonLength > 0 {
                    reason = String(
                        bytes: try await read(reasonLength, from: stream), encoding: .utf8)
                }
                throw RFBError.unsupported(reason ?? "The VNC server refused the connection.")
            }
        } else {
            let typeBytes = try await read(4, from: stream)
            guard typeBytes.u32(0) == 1 else {
                throw RFBError.unsupported(
                    "This guest's VNC server asks for a password, which is not supported.")
            }
        }

        try await write([1], to: writer) // ClientInit: shared

        let header = try await read(24, from: stream)
        let width = Int(header.u16(0))
        let height = Int(header.u16(2))
        let nameLength = Int(header.u32(20))
        try framebuffer.resize(width: width, height: height)
        if nameLength > 0 { _ = try await read(nameLength, from: stream) }

        await MainActor.run {
            self.size = CGSize(width: width, height: height)
        }

        try await write(pixelFormat, to: writer)
        try await write(encodings, to: writer)
        try await updateRequest(incremental: false, stream: stream, framebuffer: framebuffer, writer: writer)
    }

    private func loop(
        stream: ByteStream, framebuffer: Framebuffer, writer: SocketWriter
    ) async throws {
        while !Task.isCancelled {
            let typeBytes = try await read(1, from: stream)
            switch typeBytes[0] {
            case 0:
                try await framebufferUpdate(
                    stream: stream, framebuffer: framebuffer, writer: writer)
            case 1:
                // SetColourMapEntries is not used by our pixel format; consume
                // it so the stream stays aligned.
                let header = try await read(6, from: stream)
                let count = Int(header.u16(4))
                if count > 0 { _ = try await read(count * 6, from: stream) }
            case 2:
                break
            case 3:
                let header = try await read(8, from: stream)
                let length = Int(header.u32(4))
                guard length <= 1_048_576 else {
                    throw RFBError.protocolError("oversized clipboard message")
                }
                if length > 0 { _ = try await read(length, from: stream) }
            default:
                throw RFBError.protocolError("unexpected message type \(typeBytes[0])")
            }
        }
    }

    private func framebufferUpdate(
        stream: ByteStream, framebuffer: Framebuffer, writer: SocketWriter
    ) async throws {
        let header = try await read(4, from: stream)
        let count = Int(header.u16(2))
        for _ in 0..<count {
            let rectangle = try await read(12, from: stream)
            let x = Int(rectangle.u16(0))
            let y = Int(rectangle.u16(2))
            let width = Int(rectangle.u16(4))
            let height = Int(rectangle.u16(6))
            switch rectangle.i32(8) {
            case 0:
                guard width > 0, height > 0, width <= 16384, height <= 16384,
                      width * height <= 16_000_000 else {
                    throw RFBError.protocolError("bad Raw rectangle \(width)x\(height)")
                }
                guard x >= 0, y >= 0,
                      x + width <= framebuffer.width, y + height <= framebuffer.height else {
                    throw RFBError.protocolError("Raw rectangle outside the framebuffer")
                }
                let bytes = try await read(width * height * 4, from: stream)
                try framebuffer.blitRaw(
                    x: x, y: y, width: width, height: height, bytes: bytes)
            case 1:
                let source = try await read(4, from: stream)
                try framebuffer.copyRect(
                    dstX: x, dstY: y, width: width, height: height,
                    srcX: Int(source.u16(0)), srcY: Int(source.u16(2)))
            case -223:
                try framebuffer.resize(width: width, height: height)
                await MainActor.run {
                    self.size = CGSize(width: width, height: height)
                }
            default:
                throw RFBError.protocolError("unsupported encoding \(rectangle.i32(8))")
            }
        }
        try await updateRequest(
            incremental: true, stream: stream, framebuffer: framebuffer, writer: writer)
        let next = framebuffer.makeImage()
        await MainActor.run { self.image = next }
    }

    private func updateRequest(
        incremental: Bool, stream: ByteStream, framebuffer: Framebuffer, writer: SocketWriter
    ) async throws {
        let width = framebuffer.width
        let height = framebuffer.height
        let bytes: [UInt8] = [
            3, incremental ? 1 : 0, 0, 0, 0, 0,
            UInt8((width >> 8) & 0xff), UInt8(width & 0xff),
            UInt8((height >> 8) & 0xff), UInt8(height & 0xff),
        ]
        try await write(bytes, to: writer)
    }

    /// 32 bpp, depth 24, true colour, r16 g8 b0: B,G,R,X in memory.
    private let pixelFormat: [UInt8] = [
        0, 0, 0, 0,
        32, 24, 0, 1,
        0, 255, 0, 255, 0, 255,
        16, 8, 0,
        0, 0, 0,
    ]

    /// Raw, CopyRect, DesktopSize. Raw-only is a deliberate shortcut; adding
    /// ZRLE is the next step if a busy desktop feels slow.
    private let encodings: [UInt8] = [
        2, 0, 0, 3,
        0, 0, 0, 0,
        0, 0, 0, 1,
        0xff, 0xff, 0xff, 0x21,
    ]
}

private actor SocketWriter {
    private let task: URLSessionWebSocketTask

    init(task: URLSessionWebSocketTask) {
        self.task = task
    }

    func send(_ data: Data) async throws {
        // Callback form on purpose: it is available on every supported SDK,
        // unlike the async overload.
        try await withCheckedThrowingContinuation {
            (continuation: CheckedContinuation<Void, Error>) in
            task.send(.data(data)) { error in
                if let error {
                    continuation.resume(throwing: error)
                } else {
                    continuation.resume()
                }
            }
        }
    }
}
