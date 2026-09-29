// Crop math: screen window -> natural-image square, clamped inside the image.

import assert from "node:assert/strict";
import test, { before } from "node:test";

let crop;

before(async () => {
  crop = await import("./_build/crop.js");
});

test("clamp bounds a value", () => {
  assert.equal(crop.clamp(5, 0, 10), 5);
  assert.equal(crop.clamp(-3, 0, 10), 0);
  assert.equal(crop.clamp(42, 0, 10), 10);
});

test("centred window maps to the centre square of a fitted image", () => {
  // Stage 600x300 -> window 300x300 centred at (150, 0). A 1200x600 image
  // fitted at scale .5 fills the stage exactly (offset 0,0).
  const r = crop.cropRect({ naturalWidth: 1200, naturalHeight: 600 }, 0, 0, 0.5, 150, 0, 300);
  assert.deepEqual(r, { x: 300, y: 0, size: 600 });
});

test("zooming scales the crop to natural pixels", () => {
  // Same stage/window, but scale 1 (image shown at natural size, offset so the
  // window is centred on the image: ox=(600-1200)/2=-300, oy=(300-600)/2=-150).
  const r = crop.cropRect({ naturalWidth: 1200, naturalHeight: 600 }, -300, -150, 1, 150, 0, 300);
  assert.deepEqual(r, { x: 450, y: 150, size: 300 });
});

test("an image smaller than the window crops its full extent", () => {
  // 100x100 image centred in the stage: ox=250, oy=100.
  const r = crop.cropRect({ naturalWidth: 100, naturalHeight: 100 }, 250, 100, 1, 150, 0, 300);
  assert.deepEqual(r, { x: 0, y: 0, size: 100 });
});

test("clamps when the window would fall outside the image", () => {
  const r = crop.cropRect({ naturalWidth: 1000, naturalHeight: 1000 }, 300, 0, 1, 150, 150, 300);
  assert.deepEqual(r, { x: 0, y: 150, size: 300 });
});

// ---------------------------------------------------------------------------
// The dialog itself. jsdom does no layout (clientWidth is 0) and cannot decode
// an image, so the geometry cannot be asserted here -- but the failure that
// mattered was not geometry.
//
// The stage's own <img> was created empty and never given a source: an earlier
// version loaded the file into a *different* Image, revoked its object URL on
// load, and used that only to read naturalWidth and to draw the preview. The
// square then rendered nothing but the shaded stage background, which read as
// "the avatar does not show in the crop square". Nothing in the pure math could
// see that, so this pins the wiring instead.


// ---------------------------------------------------------------------------
// The dialog's wiring, not its geometry.
//
// jsdom does no layout (clientWidth is 0) and never decodes an image, so the
// crop math above is asserted directly and this test covers the failure that
// actually reached the user: the stage's own <img> was created empty and never
// given a source. The file was loaded into a *different* Image, whose object URL
// was revoked on load and used only to read naturalWidth and to draw the old
// 64px preview -- so the square rendered nothing but the shaded stage
// background, which read as "the avatar does not show in the crop square".
// No amount of pure-math testing could see that, hence a DOM test.


test("cropQuality reports the source resolution and when it is soft", () => {
  // 1:1 and larger: nothing is invented on the way out.
  assert.deepEqual(crop.cropQuality(256), { sourcePx: 256, upscale: 1, soft: false });
  assert.deepEqual(crop.cropQuality(1000).soft, false, "a large source is not soft");
  // A square covering 100 source px, exported at 256, is magnified 2.56x.
  const small = crop.cropQuality(100);
  assert.equal(small.upscale, 2.56);
  assert.equal(small.soft, true, "2.56x upscaling is visible");
  // The threshold: just over 1.5x warns, just under does not.
  assert.equal(crop.cropQuality(171).soft, false, "1.497x is imperceptible");
  assert.equal(crop.cropQuality(170).soft, true, "1.506x is not");
  // Degenerate input must not divide by zero.
  assert.equal(crop.cropQuality(0).soft, false);
  assert.equal(crop.cropQuality(0).upscale, 0);
});

