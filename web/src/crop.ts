// Avatar cropping: a resizable square window over a draggable, zoomable
// image, with a live 64px preview. The crop math lives in pure functions so it
// can be unit-tested without a canvas or real layout.
//
// The image is scaled to cover the stage, so the square always shows the
// picture; dragging inside it pans, and the eight handles on its edges and
// corners resize it (the zoom slider does the same thing coarsely).

import { closeSvg } from "./icons";

export interface CropRect {
  x: number;
  y: number;
  size: number; // natural image px, square
}

export function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
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

/** Draw `rect` (natural px) from `image` into an `out`-square canvas, PNG. */
export function drawCrop(image: HTMLImageElement, rect: CropRect, out = 256): Promise<Blob> {
  const canvas = document.createElement("canvas");
  canvas.width = out;
  canvas.height = out;
  const ctx = canvas.getContext("2d");
  if (!ctx) return Promise.reject(new Error("canvas unavailable"));
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(image, rect.x, rect.y, rect.size, rect.size, 0, 0, out, out);
  return new Promise((resolve, reject) =>
    canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("crop failed"))), "image/png"),
  );
}

function loadImage(file: File): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      resolve(img);
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error("could not read image"));
    };
    img.src = url;
  });
}

/** Open the crop dialog. Resolves with a cropped PNG blob, or null on cancel. */
export async function openCropDialog(file: File): Promise<Blob | null> {
  let image: HTMLImageElement;
  try {
    image = await loadImage(file);
  } catch {
    return null;
  }

  return new Promise((resolve) => {
    let done = false;
    const finish = (v: Blob | null) => {
      if (done) return;
      done = true;
      el.remove();
      document.removeEventListener("pointermove", move, true);
      document.removeEventListener("pointerup", up, true);
      resolve(v);
    };

    const el = document.createElement("div");
    el.className = "modal-back open";

    const stage = document.createElement("div");
    stage.className = "crop-stage";
    const img = document.createElement("img");
    img.draggable = false;
    const win = document.createElement("div");
    win.className = "crop-window";

    const zoom = document.createElement("input");
    zoom.type = "range";
    zoom.min = "1";
    zoom.max = "4";
    zoom.step = "0.05";
    zoom.value = "1";
    zoom.className = "crop-zoom";

    const preview = document.createElement("div");
    preview.className = "avatar";
    preview.style.width = "64px";
    preview.style.height = "64px";
    preview.style.minWidth = "64px";

    const body = document.createElement("div");
    body.className = "modal-body crop-body";
    const previewRow = document.createElement("div");
    previewRow.className = "crop-preview";
    const previewLabel = document.createElement("span");
    previewLabel.className = "hint";
    previewLabel.textContent = "Preview";
    previewRow.append(previewLabel, preview);
    body.append(stage, zoom, previewRow);

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
    modal.className = "modal";
    modal.append(head, body, foot);
    el.append(modal);
    document.body.append(el);

    stage.append(img, win);

    // The image is scaled to *cover* the stage, so the square is always filled
    // with the picture; a contain fit letterboxed a small image and showed the
    // stage behind it. The square starts at 85% of the stage and can be
    // dragged by its edges and corners, which changes how much is cropped.
    const stageW = () => stage.clientWidth;
    const stageH = () => stage.clientHeight;
    const base = () =>
      Math.max(stageW() / image.naturalWidth, stageH() / image.naturalHeight);
    const scale = () => base() * Number(zoom.value);
    const centre = () => {
      const w = image.naturalWidth * scale();
      const h = image.naturalHeight * scale();
      return { ox: (stageW() - w) / 2, oy: (stageH() - h) / 2 };
    };

    const MIN_SIDE = 48;
    let ox = 0;
    let oy = 0;
    let winX = 0;
    let winY = 0;
    let winSide = 0;

    const apply = () => {
      const s = scale();
      img.style.width = `${image.naturalWidth * s}px`;
      img.style.height = `${image.naturalHeight * s}px`;
      img.style.left = `${ox}px`;
      img.style.top = `${oy}px`;
      win.style.width = `${winSide}px`;
      win.style.height = `${winSide}px`;
      win.style.left = `${winX}px`;
      win.style.top = `${winY}px`;
      const rect = cropRect(
        image,
        ox,
        oy,
        s,
        winX + winSide / 2,
        winY + winSide / 2,
        winSide,
      );
      drawCrop(image, rect, 64).then((b) => {
        if (done) return;
        const url = URL.createObjectURL(b);
        preview.style.backgroundImage = `url(${url})`;
        preview.style.backgroundSize = "cover";
        preview.style.backgroundPosition = "center";
      });
    };

    // The image keeps covering the square: panning is clamped so no stage
    // background can show through it.
    const keepVisible = () => {
      const s = scale();
      const w = image.naturalWidth * s;
      const h = image.naturalHeight * s;
      ox = clamp(ox, winX + winSide - w, winX);
      oy = clamp(oy, winY + winSide - h, winY);
    };

    const reset = () => {
      winSide = Math.min(stageW(), stageH()) * 0.85;
      winX = (stageW() - winSide) / 2;
      winY = (stageH() - winSide) / 2;
      const c = centre();
      ox = c.ox;
      oy = c.oy;
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

    // Drag inside the square to pan; drag a handle to resize.
    let drag: { sx: number; sy: number; ox: number; oy: number } | null = null;
    let resizing: Handle | null = null;
    const down = (e: PointerEvent) => {
      drag = { sx: e.clientX, sy: e.clientY, ox, oy };
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
      ox = drag.ox + (e.clientX - drag.sx);
      oy = drag.oy + (e.clientY - drag.sy);
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

    zoom.oninput = () => {
      keepVisible();
      apply();
    };

    // The image may need a tick to lay out before clientWidth is meaningful.
    const first = () => {
      if (stage.clientWidth > 0) reset();
      else requestAnimationFrame(first);
    };
    first();

    cancel.onclick = () => finish(null);
    x.onclick = () => finish(null);
    ok.onclick = () => {
      const rect = cropRect(
        image,
        ox,
        oy,
        scale(),
        winX + winSide / 2,
        winY + winSide / 2,
        winSide,
      );
      drawCrop(image, rect, 256).then(finish, () => finish(null));
    };
    el.addEventListener("modal-escape", () => finish(null));
    el.onclick = (e: MouseEvent) => {
      if (e.target === el) finish(null);
    };
  });
}
