// Avatar cropping: a resizable square window over a draggable image. The crop
// math lives in pure functions so it can be unit-tested without a canvas or
// real layout.
//
// The image is scaled to *fit* the stage and the square shows the part of it
// that is cropped, so the square is the preview -- there is no second copy. The
// square starts covering the whole photo and the eight handles on its edges and
// corners resize it; dragging it moves it around the picture. The avatar is
// exported at the resolution the square covers, held to `OUT_MAX`, so a 1080px
// photo keeps its 1080 pixels rather than being flattened to a fixed square. A
// readout under the stage says what the source is and what the crop will be
// written at, and warns when the source is too small to fill it.

import { closeSvg } from "./icons";

export interface CropRect {
  x: number;
  y: number;
  size: number; // natural image px, square
}

/// The avatar is exported at the resolution the square actually covers, so a
/// 1080px photo keeps its 1080 pixels instead of being flattened to a fixed
/// size. `OUT_MAX` bounds it: past this the file grows without adding detail
/// anyone sees in a 32px avatar, and a lossless PNG of it would blow past the
/// server's 2 MB upload cap (`profile.rs`). 1080 is the bound because that is
/// the size a phone or 1080p photo comes in at, and it is meant to survive
/// uncropped; the export is JPEG, which keeps even a 1080px square a few
/// hundred KB.
export const OUT_MIN = 1;
export const OUT_MAX = 1080;

/// Kept as the name for "the size a crop without a usable source exports at",
/// which the readout and `drawCrop`'s default both want.
export const OUT = OUT_MAX;

/// What a given square actually yields, for the dialog's readout and for the
/// warning that a small source cannot fill the output. Pure, so the arithmetic
/// is tested rather than inferred from a screenshot.
export interface CropQuality {
  /// Natural pixels the square currently covers (the exported crop's source).
  sourcePx: number;
  /// The square the export is written at: the covered pixels, held to
  /// [`OUT_MIN`, `OUT_MAX`].
  outPx: number;
  /// How much the export magnifies the source: 1 means one source px per output
  /// px, above 1 means the output has more pixels than the crop had detail.
  upscale: number;
  /// True when the source is too small to fill the output without upscaling.
  soft: boolean;
}

