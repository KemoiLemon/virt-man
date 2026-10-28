import CoreGraphics
import Foundation

/// BGRA framebuffer. QEMU's 32-bit pixel format (shifts r16 g8 b0, little
/// endian) arrives as B,G,R,X bytes, which is exactly what
/// `byteOrder32Little | noneSkipFirst` expects, so a Raw rectangle is a copy.
final class Framebuffer {
    private(set) var width = 0
    private(set) var height = 0
    private var pixels: [UInt8] = []

    var isEmpty: Bool { width == 0 || height == 0 }

    func resize(width: Int, height: Int) throws {
        guard width > 0, height > 0, width <= 16384, height <= 16384 else {
            throw RFBError.protocolError("bad framebuffer size \(width)x\(height)")
        }
        self.width = width
        self.height = height
        pixels = [UInt8](repeating: 0, count: width * height * 4)
    }

    func blitRaw(x: Int, y: Int, width w: Int, height h: Int, bytes: [UInt8]) throws {
        guard w > 0, h > 0, w * h * 4 == bytes.count else {
            throw RFBError.protocolError("bad Raw rectangle \(w)x\(h)")
        }
        guard x >= 0, y >= 0, x + w <= width, y + h <= height else {
            throw RFBError.protocolError("Raw rectangle outside the framebuffer")
        }
        let rowBytes = w * 4
        for row in 0..<h {
            let source = row * rowBytes
            let target = ((y + row) * width + x) * 4
            pixels.replaceSubrange(target..<(target + rowBytes), with: bytes[source..<(source + rowBytes)])
        }
    }

    func copyRect(
        dstX: Int, dstY: Int, width w: Int, height h: Int, srcX: Int, srcY: Int
    ) throws {
        guard w > 0, h > 0 else { throw RFBError.protocolError("empty CopyRect") }
        guard srcX >= 0, srcY >= 0, srcX + w <= width, srcY + h <= height,
              dstX >= 0, dstY >= 0, dstX + w <= width, dstY + h <= height else {
            throw RFBError.protocolError("CopyRect outside the framebuffer")
        }
        // The regions may overlap, so stage the source first.
        let rowBytes = w * 4
        var staged = [UInt8](repeating: 0, count: rowBytes * h)
        for row in 0..<h {
            let start = ((srcY + row) * width + srcX) * 4
            let sourceRow = Array(pixels[start..<(start + rowBytes)])
            staged.replaceSubrange((row * rowBytes)..<((row + 1) * rowBytes), with: sourceRow)
        }
        for row in 0..<h {
            let start = ((dstY + row) * width + dstX) * 4
            let stagedRow = Array(staged[(row * rowBytes)..<((row + 1) * rowBytes)])
            pixels.replaceSubrange(start..<(start + rowBytes), with: stagedRow)
        }
    }

    /// Test visibility for the pixel buffer.
    func snapshot() -> [UInt8] { pixels }

    func makeImage() -> CGImage? {        guard width > 0, height > 0, pixels.count == width * height * 4 else { return nil }
        guard let provider = CGDataProvider(data: Data(pixels) as CFData) else { return nil }
        let info = CGBitmapInfo(
            rawValue: CGImageAlphaInfo.noneSkipFirst.rawValue
                | CGBitmapInfo.byteOrder32Little.rawValue)
        return CGImage(
            width: width,
            height: height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: info,
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        )
    }
}
