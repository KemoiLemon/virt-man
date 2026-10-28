import SwiftUI
import UIKit

struct ConsoleScreen: View {
    let connection: Connection
    let guest: Guest
    let client: APIClient
    let pin: String
    let key: String

    @StateObject private var rfb = RFBClient()
    @State private var ctrl = false
    @State private var alt = false
    @State private var showType = false

    var body: some View {
        VStack(spacing: 0) {
            ZStack {
                Color.black
                if let image = rfb.image {
                    ScreenView(image: image, client: rfb)
                } else {
                    VStack(spacing: 10) {
                        ProgressView().tint(.white)
                        Text(rfb.status.isEmpty ? "Waiting for the console…" : rfb.status)
                            .foregroundStyle(.white)
                            .multilineTextAlignment(.center)
                            .padding()
                    }
                }
                if rfb.image != nil && !rfb.connected {
                    VStack {
                        Spacer()
                        Text(rfb.status.isEmpty ? "Console disconnected." : rfb.status)
                            .font(.caption)
                            .foregroundStyle(.white)
                            .padding(8)
                            .background(.black.opacity(0.75))
                    }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            controls
        }
        .navigationTitle(guest.label)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .primaryAction) {
                Button {
                    connect()
                } label: {
                    Label("Reconnect", systemImage: "arrow.clockwise")
                }
            }
        }
        .task { connect() }
        .onDisappear { rfb.stop() }
        .sheet(isPresented: $showType) {
            TypeSheet { text in
                Task { await rfb.type(text) }
            }
        }
    }

    private var controls: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                keyButton("Esc", Keysym.escape)
                keyButton("Tab", Keysym.tab)
                keyButton("←", Keysym.left)
                keyButton("↑", Keysym.up)
                keyButton("↓", Keysym.down)
                keyButton("→", Keysym.right)
                modifier("Ctrl", isOn: $ctrl)
                modifier("Alt", isOn: $alt)
                Button("Ctrl+Alt+Del") {
                    Task { await rfb.chord([Keysym.control, Keysym.alt, Keysym.delete]) }
                }
                .buttonStyle(.bordered)
                Button("Type…") { showType = true }
                    .buttonStyle(.bordered)
            }
            .padding(.horizontal)
            .padding(.vertical, 6)
        }
        .background(.bar)
    }

    private func keyButton(_ label: String, _ sym: UInt32) -> some View {
        Button(label) {
            var chord: [UInt32] = []
            if ctrl { chord.append(Keysym.control) }
            if alt { chord.append(Keysym.alt) }
            chord.append(sym)
            Task {
                await rfb.chord(chord)
                await MainActor.run {
                    ctrl = false
                    alt = false
                }
            }
        }
        .buttonStyle(.bordered)
    }

    private func modifier(_ label: String, isOn: Binding<Bool>) -> some View {
        Button(label) { isOn.wrappedValue.toggle() }
            .buttonStyle(.borderedProminent)
            .tint(isOn.wrappedValue ? Color.accentColor : Color.secondary)
    }

    private func connect() {
        guard let url = client.consoleURL(conn: connection.id, guest: guest.name) else { return }
        rfb.start(url: url, pin: pin, key: key)
    }
}

private struct TypeSheet: View {
    let send: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""

    var body: some View {
        NavigationStack {
            Form {
                TextField("Text to type into the guest", text: $text, axis: .vertical)
                    .lineLimit(3...6)
            }
            .navigationTitle("Type")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Send") {
                        send(text)
                        dismiss()
                    }
                    .disabled(text.isEmpty)
                }
            }
        }
        .presentationDetents([.medium])
    }
}

struct ScreenView: UIViewRepresentable {
    let image: CGImage
    let client: RFBClient

    func makeUIView(context: Context) -> VNCScreenView {
        let view = VNCScreenView()
        view.client = client
        return view
    }

    func updateUIView(_ view: VNCScreenView, context: Context) {
        view.client = client
        view.setImage(image)
    }
}

final class VNCScreenView: UIView {
    weak var client: RFBClient?
    private var guestSize = CGSize.zero

    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = true
        backgroundColor = .black
        layer.contentsGravity = .resizeAspect
        layer.magnificationFilter = .linear
        layer.backgroundColor = UIColor.black.cgColor

        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        addGestureRecognizer(tap)
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
        pan.maximumNumberOfTouches = 1
        addGestureRecognizer(pan)
        let long = UILongPressGestureRecognizer(target: self, action: #selector(longPressed(_:)))
        addGestureRecognizer(long)
        tap.require(toFail: long)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    func setImage(_ image: CGImage) {
        guestSize = CGSize(width: image.width, height: image.height)
        layer.contents = image
    }

    private func guestPoint(_ location: CGPoint) -> (Int, Int)? {
        guard guestSize.width > 0, guestSize.height > 0 else { return nil }
        let scale = min(bounds.width / guestSize.width, bounds.height / guestSize.height)
        guard scale > 0 else { return nil }
        let drawnWidth = guestSize.width * scale
        let drawnHeight = guestSize.height * scale
        let x = (location.x - (bounds.width - drawnWidth) / 2) / scale
        let y = (location.y - (bounds.height - drawnHeight) / 2) / scale
        guard x >= 0, y >= 0, x < guestSize.width, y < guestSize.height else { return nil }
        return (Int(x), Int(y))
    }

    @objc private func tapped(_ gesture: UITapGestureRecognizer) {
        guard let (x, y) = guestPoint(gesture.location(in: self)), let client else { return }
        Task {
            await client.pointer(x: x, y: y, mask: 0)
            await client.pointer(x: x, y: y, mask: 1)
            await client.pointer(x: x, y: y, mask: 0)
        }
    }

    @objc private func panned(_ gesture: UIPanGestureRecognizer) {
        guard let (x, y) = guestPoint(gesture.location(in: self)), let client else { return }
        switch gesture.state {
        case .began:
            Task {
                await client.pointer(x: x, y: y, mask: 0)
                await client.pointer(x: x, y: y, mask: 1)
            }
        case .changed:
            Task { await client.pointer(x: x, y: y, mask: 1) }
        case .ended, .cancelled, .failed:
            Task { await client.pointer(x: x, y: y, mask: 0) }
        default:
            break
        }
    }

    @objc private func longPressed(_ gesture: UILongPressGestureRecognizer) {
        guard gesture.state == .began,
              let (x, y) = guestPoint(gesture.location(in: self)),
              let client else { return }
        Task {
            await client.pointer(x: x, y: y, mask: 4)
            await client.pointer(x: x, y: y, mask: 0)
        }
    }
}
