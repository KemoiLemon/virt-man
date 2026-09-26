// Avatar cropping: a fixed square window over a draggable, zoomable image,
// with a live 64px preview. The crop math lives in pure functions so it can
// be unit-tested without a canvas or real layout.
//
// ponytail: the crop window is fixed-size on screen and the image moves under
// it (no free-form box dragging). Enough for avatars; add a resizable box only
// if non-square crops are ever wanted.

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

    // Fit the image inside the stage (never upscale), then allow 1-4x zoom.
    const stageW = () => stage.clientWidth;
    const stageH = () => stage.clientHeight;
    const base = () =>
      Math.min(stageW() / image.naturalWidth, stageH() / image.naturalHeight, 1);
    const scale = () => base() * Number(zoom.value);
    const winSize = () => Math.min(stageW(), stageH());
    const centre = () => {
      const w = image.naturalWidth * scale();
      const h = image.naturalHeight * scale();
      return { ox: (stageW() - w) / 2, oy: (stageH() - h) / 2 };
    };

    let ox = 0;
    let oy = 0;

    const apply = () => {
      const s = scale();
      img.style.width = `${image.naturalWidth * s}px`;
      img.style.height = `${image.naturalHeight * s}px`;
      img.style.left = `${ox}px`;
      img.style.top = `${oy}px`;
      const size = winSize();
      win.style.width = `${size}px`;
      win.style.height = `${size}px`;
      win.style.left = `${(stageW() - size) / 2}px`;
      win.style.top = `${(stageH() - size) / 2}px`;
      const rect = cropRect(
        image,
        ox,
        oy,
        s,
        (stageW() - size) / 2,
        (stageH() - size) / 2,
        size,
      );
      drawCrop(image, rect, 64).then((b) => {
        if (done) return;
        const url = URL.createObjectURL(b);
        preview.style.backgroundImage = `url(${url})`;
        preview.style.backgroundSize = "cover";
        preview.style.backgroundPosition = "center";
      });
    };

    const keepVisible = () => {
      const s = scale();
      const w = image.naturalWidth * s;
      const h = image.naturalHeight * s;
      const size = winSize();
      const c = centre();
      // Keep the image covering the window whenever it is large enough; a
      // source smaller than the window stays centred on the short axis.
      ox = w >= size ? clamp(ox, (stageW() - size) / 2 + size - w, (stageW() - size) / 2) : c.ox;
      oy = h >= size ? clamp(oy, (stageH() - size) / 2 + size - h, (stageH() - size) / 2) : c.oy;
    };

    const reset = () => {
      const c = centre();
      ox = c.ox;
      oy = c.oy;
      keepVisible();
      apply();
    };

    // Drag to pan.
    let drag: { sx: number; sy: number; ox: number; oy: number } | null = null;
    const down = (e: PointerEvent) => {
      drag = { sx: e.clientX, sy: e.clientY, ox, oy };
      (e.target as HTMLElement).setPointerCapture?.(e.pointerId);
    };
    const move = (e: PointerEvent) => {
      if (!drag) return;
      ox = drag.ox + (e.clientX - drag.sx);
      oy = drag.oy + (e.clientY - drag.sy);
      keepVisible();
      apply();
    };
    const up = () => {
      drag = null;
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
        (stageW() - winSize()) / 2,
        (stageH() - winSize()) / 2,
        winSize(),
      );
      drawCrop(image, rect, 256).then(finish, () => finish(null));
    };
    el.addEventListener("modal-escape", () => finish(null));
    el.onclick = (e: MouseEvent) => {
      if (e.target === el) finish(null);
    };
  });
}
