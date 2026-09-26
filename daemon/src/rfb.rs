//! Native RFB (VNC) client — no external VNC viewer, no vnc crate.
//!
//! Ported from vm-man, where this same client backs its console. Speaks RFB
//! 3.8 against the hypervisor's VNC server, keeps the guest framebuffer in
//! memory here on the daemon, and hands raw pixels to whichever UI asks.
//!
//! The daemon runs on the virtualisation host, so the socket to the guest's
//! VNC port is local: the transport is tuned for latency rather than
//! bandwidth, and the changed rectangle ships as raw BGRA with no image
//! compression. Compressing cost more time than sending it uncompressed.
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
    /// Union of the rectangles changed since `damage_base`. Lets the viewer
    /// receive only what actually moved instead of a whole screen.
    damage: Option<Rect>,
    /// `seq` the damage was accumulated from; a viewer further behind than
    /// this must take a full frame.
    damage_base: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    fn union(self, o: Rect) -> Rect {
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = (self.x + self.w).max(o.x + o.w);
        let b = (self.y + self.h).max(o.y + o.h);
        Rect {
            x,
            y,
            w: r - x,
            h: b - y,
        }
    }
}

impl Frame {
    /// Record a changed region, merged into the pending damage.
    fn touch(&mut self, r: Rect) {
        if r.w == 0 || r.h == 0 {
            return;
        }
        self.damage = Some(match self.damage {
            Some(d) => d.union(r),
            None => r,
        });
    }
}

/// One update ready for the viewer: raw BGRA pixels for `rect` only.
pub struct Update {
    pub seq: u64,
    pub w: u16,
    pub h: u16,
    pub rect: Rect,
    /// Row-major BGRA, `rect.w * rect.h * 4` bytes.
    pub pixels: Vec<u8>,
}

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

    /// Block until a frame newer than `since` exists, then return only the
    /// changed rectangle as raw pixels.
    ///
    /// This is the whole latency story: the request is already parked inside
    /// the backend when the frame lands, so the pixels go out on the same hop
    /// that was already waiting. No poll interval, no second round trip, and
    /// no image compression.
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
        // be patched incrementally; give it the whole screen.
        //
        // Whatever changed is always sent as ONE rectangle. Splitting a large
        // repaint into bands made it visibly assemble in strips; a whole screen
        // is only a few tens of milliseconds, which reads as instant when it
        // lands in a single paint.
        let full = Rect {
            x: 0,
            y: 0,
            w: f.w,
            h: f.h,
        };
        let rect = match f.damage {
            Some(d) if since >= f.damage_base => d,
            _ => full,
        };
        let rect = clamp_rect(rect, f.w, f.h);

        // Copy just the rectangle out of the framebuffer.
        let (fw, rw, rh) = (f.w as usize, rect.w as usize, rect.h as usize);
        let mut pixels = vec![0u8; rw * rh * 4];
        for row in 0..rh {
            let src = ((rect.y as usize + row) * fw + rect.x as usize) * 4;
            let dst = row * rw * 4;
            let n = rw * 4;
            if src + n <= f.rgba.len() {
                pixels[dst..dst + n].copy_from_slice(&f.rgba[src..src + n]);
            }
        }

        // Everything pending has now been delivered, so the next update only
        // carries what changes from here on. A viewer that falls behind this
        // base is given a full screen instead, so this cannot lose pixels.
        f.damage = None;
        f.damage_base = f.seq;

        Some(Update {
            seq: f.seq,
            w: f.w,
            h: f.h,
            rect,
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
        frame: Mutex::new(Frame {
            rgba: vec![0u8; w as usize * h as usize * 4],
            w,
            h,
            seq: 0,
            damage: None,
            damage_base: 0,
        }),
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
                            // DesktopSize: the guest changed resolution.
                            let mut f = sess.frame.lock().unwrap();
                            f.w = w.max(1);
                            f.h = h.max(1);
                            f.rgba = vec![0u8; f.w as usize * f.h as usize * 4];
                            // Everything changed; force a full frame next.
                            f.damage = None;
                            f.damage_base = f.seq;
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
                    let changed = f.damage.is_some() || resized.is_some();
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
    fn damage_merges_into_one_rect() {
        let mut f = Frame {
            rgba: vec![0u8; 64 * 64 * 4],
            w: 64,
            h: 64,
            seq: 0,
            damage: None,
            damage_base: 0,
        };
        assert!(f.damage.is_none(), "a fresh frame has no damage");
        f.touch(Rect {
            x: 10,
            y: 10,
            w: 4,
            h: 4,
        });
        assert_eq!(
            f.damage,
            Some(Rect {
                x: 10,
                y: 10,
                w: 4,
                h: 4
            })
        );
        // A second region expands the union, never replaces it.
        f.touch(Rect {
            x: 20,
            y: 30,
            w: 4,
            h: 2,
        });
        assert_eq!(
            f.damage,
            Some(Rect {
                x: 10,
                y: 10,
                w: 14,
                h: 22
            })
        );
        // Empty rectangles are ignored.
        f.touch(Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        });
        assert_eq!(
            f.damage,
            Some(Rect {
                x: 10,
                y: 10,
                w: 14,
                h: 22
            })
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
            frame: Mutex::new(Frame {
                rgba: vec![9u8; w as usize * h as usize * 4],
                w,
                h,
                seq: 1,
                damage: None, // no damage yet => a new viewer gets it all
                damage_base: 0,
            }),
            frame_ready: Condvar::new(),
            tx: Mutex::new(None),
            running: AtomicBool::new(true),
            error: Mutex::new(None),
        };

        let u = sess
            .next_update(0, Duration::from_millis(50))
            .expect("a waiting viewer must get pixels");
        assert_eq!(u.pixels.len(), u.rect.w as usize * u.rect.h as usize * 4);
        // The whole screen in a single rectangle: no strips.
        assert_eq!(
            (u.rect.x, u.rect.y, u.rect.w, u.rect.h),
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
