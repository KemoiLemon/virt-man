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
