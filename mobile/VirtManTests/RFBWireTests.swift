import XCTest
@testable import VirtMan

final class RFBWireTests: XCTestCase {
    func testReadsValues() {
        let bytes: [UInt8] = [0x01, 0x02, 0x00, 0x08, 0xff, 0xff, 0xff, 0x21]
        XCTAssertEqual(bytes.u16(0), 0x0102)
        XCTAssertEqual(bytes.u32(2), 0x0000_08ff)
        XCTAssertEqual(bytes.i32(4), -223)
    }

    func testKeysyms() {
        XCTAssertEqual(Keysym.character("A"), 0x41)
        XCTAssertEqual(Keysym.character("\n"), Keysym.enter)
        XCTAssertEqual(Keysym.character("é"), 0xe9)
        XCTAssertEqual(Keysym.character("€"), 0x0100_20ac)
    }

    func testReadReassemblesSplitChunks() async throws {
        let stream = ByteStream()
        await stream.feed([1, 2])
        let read = Task { try await stream.read(4) }
        await stream.feed([3])
        await stream.feed([4, 5])
        let first = try await read.value
        XCTAssertEqual(first, [1, 2, 3, 4])
        let second = try await stream.read(1)
        XCTAssertEqual(second, [5])
    }

    func testCloseWakesTheReader() async {
        let stream = ByteStream()
        let read = Task { try await stream.read(3) }
        await stream.close(nil)
        do {
            _ = try await read.value
            XCTFail("a closed stream must not return data")
        } catch {
            // expected
        }
    }

    func testRawAndCopyRect() throws {
        let framebuffer = Framebuffer()
        try framebuffer.resize(width: 2, height: 2)
        try framebuffer.blitRaw(
            x: 0, y: 0, width: 2, height: 1,
            bytes: [1, 2, 3, 4, 5, 6, 7, 8])
        try framebuffer.copyRect(
            dstX: 0, dstY: 1, width: 2, height: 1, srcX: 0, srcY: 0)
        XCTAssertEqual(
            framebuffer.snapshot(),
            [1, 2, 3, 4, 5, 6, 7, 8, 1, 2, 3, 4, 5, 6, 7, 8])
    }

    func testOverlappingCopyRectIsStable() throws {
        let framebuffer = Framebuffer()
        try framebuffer.resize(width: 3, height: 1)
        var pixels: [UInt8] = []
        for value in 0..<12 { pixels.append(UInt8(value)) }
        try framebuffer.blitRaw(x: 0, y: 0, width: 3, height: 1, bytes: pixels)
        // Copy pixels 0..1 into 1..2, overlapping the source.
        try framebuffer.copyRect(dstX: 1, dstY: 0, width: 2, height: 1, srcX: 0, srcY: 0)
        XCTAssertEqual(
            framebuffer.snapshot(),
            [0, 1, 2, 3, 0, 1, 2, 3, 4, 5, 6, 7])
    }

    func testOutOfBoundsRectangleIsRejected() throws {
        let framebuffer = Framebuffer()
        try framebuffer.resize(width: 2, height: 2)
        XCTAssertThrowsError(
            try framebuffer.blitRaw(
                x: 1, y: 0, width: 2, height: 1, bytes: [0, 0, 0, 0, 0, 0, 0, 0]))
        XCTAssertThrowsError(
            try framebuffer.copyRect(
                dstX: 2, dstY: 2, width: 1, height: 1, srcX: 0, srcY: 0))
    }
}
