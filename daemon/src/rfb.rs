//! Native RFB (VNC) client — no external VNC viewer, no vnc crate.
//!
//! Ported from vm-man, where this same client backs its console. Speaks RFB
//! 3.8 against the hypervisor's VNC server, keeps the guest framebuffer in
//! memory here on the daemon, and hands raw pixels to whichever UI asks.
//!
//! The daemon runs on the virtualisation host, so the socket to the guest's
//! VNC port is local: the transport is tuned for latency rather than
//! bandwidth, and the changed rectangle ships as raw RGBA with no image
//! compression. Compressing cost more time than sending it uncompressed.
//!
//! **RGBA, not BGRA**, and that matters: `raw_to_rgba` already converts the
//! guest's BGRX wire bytes once, and `Update::pixels` carries that framebuffer
//! out unchanged, because RGBA is exactly what a canvas `ImageData` wants. A
//! reader who believes the bytes are BGRA will "fix" the channels in the
//! viewer and swap them a second time — which renders blue as red. Keep the
//! vocabulary straight: BGRX is the wire, RGBA is ours.
//!
//! Encodings: Raw, CopyRect, and the DesktopSize pseudo-encoding so a guest
//! resolution change is absorbed here instead of resizing anything locally.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

pub type R<T> = Result<T, String>;

// ------------------------------------------------------------------ session

pub struct Frame {
    /// RGBA, `w * h * 4` bytes.
    pub rgba: Vec<u8>,
    pub w: u16,
    pub h: u16,
    /// Bumped on every completed update so the UI can skip unchanged frames.
    pub seq: u64,
    /// One flag per `TILE`x`TILE` cell, row-major, `tile_w * tile_h` long. Lets
    /// the viewer receive only what actually moved instead of a whole screen.
    dirty: Vec<bool>,
    dirty_any: bool,
    tile_w: usize,
    tile_h: usize,
    /// `seq` the damage was accumulated from; a viewer further behind than
    /// this must take a full frame.
    damage_base: u64,
}

/// Damage tile size, in pixels. See [`Frame::touch`].
const TILE: u16 = 64;