export function cropQuality(rectSize: number): CropQuality {
  const sourcePx = Math.max(0, rectSize);
  const outPx = Math.round(Math.min(OUT_MAX, Math.max(OUT_MIN, sourcePx)));
  const upscale = sourcePx > 0 ? outPx / sourcePx : 0;
  // A little over 1 is imperceptible; past 1.5x the softness is visible, so
  // that is where the dialog starts warning.
  return { sourcePx, outPx, upscale, soft: sourcePx > 0 && upscale > 1.5 };
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

/// The smallest square the handles may leave, in stage pixels.
export const MIN_SIDE = 48;

/// Where the image sits in the stage and the square it starts with. Pure, so the
/// fit mode and the starting square are tested rather than re-derived by hand:
/// the tests used to recompute the scale themselves, which meant flipping the
/// fit from contain to cover still passed.
///
/// The image *fits* the stage (contain), so the whole photo is visible and the
/// square can cover all of it -- that is what lets a 1080x1080 photo export at
/// 1080. Covering would scale it to fill the stage's height and crop the sides
/// off, leaving the square over a fraction of the photo.
export interface StageLayout {
  /// Image top-left and size in stage pixels.
  ox: number;
  oy: number;
  w: number;
  h: number;
  /// Display scale: stage px per natural px.
  scale: number;
  /// The starting square: the whole image when it fits the stage, else the
  /// largest square the stage allows.
  winX: number;
  winY: number;
  winSide: number;
}

export function stageLayout(
  image: { naturalWidth: number; naturalHeight: number },
  stageW: number,
  stageH: number,
): StageLayout {
  const nw = image.naturalWidth || 1;
  const nh = image.naturalHeight || 1;
  const scale = Math.min(stageW / nw, stageH / nh); // contain
  const w = nw * scale;
  const h = nh * scale;
  const ox = (stageW - w) / 2;
  const oy = (stageH - h) / 2;
  // Start covering the photo's short side, so the export keeps the source's
  // resolution rather than a fraction of it.
  const winSide = Math.max(MIN_SIDE, Math.min(w, h));
  return {
    ox,
    oy,
    w,
    h,
    scale,
    winX: ox + (w - winSide) / 2,
    winY: oy + (h - winSide) / 2,
    winSide,
  };
}

/** Map a screen-space crop window to natural-image coordinates, clamped and
 *  kept square inside the image. `ox`/`oy` are the image's top-left corner in
 *  stage pixels; `scale` is displayWidth / naturalWidth. */
export function cropRect(
  image: { naturalWidth: number; naturalHeight: number },
  ox: number,
  oy: number,
  scale: number,
  cx: number,
  cy: number,
  win: number,
): CropRect {
  const nw = image.naturalWidth;
  const nh = image.naturalHeight;
  let size = win / scale;
  // Never crop beyond the image: shrink to the smaller dimension when the
  // source is smaller than the window at this zoom.
  size = Math.min(size, nw, nh);
  // When shrunk, keep the crop centred in the window; the normal case
  // (size === win/scale) leaves x/y at the window's top-left mapping.
  const centre = (win / scale - size) / 2;
  const x = clamp((cx - ox) / scale + centre, 0, nw - size);
  const y = clamp((cy - oy) / scale + centre, 0, nh - size);
  return { x, y, size };
}

/**
 * Draw `rect` (natural px) from `image` into an `out`-square canvas.
 *
 * JPEG, not PNG: the export is now the crop's real resolution, so a 1080px
 * photo is ~3 MB as a lossless PNG and the server refuses anything over 2 MB
 * (`profile.rs`). At quality 0.92 the same square is well under that for a
 * photo. Uniform noise is the exception -- it is incompressible, and a
 * synthetic 1080px noise image really does reach ~2.1 MB -- so the quality
 * steps down until the blob fits rather than letting the upload be refused
 * after the work.
 */
export function drawCrop(image: HTMLImageElement, rect: CropRect, out = OUT): Promise<Blob> {
  const canvas = document.createElement("canvas");
  canvas.width = out;
  canvas.height = out;
  const ctx = canvas.getContext("2d");
  if (!ctx) return Promise.reject(new Error("canvas unavailable"));
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(image, rect.x, rect.y, rect.size, rect.size, 0, 0, out, out);
  // The same ceiling the server enforces; keep a little margin for the multipart
  // framing and headers that wrap the body.
  const LIMIT = 2 * 1024 * 1024 - 64 * 1024;
  const encode = (q: number) =>
    new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, "image/jpeg", q));
  return (async () => {
    for (const q of [0.92, 0.8, 0.65, 0.5]) {
      const blob = await encode(q);
      if (blob && blob.size <= LIMIT) return blob;
    }
    // Nothing fits at a usable quality: hand back the smallest try and let the
    // server's message name the limit, rather than failing here opaquely.
    const last = await encode(0.5);
    if (!last) throw new Error("crop failed");
    return last;
  })();
}

/// Read the file into a decoded image, returning it with its object URL still
/// live. The caller must revoke that URL when it is done: the image is both the
/// source the crop is drawn from *and* the picture shown on the stage, so
/// revoking here (which an earlier version did) left the stage's own <img>
/// pointing at a dead blob and rendered an empty square.
function loadImage(file: File): Promise<{ image: HTMLImageElement; url: string }> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const image = new Image();
    image.onload = () => resolve({ image, url });
    image.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("could not read image"));
    };
    image.src = url;
  });
}