test("the readout accounts for the source width and height", () => {
  // The bug: cover-fitting a 256x256 photo magnified it, so the square covered
  // only ~92 of its pixels and the export scaled those back up to 256, with
  // nothing on screen saying so. These are the numbers from that case.
  const stage = { W: 612, H: 260 };
  const rect = crop.cropRect({ naturalWidth: 256, naturalHeight: 256 }, 0, 0, 1, 150, 0, 300);
  assert.ok(rect.size > 0, "a real crop");
  const q = crop.cropQuality(92.44);
  assert.equal(q.soft, true, "a 256px source cannot fill a 256px avatar");
  assert.ok(q.upscale > 2.7 && q.upscale < 2.8, `upscale ${q.upscale}`);
  // A photo with enough pixels for the export is not flagged.
  const big = crop.cropQuality(1444.44);
  assert.equal(big.soft, false);
  assert.ok(big.upscale < 0.2, "a 4000px source is downsampled, not upscaled");
  void stage;
});

test("the crop square is given the picture, not an empty <img>", async () => {
  const { JSDOM } = await import("jsdom");
  const dom = new JSDOM("<!doctype html><body></body>", {
    url: "http://localhost/", pretendToBeVisual: true,
  });
  const w = dom.window;
  const doc = w.document;
  // The dialog uses the BARE globals `Image`, `URL` and `document`, not
  // `window.Image`; in a browser they are the same object and here they are not
  // (`globalThis.Image` is undefined in Node). Wire them for the duration.
  const RealImage = w.Image;
  const PatchedImage = function PatchedImage() {
    const el = new RealImage();
    Object.defineProperty(el, "naturalWidth", { get: () => 400 });
    Object.defineProperty(el, "naturalHeight", { get: () => 300 });
    let src = "";
    Object.defineProperty(el, "src", {
      get: () => src,
      set: (v) => { src = v; queueMicrotask(() => el.onload?.()); },
    });
    return el;
  };
  const prev = {
    window: globalThis.window, document: globalThis.document,
    Image: globalThis.Image, URL: globalThis.URL, raf: globalThis.requestAnimationFrame,
  };
  globalThis.window = w;
  globalThis.document = doc;
  globalThis.Image = PatchedImage;
  // The dialog waits for a layout tick before reading clientWidth; jsdom's rAF
  // exists on the window but the code calls the bare global. jsdom has no
  // layout, so clientWidth stays 0 and `first()` would retry forever -- run the
  // callback a bounded number of times and then stop, or the test leaks a
  // spinning timer that throws after the test has already passed.
  let ticks = 0;
  globalThis.requestAnimationFrame = (fn) => {
    if (ticks++ > 5) return 0;
    return w.setTimeout(() => fn(0), 0);
  };
  // createObjectURL must return a string the patched image will accept, and
  // revoking is a no-op here (nothing was really allocated).
  globalThis.URL = { createObjectURL: () => "blob:test", revokeObjectURL: () => {} };
  try {
    const p = crop.openCropDialog({ name: "a.png", type: "image/png" });
    await new Promise((r) => setTimeout(r, 30));
    const stage = doc.querySelector(".crop-stage");
    assert.ok(stage, "the dialog rendered a stage");
    const img = stage.querySelector("img");
    assert.ok(img, "the stage has an image element");
    // The regression: the stage <img> was created empty and never given a
    // source, so the square painted only the shaded stage background.
    assert.equal(img.getAttribute("src"), "blob:test",
      "the stage image must carry the picture's source");
    // Removed, and must not return as a second copy of the square's job.
    assert.equal(doc.querySelector(".crop-zoom"), null, "no zoom slider");
    assert.equal(doc.querySelector(".crop-preview"), null, "no separate preview");
    assert.ok(doc.querySelector(".crop-handle"), "the square still resizes by hand");
    // The readout element, which accounts for the source's width and height.
    // Its *text* is filled by apply(), and apply() only runs once the stage has
    // a width -- jsdom does no layout, so clientWidth is 0 and the dialog never
    // reaches its first paint here. The formatting itself is covered by the
    // cropQuality tests above; this pins that the readout is in the dialog and
    // sits directly under the stage, which is where a person reads it.
    const info = doc.querySelector(".crop-info");
    assert.ok(info, "the dialog shows a resolution readout");
    assert.equal(info.previousElementSibling, stage, "and it follows the stage");
    doc.querySelector(".modal-foot .btn").dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
    assert.equal(await p, null, "cancel resolves null");
  } finally {
    globalThis.window = prev.window;
    globalThis.document = prev.document;
    globalThis.Image = prev.Image;
    globalThis.URL = prev.URL;
    globalThis.requestAnimationFrame = prev.raf;
  }
});
