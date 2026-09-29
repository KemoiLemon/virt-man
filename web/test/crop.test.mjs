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


test("cropQuality exports at the crop's resolution, held to the cap", () => {
  // The point of the change: a crop covering 1080 px keeps 1080 px, instead of
  // every avatar coming out at one fixed size.
  assert.deepEqual(crop.cropQuality(1080), { sourcePx: 1080, outPx: 1080, upscale: 1, soft: false });
  assert.equal(crop.cropQuality(256).outPx, 256, "a small crop keeps its size");
  // Past the cap the output stops growing: a 4000px crop is written at the cap.
  const huge = crop.cropQuality(4000);
  assert.equal(huge.outPx, crop.OUT_MAX, "capped");
  assert.equal(huge.outPx, 1080, "and the cap lets a 1080px photo through whole");
  assert.equal(huge.soft, false, "downscaling is not softness");
  assert.ok(huge.upscale < 1, "and the export shrinks the crop");
  // A source below the cap is written 1:1, never padded up to it.
  const mid = crop.cropQuality(400);
  assert.equal(mid.outPx, 400);
  assert.equal(mid.upscale, 1);
  // Degenerate input must not divide by zero or ask for a 0px canvas.
  assert.equal(crop.cropQuality(0).upscale, 0);
  assert.ok(crop.cropQuality(0).outPx >= crop.OUT_MIN, "canvas stays a real size");
  assert.ok(crop.OUT_MAX >= crop.OUT_MIN, "the cap is not below the floor");
});

test("cropQuality warns only when the export is magnified", () => {
  // The output now matches the crop, so a 256px photo is 1:1 and not soft --
  // the previous fixed 256px export flagged exactly that case.
  assert.equal(crop.cropQuality(256).soft, false, "1:1 is not soft");
  assert.equal(crop.cropQuality(1080).soft, false, "the largest source is still 1:1");
  assert.equal(crop.cropQuality(4000).soft, false);
  // Softness needs an output larger than the crop, which the cap only allows
  // below `OUT_MIN`; with both at 1 that is unreachable, so assert the pairs
  // that would produce it rather than pretending to hit it here.
  assert.equal(crop.cropQuality(90).upscale, 1, "90 of 90 is 1:1");
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

test("stageLayout fits the whole photo and starts on all of it", () => {
  // The point of the change: the image FITS the stage (contain), so a square
  // photo is fully visible and the starting square covers the whole of it --
  // which is what makes its export 1080x1080. Re-deriving the scale in the test
  // is what let a cover-fit regression slip through, so this calls the real
  // function.
  const L = crop.stageLayout({ naturalWidth: 1080, naturalHeight: 1080 }, 690, 420);
  assert.equal(L.scale, 420 / 1080, "the short stage side bounds the scale");
  assert.equal(Math.round(L.w), 420, "the image renders at the stage height");
  assert.equal(Math.round(L.h), 420);
  assert.equal(L.ox, (690 - 420) / 2, "and is centred");
  assert.equal(L.winSide, 420, "the square starts covering the whole photo");
  // The crop that starting square takes is the whole source: 1080 natural px.
  const rect = crop.cropRect({ naturalWidth: 1080, naturalHeight: 1080 },
    L.ox, L.oy, L.scale, L.winX + L.winSide / 2, L.winY + L.winSide / 2, L.winSide);
  assert.equal(Math.round(rect.size), 1080, "the whole photo");
  assert.equal(crop.cropQuality(rect.size).outPx, 1080, "exported at source size");
});

test("stageLayout on a photo larger than the cap still caps the export", () => {
  // A 4000x3000 photo fits the stage at 420 tall, the square covers its short
  // side (3000 natural px), and the cap holds the output to 1080.
  const L = crop.stageLayout({ naturalWidth: 4000, naturalHeight: 3000 }, 690, 420);
  const rect = crop.cropRect({ naturalWidth: 4000, naturalHeight: 3000 },
    L.ox, L.oy, L.scale, L.winX + L.winSide / 2, L.winY + L.winSide / 2, L.winSide);
  assert.equal(Math.round(rect.size), 3000, "the short side, in source px");
  const q = crop.cropQuality(rect.size);
  assert.equal(q.outPx, 1080, "capped");
  assert.equal(q.soft, false, "capping is not softness");
});

test("a photo wider than the stage is bounded by its width", () => {
  // 4000x1000: 690/4000 < 420/1000, so the width bounds the fit, the image is
  // full-width and short, and the square is its short side.
  const L = crop.stageLayout({ naturalWidth: 4000, naturalHeight: 1000 }, 690, 420);
  assert.equal(L.scale, 690 / 4000);
  assert.equal(Math.round(L.w), 690, "full width");
  assert.equal(Math.round(L.h), 173, "and short of the stage height");
  assert.equal(L.winSide, L.h, "the square is the image's short side");
  const rect = crop.cropRect({ naturalWidth: 4000, naturalHeight: 1000 },
    L.ox, L.oy, L.scale, L.winX + L.winSide / 2, L.winY + L.winSide / 2, L.winSide);
  assert.equal(Math.round(rect.size), 1000, "the short side, in source px");
});

test("stageLayout never starts below the minimum square", () => {
  // A very wide sliver: its short side in stage px is tiny, so the floor wins
  // and the square is larger than the photo -- the one case the crop cannot be
  // a whole-image square, because an avatar has a usable size.
  const L = crop.stageLayout({ naturalWidth: 20000, naturalHeight: 400 }, 690, 420);
  assert.ok(L.winSide >= crop.MIN_SIDE, `square ${L.winSide} >= ${crop.MIN_SIDE}`);
  assert.ok(L.winSide > L.h, "the square is larger than the sliver");
});