impl Frame {
    /// A blank framebuffer of `w`x`h`, with its tile grid sized to match. The
    /// grid must be rebuilt whenever the size changes, or damage lands on the
    /// wrong tiles after a guest resolution change.
    fn sized(w: u16, h: u16) -> Frame {
        let (w, h) = (w.max(1), h.max(1));
        let tile_w = (w as usize).div_ceil(TILE as usize);
        let tile_h = (h as usize).div_ceil(TILE as usize);
        Frame {
            rgba: vec![0u8; w as usize * h as usize * 4],
            w,
            h,
            seq: 0,
            dirty: vec![false; tile_w * tile_h],
            dirty_any: false,
            tile_w,
            tile_h,
            damage_base: 0,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// Pixels in the rectangle. Used by the damage tests to compare a tiled
    /// update against the screen it covers.
    #[cfg(test)]
    fn area(self) -> u32 {
        self.w as u32 * self.h as u32
    }
}

impl Frame {
    /// Record a changed region, merged into the pending damage.
    ///
    /// Damage is kept as a *tile grid*, not one bounding box. A bounding box is
    /// what this used to do, and it made a keypress at the top-left plus one at
    /// the bottom-right cost a whole-screen transfer: the union of two distant
    /// rectangles is the screen. Tiles bound the cost by what actually changed.
    ///
    /// 64px tiles, because they are fine enough that an idle desktop's clock
    /// costs one tile, and coarse enough that a repainting window does not
    /// produce hundreds of rectangles to encode and paint separately.
    fn touch(&mut self, r: Rect) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        let x0 = (r.x / TILE) as usize;
        let y0 = (r.y / TILE) as usize;
        let x1 = ((r.x as u32 + r.w as u32 - 1) / TILE as u32) as usize;
        let y1 = ((r.y as u32 + r.h as u32 - 1) / TILE as u32) as usize;
        let (tw, th) = (self.tile_w, self.tile_h);
        for ty in y0..=y1.min(th.saturating_sub(1)) {
            for tx in x0..=x1.min(tw.saturating_sub(1)) {
                if ty * tw + tx < self.dirty.len() {
                    self.dirty[ty * tw + tx] = true;
                }
            }
        }
        self.dirty_any = true;
    }

    /// Forget all pending damage and return the tiles that were set, as merged
    /// rectangles. Called once per delivered update.
    fn take_damage(&mut self) -> Vec<Rect> {
        if !self.dirty_any {
            return Vec::new();
        }
        let (tw, th) = (self.tile_w, self.tile_h);
        let mut out = Vec::new();
        // Merge each run of dirty tiles on a row, then merge identical runs
        // down the rows. Without this a repaint is N rectangles; with it, the
        // common case (a rectangle several tiles across) is one.
        let mut open: Vec<(usize, usize, Rect)> = Vec::new(); // tx0, tx1, rect
        for ty in 0..th {
            let mut runs: Vec<(usize, usize)> = Vec::new();
            let mut tx = 0;
            while tx < tw {
                if self.dirty[ty * tw + tx] {
                    let start = tx;
                    while tx < tw && self.dirty[ty * tw + tx] {
                        tx += 1;
                    }
                    runs.push((start, tx));
                } else {
                    tx += 1;
                }
            }
            let mut next: Vec<(usize, usize, Rect)> = Vec::new();
            for (a, b) in runs {
                // Extend a rectangle from the previous row when its tx range
                // matches exactly, so a solid block becomes one rect.
                if let Some(i) = open.iter().position(|(oa, ob, _)| *oa == a && *ob == b) {
                    let (oa, ob, mut r) = open.remove(i);
                    r.h = (ty as u16 + 1) * TILE - r.y;
                    next.push((oa, ob, r));
                } else {
                    next.push((
                        a,
                        b,
                        Rect {
                            x: (a as u16) * TILE,
                            y: (ty as u16) * TILE,
                            w: ((b - a) as u16) * TILE,
                            h: TILE,
                        },
                    ));
                }
            }
            // Rows that did not continue are done.
            for (_, _, r) in open.drain(..) {
                out.push(r);
            }
            open = next;
        }
        for (_, _, r) in open.drain(..) {
            out.push(r);
        }
        self.dirty.iter_mut().for_each(|d| *d = false);
        self.dirty_any = false;
        out
    }
}

/// One update ready for the viewer: raw RGBA pixels, one buffer per rectangle.
///
/// Parallel arrays rather than a struct per rect: the pixels are the payload and
/// the rects are the framing, and both go out in one JSON response.
pub struct Update {
    pub seq: u64,
    pub w: u16,
    pub h: u16,
    /// Each rectangle, and `pixels[i]` its row-major RGBA (`rects[i].w *
    /// rects[i].h * 4` bytes) — already converted from the guest's BGRX by
    /// `raw_to_rgba`, so a consumer must not convert again.
    pub rects: Vec<Rect>,
    pub pixels: Vec<Vec<u8>>,
}

/// Most rectangles one update will carry. Past this the whole screen goes as a
/// single rect: each rectangle costs the viewer a separate `putImageData`, and a
/// thousand of those is slower than one full paint. See `next_update`.
const MAX_RECTS: usize = 24;

pub struct Session {
    pub frame: Mutex<Frame>,
    /// Signalled whenever `frame.seq` advances, so a waiting viewer wakes the
    /// instant a frame lands instead of polling for it.
    frame_ready: Condvar,
    /// Write half of the socket; None once the session is closed.
    tx: Mutex<Option<TcpStream>>,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Session {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    /// Block until a frame newer than `since` exists, then return the changed
    /// rectangles as raw pixels.
    ///
    /// This is the whole latency story: the request is already parked inside
    /// the backend when the frame lands, so the pixels go out on the same hop
    /// that was already waiting. No poll interval and no second round trip.
    ///
    /// The cap on the number of rectangles exists because each one is a
    /// separate `putImageData` in the viewer, and a pathological repaint (a
    /// guest scrolling a textured window) can dirty nearly every tile. Past
    /// [`MAX_RECTS`] the whole screen is sent as one rect instead: one big
    /// paint is cheaper to ship than a thousand small ones, and it cannot
    /// tear.
    pub fn next_update(&self, since: u64, timeout: Duration) -> Option<Update> {
        let mut f = self.frame.lock().unwrap();
        if f.seq <= since {
            let (guard, _) = self
                .frame_ready
                .wait_timeout_while(f, timeout, |f| f.seq <= since && self.is_running())
                .ok()?;
            f = guard;
        }
        if f.seq <= since {
            return None; // timed out: let the caller re-arm
        }

        // A viewer that missed the window the damage was tracked from cannot
        // be patched incrementally; give it the whole screen. Same when the
        // damage is too fragmented to be worth sending piecemeal.
        let full = Rect {
            x: 0,
            y: 0,
            w: f.w,
            h: f.h,
        };
        let mut rects = if since >= f.damage_base {
            f.take_damage()
        } else {
            f.dirty_any = false;
            Vec::new()
        };
        if rects.is_empty() || rects.len() > MAX_RECTS {
            rects = vec![full];
        }
        let rects: Vec<Rect> = rects
            .into_iter()
            .map(|r| clamp_rect(r, f.w, f.h))
            .filter(|r| r.w > 0 && r.h > 0)
            .collect();
        let rects = if rects.is_empty() { vec![full] } else { rects };

        let pixels = rects
            .iter()
            .map(|rect| {
                // Copy just this rectangle out of the framebuffer.
                let (fw, rw, rh) = (f.w as usize, rect.w as usize, rect.h as usize);
                let mut px = vec![0u8; rw * rh * 4];
                for row in 0..rh {
                    let src = ((rect.y as usize + row) * fw + rect.x as usize) * 4;
                    let dst = row * rw * 4;
                    let n = rw * 4;
                    if src + n <= f.rgba.len() {
                        px[dst..dst + n].copy_from_slice(&f.rgba[src..src + n]);
                    }
                }
                px
            })
            .collect();

        // Everything pending has now been delivered, so the next update only
        // carries what changes from here on. A viewer that falls behind this
        // base is given a full screen instead, so this cannot lose pixels.
        f.damage_base = f.seq;

        Some(Update {
            seq: f.seq,
            w: f.w,
            h: f.h,
            rects,
            pixels,
        })
    }

    fn fail(&self, e: String) {
        *self.error.lock().unwrap() = Some(e);
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn close(&self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(s) = self.tx.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn send(&self, bytes: &[u8]) -> R<()> {
        let mut g = self.tx.lock().unwrap();
        let s = g.as_mut().ok_or("VNC session is closed")?;
        s.write_all(bytes).map_err(|e| format!("vnc write: {e}"))
    }

    /// PointerEvent (msg 5). Coordinates are guest pixels.
    pub fn pointer(&self, x: u16, y: u16, buttons: u8) -> R<()> {
        let mut b = [0u8; 6];
        b[0] = 5;
        b[1] = buttons;
        b[2..4].copy_from_slice(&x.to_be_bytes());
        b[4..6].copy_from_slice(&y.to_be_bytes());
        self.send(&b)
    }

    /// KeyEvent (msg 4).
    pub fn key(&self, keysym: u32, down: bool) -> R<()> {
        let mut b = [0u8; 8];
        b[0] = 4;
        b[1] = down as u8;
        b[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.send(&b)
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Session>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<Session>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn get(id: &str) -> Option<Arc<Session>> {
    registry().lock().unwrap().get(id).cloned()
}

pub fn disconnect(id: &str) {
    if let Some(s) = registry().lock().unwrap().remove(id) {
        s.close();
    }
}

/// VM ids with a live session. Closing the console window does NOT end the
/// session, so the framebuffer keeps updating and reopening is instant.
pub fn active_ids() -> Vec<String> {
    let reg = registry().lock().unwrap();
    let mut v: Vec<String> = reg
        .iter()
        .filter(|(_, s)| s.is_running())
        .map(|(k, _)| k.clone())
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- handshake

fn read_exact(s: &mut TcpStream, n: usize) -> R<Vec<u8>> {
    let mut v = vec![0u8; n];
    s.read_exact(&mut v).map_err(|e| format!("vnc read: {e}"))?;
    Ok(v)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Byte offsets of R/G/B inside each little-endian 32-bit pixel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixOrder {
    pub r: usize,
    pub g: usize,
    pub b: usize,
}

/// We always ask the server for 32bpp true colour; this is the layout that
/// implies. Shifts are bit positions, so /8 gives the little-endian byte index.
pub const OUR_FORMAT: PixOrder = PixOrder { r: 2, g: 1, b: 0 };

/// Convert one Raw rectangle (32bpp, little-endian, `ord` layout) into RGBA.
pub fn raw_to_rgba(src: &[u8], ord: PixOrder, out: &mut [u8]) {
    debug_assert_eq!(src.len(), out.len());
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        d[0] = s[ord.r];
        d[1] = s[ord.g];
        d[2] = s[ord.b];
        d[3] = 255;
    }
}

/// Blit a decoded rectangle into the framebuffer at (x, y).
pub fn blit(fb: &mut [u8], fb_w: u16, x: u16, y: u16, w: u16, h: u16, rect: &[u8]) {
    let (fb_w, x, y, w) = (fb_w as usize, x as usize, y as usize, w as usize);
    for row in 0..h as usize {
        let dst = ((y + row) * fb_w + x) * 4;
        let src = row * w * 4;
        let n = w * 4;
        if dst + n <= fb.len() && src + n <= rect.len() {
            fb[dst..dst + n].copy_from_slice(&rect[src..src + n]);
        }
    }
}

/// Blit, and report the sub-rectangle whose pixels actually changed.
///
/// Returns `None` when the incoming rectangle is identical to what is already
/// on screen, which is the common case for a full-screen Raw update where only
/// the cursor moved. Comparing is a cheap memcmp per row and saves sending
/// megabytes that did not change.
pub fn blit_changed(
    fb: &mut [u8],
    fb_w: u16,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    rect: &[u8],
) -> Option<Rect> {
    let (fbw, xs, ys, ws) = (fb_w as usize, x as usize, y as usize, w as usize);
    let row_bytes = ws * 4;
    let (mut top, mut bottom) = (usize::MAX, 0usize);
    let (mut left, mut right) = (usize::MAX, 0usize);

    for row in 0..h as usize {
        let dst = ((ys + row) * fbw + xs) * 4;
        let src = row * row_bytes;
        if dst + row_bytes > fb.len() || src + row_bytes > rect.len() {
            continue;
        }
        let old = &fb[dst..dst + row_bytes];
        let new = &rect[src..src + row_bytes];
        if old == new {
            continue;
        }
        // Narrow to the changed span within the row.
        let first = (0..ws).find(|i| old[i * 4..i * 4 + 4] != new[i * 4..i * 4 + 4]);
        let last = (0..ws)
            .rev()
            .find(|i| old[i * 4..i * 4 + 4] != new[i * 4..i * 4 + 4]);
        if let (Some(a), Some(b)) = (first, last) {
            left = left.min(a);
            right = right.max(b);
            top = top.min(row);
            bottom = bottom.max(row);
        }
        fb[dst..dst + row_bytes].copy_from_slice(new);
    }

    if top == usize::MAX {
        return None; // nothing actually changed
    }
    Some(Rect {
        x: (xs + left) as u16,
        y: (ys + top) as u16,
        w: (right - left + 1) as u16,
        h: (bottom - top + 1) as u16,
    })
}

/// CopyRect: move a block already present in the framebuffer.
/// Copies through a temporary so overlapping regions stay correct.
pub fn copy_rect(fb: &mut [u8], fb_w: u16, src: Rect, dst: Rect) {
    let stride = fb_w as usize * 4;
    let mut tmp = vec![0u8; src.w as usize * src.h as usize * 4];
    for row in 0..src.h as usize {
        let s = (src.y as usize + row) * stride + src.x as usize * 4;
        let n = src.w as usize * 4;
        if s + n <= fb.len() {
            tmp[row * n..(row + 1) * n].copy_from_slice(&fb[s..s + n]);
        }
    }
    blit(fb, fb_w, dst.x, dst.y, dst.w, dst.h, &tmp);
}

/// Keep a rectangle inside the framebuffer, so a malformed or stale rect can
/// never index outside the buffer.
fn clamp_rect(r: Rect, w: u16, h: u16) -> Rect {
    let x = r.x.min(w.saturating_sub(1));
    let y = r.y.min(h.saturating_sub(1));
    Rect {
        x,
        y,
        w: r.w.min(w.saturating_sub(x)),
        h: r.h.min(h.saturating_sub(y)),
    }
}

// ------------------------------------------------------------------- client

/// Connect, handshake, and spawn the reader thread. Returns the session once
/// the guest framebuffer size is known.
pub fn connect(id: &str, host: &str, port: u16) -> R<Arc<Session>> {
    disconnect(id);

    // libvirt reports the graphics listen address, which is usually loopback
    // but need not be; vm-man could assume 127.0.0.1 because it launched QEMU
    // itself.
    let addr = format!("{host}:{port}");
    let mut s = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();

    // --- ProtocolVersion
    let ver = read_exact(&mut s, 12)?;
    if !ver.starts_with(b"RFB ") {
        return Err("not an RFB server".into());
    }
    s.write_all(b"RFB 003.008\n")
        .map_err(|e| format!("vnc write: {e}"))?;

    // --- Security: we only support None (QEMU default with no password).
    let n = read_exact(&mut s, 1)?[0];
    if n == 0 {
        let len = be32(&read_exact(&mut s, 4)?) as usize;
        let msg = String::from_utf8_lossy(&read_exact(&mut s, len)?).into_owned();
        return Err(format!("VNC server refused connection: {msg}"));
    }
    let types = read_exact(&mut s, n as usize)?;
    if !types.contains(&1) {
        return Err("VNC server requires authentication (unsupported)".into());
    }
    s.write_all(&[1]).map_err(|e| format!("vnc write: {e}"))?;
    let res = be32(&read_exact(&mut s, 4)?);
    if res != 0 {
        return Err("VNC handshake rejected".into());
    }

    // --- ClientInit (shared = 1) / ServerInit
    s.write_all(&[1]).map_err(|e| format!("vnc write: {e}"))?;
    let init = read_exact(&mut s, 24)?;
    let mut w = be16(&init[0..2]);
    let mut h = be16(&init[2..4]);
    let name_len = be32(&init[20..24]) as usize;
    let _ = read_exact(&mut s, name_len)?;
    if w == 0 || h == 0 {
        w = w.max(1);
        h = h.max(1);
    }

    // --- SetPixelFormat: 32bpp true colour, BGRX byte order.
    let mut spf = [0u8; 20];
    spf[0] = 0; // msg type
    spf[4] = 32; // bits-per-pixel
    spf[5] = 24; // depth
    spf[6] = 0; // big-endian-flag
    spf[7] = 1; // true-colour-flag
    spf[8..10].copy_from_slice(&255u16.to_be_bytes()); // red-max
    spf[10..12].copy_from_slice(&255u16.to_be_bytes()); // green-max
    spf[12..14].copy_from_slice(&255u16.to_be_bytes()); // blue-max
    spf[14] = 16; // red-shift
    spf[15] = 8; // green-shift
    spf[16] = 0; // blue-shift
    s.write_all(&spf).map_err(|e| format!("vnc write: {e}"))?;

    // --- SetEncodings: Raw, CopyRect, DesktopSize
    let encs: [i32; 3] = [0, 1, -223];
    let mut se = vec![2u8, 0];
    se.extend_from_slice(&(encs.len() as u16).to_be_bytes());
    for e in encs {
        se.extend_from_slice(&e.to_be_bytes());
    }
    s.write_all(&se).map_err(|e| format!("vnc write: {e}"))?;

    let tx = s.try_clone().map_err(|e| format!("vnc clone: {e}"))?;
    // Key/pointer events are tiny writes. Without TCP_NODELAY on the write
    // half, Nagle buffers them and input arrives late.
    tx.set_nodelay(true).ok();
    let sess = Arc::new(Session {
        frame: Mutex::new(Frame::sized(w, h)),
        frame_ready: Condvar::new(),
        tx: Mutex::new(Some(tx)),
        running: AtomicBool::new(true),
        error: Mutex::new(None),
    });

    registry()
        .lock()
        .unwrap()
        .insert(id.to_string(), sess.clone());

    // Full update first, then the reader loop keeps asking for incrementals.
    request_update(&sess, 0, 0, w, h, false)?;

    let worker = sess.clone();
    let vm = id.to_string();
    std::thread::spawn(move || {
        if let Err(e) = reader_loop(&worker, s)
            && worker.is_running()
        {
            worker.fail(e);
        }
        worker.running.store(false, Ordering::Relaxed);
        // Drop the registry entry so a later connect starts clean.
        let mut reg = registry().lock().unwrap();
        if let Some(cur) = reg.get(&vm)
            && Arc::ptr_eq(cur, &worker)
        {
            reg.remove(&vm);
        }
    });

    Ok(sess)
}

fn request_update(s: &Session, x: u16, y: u16, w: u16, h: u16, incremental: bool) -> R<()> {
    let mut b = [0u8; 10];
    b[0] = 3;
    b[1] = incremental as u8;
    b[2..4].copy_from_slice(&x.to_be_bytes());
    b[4..6].copy_from_slice(&y.to_be_bytes());
    b[6..8].copy_from_slice(&w.to_be_bytes());
    b[8..10].copy_from_slice(&h.to_be_bytes());
    s.send(&b)
}

fn reader_loop(sess: &Arc<Session>, mut s: TcpStream) -> R<()> {
    // No read timeout in the loop: an idle guest legitimately sends nothing.
    s.set_read_timeout(None).ok();

    while sess.is_running() {
        let msg = match read_exact(&mut s, 1) {
            Ok(v) => v[0],
            Err(e) => {
                if sess.is_running() {
                    return Err(e);
                }
                return Ok(());
            }
        };
        match msg {
            0 => {
                // FramebufferUpdate
                let head = read_exact(&mut s, 3)?;
                let nrects = be16(&head[1..3]);
                let mut resized = None;

                for _ in 0..nrects {
                    let r = read_exact(&mut s, 12)?;
                    let (x, y, w, h) = (
                        be16(&r[0..2]),
                        be16(&r[2..4]),
                        be16(&r[4..6]),
                        be16(&r[6..8]),
                    );
                    let enc = be32(&r[8..12]) as i32;
                    match enc {
                        0 => {
                            let n = w as usize * h as usize * 4;
                            let raw = read_exact(&mut s, n)?;
                            let mut rgba = vec![0u8; n];
                            raw_to_rgba(&raw, OUR_FORMAT, &mut rgba);
                            let mut f = sess.frame.lock().unwrap();
                            let fw = f.w;
                            // QEMU's Raw encoding resends the whole screen even
                            // when a single character changed, so trust the
                            // pixels rather than the advertised rectangle:
                            // compare against what we already have and mark
                            // only the rows that actually differ. Without this
                            // every keystroke would ship a full frame.
                            let changed = blit_changed(&mut f.rgba, fw, x, y, w, h, &rgba);
                            if let Some(c) = changed {
                                f.touch(c);
                            }
                        }
                        1 => {
                            let src = read_exact(&mut s, 4)?;
                            let (sx, sy) = (be16(&src[0..2]), be16(&src[2..4]));
                            let mut f = sess.frame.lock().unwrap();
                            let fw = f.w;
                            // CopyRect carries the same w/h for source and
                            // destination; only the source x/y differs.
                            copy_rect(
                                &mut f.rgba,
                                fw,
                                Rect { x: sx, y: sy, w, h },
                                Rect { x, y, w, h },
                            );
                            f.touch(Rect { x, y, w, h });
                        }
                        -223 => {
                            // DesktopSize: the guest changed resolution. Rebuild
                            // the framebuffer *and* its tile grid, which is sized
                            // to the old resolution and would otherwise drop
                            // damage on the wrong tiles.
                            let mut f = sess.frame.lock().unwrap();
                            let seq = f.seq;
                            *f = Frame::sized(w, h);
                            // Everything changed; force a full frame next.
                            f.seq = seq;
                            f.damage_base = seq;
                            resized = Some((f.w, f.h));
                        }
                        other => {
                            return Err(format!("unsupported VNC encoding {other}"));
                        }
                    }
                }

                // An update that changed no pixels must not advance `seq`, or
                // every parked viewer wakes up to redraw an identical screen.
                let bumped = {
                    let mut f = sess.frame.lock().unwrap();
                    let changed = f.dirty_any || resized.is_some();
                    if changed {
                        f.seq += 1;
                    }
                    changed.then_some((f.seq, f.w, f.h))
                };
                if bumped.is_some() {
                    // Wake any viewer already parked in next_update(). This is
                    // the fast path: pixels leave on a request already waiting.
                    sess.frame_ready.notify_all();
                }

                match resized {
                    // After a resize the whole screen must be re-fetched.
                    Some((w, h)) => request_update(sess, 0, 0, w, h, false)?,
                    None => {
                        let (w, h) = {
                            let f = sess.frame.lock().unwrap();
                            (f.w, f.h)
                        };
                        request_update(sess, 0, 0, w, h, true)?;
                    }
                }
            }
            1 => {
                // SetColourMapEntries — ignored, we forced true colour.
                let head = read_exact(&mut s, 5)?;
                let n = be16(&head[3..5]) as usize;
                let _ = read_exact(&mut s, n * 6)?;
            }
            2 => { /* Bell */ }
            3 => {
                // ServerCutText
                let head = read_exact(&mut s, 7)?;
                let n = be32(&head[3..7]) as usize;
                let _ = read_exact(&mut s, n)?;
            }
            other => return Err(format!("unknown VNC server message {other}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgrx_pixels_become_rgba() {
        // little-endian BGRX: blue=0x10, green=0x20, red=0x30
        let src = [0x10, 0x20, 0x30, 0x00, 0x01, 0x02, 0x03, 0x00];
        let mut out = [0u8; 8];
        raw_to_rgba(&src, OUR_FORMAT, &mut out);
        assert_eq!(out[0..4], [0x30, 0x20, 0x10, 255]);
        assert_eq!(out[4..8], [0x03, 0x02, 0x01, 255]);
    }

    #[test]
    fn blit_lands_at_the_right_offset() {
        // 4x2 framebuffer, write a 2x1 red rect at (1,1)
        let mut fb = vec![0u8; 4 * 2 * 4];
        let rect: Vec<u8> = [255, 0, 0, 255, 255, 0, 0, 255].to_vec();
        blit(&mut fb, 4, 1, 1, 2, 1, &rect);
        let px = |x: usize, y: usize| &fb[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(1, 1), [255, 0, 0, 255]);
        assert_eq!(px(2, 1), [255, 0, 0, 255]);
        assert_eq!(px(0, 1), [0, 0, 0, 0], "must not touch neighbours");
        assert_eq!(px(1, 0), [0, 0, 0, 0], "must not touch other rows");
    }

    #[test]
    fn blit_ignores_out_of_bounds_rects() {
        let mut fb = vec![0u8; 2 * 2 * 4];
        let rect = vec![9u8; 2 * 4];
        // y=5 is outside a 2-row framebuffer: must not panic
        blit(&mut fb, 2, 0, 5, 2, 1, &rect);
        assert!(fb.iter().all(|b| *b == 0));
    }

    #[test]
    fn copyrect_moves_a_block() {
        // 2x2, top-left white, rest black; copy (0,0)1x1 to (1,1)
        let mut fb = vec![0u8; 2 * 2 * 4];
        fb[0..4].copy_from_slice(&[255, 255, 255, 255]);
        copy_rect(
            &mut fb,
            2,
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
            Rect {
                x: 1,
                y: 1,
                w: 1,
                h: 1,
            },
        );
        // Target pixel (1,1) in a 2-wide RGBA framebuffer is at byte 12.
        assert_eq!(&fb[12..16], [255, 255, 255, 255]);
        assert_eq!(&fb[0..4], [255, 255, 255, 255], "source stays put");
    }

    #[test]
    fn damage_is_tiled_not_unioned() {
        let mut f = Frame::sized(128, 128);
        assert!(!f.dirty_any, "a fresh frame has no damage");
        f.touch(Rect {
            x: 10,
            y: 10,
            w: 4,
            h: 4,
        });
        assert!(f.dirty_any);
        let d = f.take_damage();
        assert_eq!(d.len(), 1, "one small change is one rectangle");
        assert_eq!((d[0].x, d[0].y), (0, 0), "the tile containing it");
        f.take_damage();
        assert!(!f.dirty_any, "taking damage clears it");

        // The point of tiling: two changes far apart stay two rectangles. A
        // union (what this used to do) would make them one screen-sized rect,
        // so a keypress with a clock repainting cost a whole frame.
        f.touch(Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        });
        f.touch(Rect {
            x: 120,
            y: 120,
            w: 4,
            h: 4,
        });
        let d = f.take_damage();
        assert_eq!(d.len(), 2, "two distant changes, two rectangles");
        let area: u32 = d.iter().map(|r| r.area()).sum();
        assert!(
            area < 128 * 128,
            "tiled damage ({area}px) must be far smaller than the screen"
        );
        // Empty rectangles are ignored.
        f.touch(Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });
        assert!(!f.dirty_any, "an empty rect dirties nothing");
    }

    #[test]
    fn a_solid_block_becomes_one_rectangle() {
        // Merging rows matters: without it a 128x128 repaint is four row-runs
        // (or many more) and each costs the viewer a separate putImageData.
        let mut f = Frame::sized(128, 128);
        f.touch(Rect {
            x: 0,
            y: 0,
            w: 128,
            h: 128,
        });
        let d = f.take_damage();
        assert_eq!(d.len(), 1, "a solid block is one rect: {d:?}");
        assert_eq!((d[0].w, d[0].h), (128, 128));
    }

    #[test]
    fn a_resize_rebuilds_the_tile_grid() {
        // The grid is sized to the framebuffer. After a guest resolution
        // change an old grid would drop damage on the wrong tiles (or index
        // out of bounds), so `Frame::sized` must be used for the new size.
        let mut f = Frame::sized(64, 64);
        assert_eq!((f.tile_w, f.tile_h), (1, 1));
        f.touch(Rect {
            x: 0,
            y: 0,
            w: 64,
            h: 64,
        });
        assert_eq!(f.take_damage().len(), 1);

        let mut grown = Frame::sized(1920, 1080);
        assert_eq!((grown.tile_w, grown.tile_h), (30, 17));
        grown.touch(Rect {
            x: 1900,
            y: 1070,
            w: 4,
            h: 4,
        });
        let d = grown.take_damage();
        assert_eq!(d.len(), 1, "a change in the far corner dirties its tile");
        // The tile grid overhangs the framebuffer on the last row and column
        // (1080 is not a multiple of 64), so a raw tile can run past the end.
        // What ships is the clamped rect -- anything else would hand the
        // viewer pixels the guest does not have.
        let c = clamp_rect(d[0], 1920, 1080);
        assert!(
            c.x as u32 + c.w as u32 <= 1920 && c.y as u32 + c.h as u32 <= 1080,
            "the delivered rect must stay inside the framebuffer: {c:?}"
        );
    }

    #[test]
    fn identical_pixels_report_no_damage() {
        // QEMU resends the whole screen on Raw; an unchanged repaint must not
        // be reported as damage or every keystroke ships a full frame.
        let mut fb = vec![7u8; 8 * 8 * 4];
        let same = vec![7u8; 8 * 8 * 4];
        assert_eq!(blit_changed(&mut fb, 8, 0, 0, 8, 8, &same), None);
    }

    #[test]
    fn only_the_changed_span_is_reported() {
        let mut fb = vec![0u8; 16 * 16 * 4];
        let mut incoming = vec![0u8; 16 * 16 * 4];
        // Change one pixel at (5, 9).
        let at = (9 * 16 + 5) * 4;
        incoming[at..at + 4].copy_from_slice(&[1, 2, 3, 4]);
        let d = blit_changed(&mut fb, 16, 0, 0, 16, 16, &incoming).expect("change missed");
        assert_eq!(
            (d.x, d.y, d.w, d.h),
            (5, 9, 1, 1),
            "damage must be the single pixel"
        );
        // And the framebuffer really took the new pixel.
        assert_eq!(&fb[at..at + 4], &[1, 2, 3, 4]);
        // A second identical blit is now a no-op.
        assert_eq!(blit_changed(&mut fb, 16, 0, 0, 16, 16, &incoming), None);
    }

    /// A large repaint must arrive as ONE update, and a caught-up viewer must
    /// then go quiet.
    ///
    /// Regression: this was split into bands, which made a full screen visibly
    /// assemble in horizontal strips. It also once looped forever, because the
    /// delivered seq sat below `damage_base` so every request looked "behind"
    /// and was answered with another full frame.
    #[test]
    fn a_full_screen_arrives_in_one_update() {
        let w = 1280u16;
        let h = 800u16;
        let sess = Session {
            frame: Mutex::new({
                let mut f = Frame::sized(w, h);
                f.rgba = vec![9u8; w as usize * h as usize * 4];
                f.seq = 1;
                f // no damage yet => a new viewer gets it all
            }),
            frame_ready: Condvar::new(),
            tx: Mutex::new(None),
            running: AtomicBool::new(true),
            error: Mutex::new(None),
        };

        let u = sess
            .next_update(0, Duration::from_millis(50))
            .expect("a waiting viewer must get pixels");
        assert_eq!(u.rects.len(), 1);
        assert_eq!(
            u.pixels[0].len(),
            u.rects[0].w as usize * u.rects[0].h as usize * 4
        );
        // The whole screen in a single rectangle: no strips.
        assert_eq!(
            (u.rects[0].x, u.rects[0].y, u.rects[0].w, u.rects[0].h),
            (0, 0, w, h),
            "a full repaint must be delivered as one rectangle, not bands"
        );
        assert_eq!(u.seq, 1, "the viewer must be reported fully caught up");

        // Caught up: nothing new, so the next call must block and time out
        // rather than hand out another full screen.
        assert!(
            sess.next_update(u.seq, Duration::from_millis(50)).is_none(),
            "a caught-up viewer must not be sent redundant frames"
        );
    }

    #[test]
    fn damage_rect_stays_inside_the_framebuffer() {
        let r = clamp_rect(
            Rect {
                x: 60,
                y: 60,
                w: 999,
                h: 999,
            },
            64,
            64,
        );
        assert_eq!(r.x + r.w, 64, "width must not run past the buffer");
        assert_eq!(r.y + r.h, 64, "height must not run past the buffer");
    }
}