/** Open the crop dialog. Resolves with a cropped PNG blob, or null on cancel. */
export async function openCropDialog(file: File): Promise<Blob | null> {
  let image: HTMLImageElement;
  let objectUrl: string;
  try {
    ({ image, url: objectUrl } = await loadImage(file));
  } catch {
    return null;
  }

  return new Promise((resolve) => {
    let done = false;
    const finish = (v: Blob | null) => {
      if (done) return;
      done = true;
      el.remove();
      URL.revokeObjectURL(objectUrl);
      document.removeEventListener("pointermove", move, true);
      document.removeEventListener("pointerup", up, true);
      resolve(v);
    };

    const el = document.createElement("div");
    el.className = "modal-back open";

    const stage = document.createElement("div");
    stage.className = "crop-stage";
    // The stage shows the picture itself -- the square is the preview -- so the
    // element needs the source, not just the geometry `apply()` writes.
    const img = document.createElement("img");
    img.src = objectUrl;
    img.draggable = false;
    const win = document.createElement("div");
    win.className = "crop-window";

    // No zoom slider and no separate preview: the square *is* the preview, and
    // the eight handles change how much it crops. A slider duplicated them and
    // a second copy of the image below implied two different crops.
    //
    // The readout accounts for the source's width and height, which nothing did
    // before: cover-fitting a small photo magnified it, so the square covered
    // only a fraction of its pixels and the 256px export scaled that fraction
    // back up -- a visibly soft avatar, with nothing on screen saying so.
    const info = document.createElement("div");
    info.className = "crop-info";

    const body = document.createElement("div");
    body.className = "modal-body crop-body";
    body.append(stage, info);

    const head = document.createElement("div");
    head.className = "modal-head";
    const title = document.createElement("span");
    title.textContent = "Crop avatar";
    const x = document.createElement("button");
    x.className = "x";
    x.setAttribute("aria-label", "Close");
    x.append(closeSvg());
    head.append(title, x);

    const foot = document.createElement("div");
    foot.className = "modal-foot";
    const cancel = document.createElement("button");
    cancel.className = "btn";
    cancel.textContent = "Cancel";
    const ok = document.createElement("button");
    ok.className = "btn primary";
    ok.textContent = "Crop & upload";
    foot.append(cancel, ok);

    const modal = document.createElement("div");
    // Its own class rather than the default 640px: the square is
    // `min(stageW, stageH) * 0.85`, so both dimensions bound it, and a 640px
    // modal against a 260px-tall stage held it at 221px -- smaller than the
    // photo being cropped, which made framing a 1080px image guesswork.
    modal.className = "modal crop-modal";
    modal.append(head, body, foot);
    el.append(modal);
    document.body.append(el);

    stage.append(img, win);

    // The image *fits* the stage (contain), so the whole photo is visible and
    // the square can cover all of it -- which is what lets a 1080x1080 photo
    // export at 1080 instead of a fraction of the frame. Fitting shows the stage
    // background beside a non-square photo, and that is deliberate: the square
    // is the crop, so what is outside it is not part of the avatar.
    const stageW = () => stage.clientWidth;
    const stageH = () => stage.clientHeight;
    const layout = () => stageLayout(image, stageW(), stageH());

    // The image does not move: it fits the stage, centred, and the square is
    // what the pointer drags. `ox`/`oy` are read back on each paint so a resize
    // (the mobile breakpoint changes the stage height) re-centres it.
    let winX = 0;
    let winY = 0;
    let winSide = 0;

    const apply = () => {
      const L = layout();
      img.style.width = `${L.w}px`;
      img.style.height = `${L.h}px`;
      img.style.left = `${L.ox}px`;
      img.style.top = `${L.oy}px`;
      win.style.width = `${winSide}px`;
      win.style.height = `${winSide}px`;
      win.style.left = `${winX}px`;
      win.style.top = `${winY}px`;
      // The crop's real resolution: natural pixels the square covers, and the
      // size the avatar is written at, which is those pixels held to the cap.
      const rect = cropRect(image, L.ox, L.oy, L.scale, winX + winSide / 2, winY + winSide / 2, winSide);
      const q = cropQuality(rect.size);
      const src = `${image.naturalWidth}\u00d7${image.naturalHeight}`;
      const px = `${Math.round(q.sourcePx)}\u00d7${Math.round(q.sourcePx)}`;
      info.textContent = q.soft
        ? `Source ${src} \u2014 the square covers only ${px}, so the ${q.outPx}px avatar will be soft. Pick a larger photo or shrink the square.`
        : `Source ${src} \u2014 avatar will be ${q.outPx}\u00d7${q.outPx}, from ${px} of the photo.`;
      info.classList.toggle("warn", q.soft);
    };

    // The square stays over the picture: the image is the box it may not leave.
    // The image can be smaller than the square only below the `MIN_SIDE` floor,
    // where the square wins and the avatar has a usable size.
    const keepVisible = () => {
      const L = layout();
      const right = L.ox + L.w;
      const bottom = L.oy + L.h;
      winSide = Math.min(winSide, Math.max(MIN_SIDE, Math.min(right - L.ox, bottom - L.oy)));
      winX = clamp(winX, L.ox, Math.max(L.ox, right - winSide));
      winY = clamp(winY, L.oy, Math.max(L.oy, bottom - winSide));
    };

    const reset = () => {
      // The starting square: the whole photo when it fits the stage, so the
      // export keeps the source's own resolution rather than a fraction of it.
      const L = layout();
      winX = L.winX;
      winY = L.winY;
      winSide = L.winSide;
      keepVisible();
      apply();
    };

    // Eight grab handles resize the square. It stays square, anchored at the
    // edge or corner opposite the one dragged, and always inside the stage.
    type Handle = "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w";
    const HANDLES: Handle[] = ["nw", "n", "ne", "e", "se", "s", "sw", "w"];
    for (const h of HANDLES) {
      const grip = document.createElement("div");
      grip.className = `crop-handle ${h}`;
      grip.dataset.handle = h;
      win.append(grip);
    }

    const resize = (h: Handle, px: number, py: number) => {
      const left = winX;
      const top = winY;
      const right = winX + winSide;
      const bottom = winY + winSide;
      const cx = left + winSide / 2;
      const cy = top + winSide / 2;
      const W = stageW();
      const H = stageH();
      let raw: number;
      let maxSide: number;
      let origin: (s: number) => { x: number; y: number };
      switch (h) {
        case "nw":
          raw = Math.max(right - px, bottom - py);
          maxSide = Math.min(right, bottom);
          origin = (s) => ({ x: right - s, y: bottom - s });
          break;
        case "ne":
          raw = Math.max(px - left, bottom - py);
          maxSide = Math.min(W - left, bottom);
          origin = (s) => ({ x: left, y: bottom - s });
          break;
        case "sw":
          raw = Math.max(right - px, py - top);
          maxSide = Math.min(right, H - top);
          origin = (s) => ({ x: right - s, y: top });
          break;
        case "se":
          raw = Math.max(px - left, py - top);
          maxSide = Math.min(W - left, H - top);
          origin = () => ({ x: left, y: top });
          break;
        case "n":
          raw = bottom - py;
          maxSide = Math.min(2 * Math.min(cx, W - cx), bottom);
          origin = (s) => ({ x: cx - s / 2, y: bottom - s });
          break;
        case "s":
          raw = py - top;
          maxSide = Math.min(2 * Math.min(cx, W - cx), H - top);
          origin = (s) => ({ x: cx - s / 2, y: top });
          break;
        case "w":
          raw = right - px;
          maxSide = Math.min(right, 2 * Math.min(cy, H - cy));
          origin = (s) => ({ x: right - s, y: cy - s / 2 });
          break;
        default:
          raw = px - left;
          maxSide = Math.min(W - left, 2 * Math.min(cy, H - cy));
          origin = (s) => ({ x: left, y: cy - s / 2 });
          break;
      }
      winSide = clamp(raw, MIN_SIDE, Math.max(MIN_SIDE, maxSide));
      const o = origin(winSide);
      winX = o.x;
      winY = o.y;
      keepVisible();
      apply();
    };

    // Drag the square around the picture; drag a handle to resize it. It used
    // to drag the *image* under a fixed square, which is the same motion with
    // the opposite sign -- and once the image fits the stage rather than
    // filling it, there is no slack to pan the image into, so the square is the
    // thing that moves.
    let drag: { sx: number; sy: number; wx: number; wy: number } | null = null;
    let resizing: Handle | null = null;
    const down = (e: PointerEvent) => {
      drag = { sx: e.clientX, sy: e.clientY, wx: winX, wy: winY };
      (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
    };
    win.querySelectorAll<HTMLElement>("[data-handle]").forEach((g) => {
      g.addEventListener("pointerdown", (e) => {
        e.preventDefault();
        e.stopPropagation();
        resizing = g.dataset.handle as Handle;
        g.setPointerCapture?.(e.pointerId);
      });
    });
    const move = (e: PointerEvent) => {
      if (resizing) {
        const r = stage.getBoundingClientRect();
        resize(resizing, e.clientX - r.left, e.clientY - r.top);
        return;
      }
      if (!drag) return;
      winX = drag.wx + (e.clientX - drag.sx);
      winY = drag.wy + (e.clientY - drag.sy);
      keepVisible();
      apply();
    };
    const up = () => {
      drag = null;
      resizing = null;
    };
    img.addEventListener("pointerdown", down);
    document.addEventListener("pointermove", move, true);
    document.addEventListener("pointerup", up, true);

    // The image may need a tick to lay out before clientWidth is meaningful.
    const first = () => {
      if (stage.clientWidth > 0) reset();
      else requestAnimationFrame(first);
    };
    first();

    cancel.onclick = () => finish(null);
    x.onclick = () => finish(null);
    ok.onclick = () => {
      const L = layout();
      const rect = cropRect(
        image,
        L.ox,
        L.oy,
        L.scale,
        winX + winSide / 2,
        winY + winSide / 2,
        winSide,
      );
      // Written at the crop's own resolution, held to the cap, so a 1080px photo
      // keeps its detail instead of being flattened to a fixed square.
      drawCrop(image, rect, cropQuality(rect.size).outPx).then(finish, () => finish(null));
    };
    el.addEventListener("modal-escape", () => finish(null));
    el.onclick = (e: MouseEvent) => {
      if (e.target === el) finish(null);
    };
  });
}
