// Memory/storage unit conversion used by the size fields. The model stays in
// canonical units (MiB for memory, GiB for storage), so these conversions are
// what a unit-selector change writes into the visible input.
import assert from "node:assert/strict";
import test, { before } from "node:test";

let units;
before(async () => { units = await import("./_build/units.js"); });

test("memory offers MiB and GiB, defaulting to MiB", () => {
  assert.deepEqual([...units.MEMORY_UNITS], ["MiB", "GiB"]);
  assert.equal(units.MEMORY_DEFAULT, "MiB");
});

test("storage offers MiB, GiB and TiB, defaulting to GiB", () => {
  assert.deepEqual([...units.STORAGE_UNITS], ["MiB", "GiB", "TiB"]);
  assert.equal(units.STORAGE_DEFAULT, "GiB");
});

test("converts between units without changing the size", () => {
  assert.equal(units.convert(2048, "MiB", "GiB"), 2);
  assert.equal(units.convert(2, "GiB", "MiB"), 2048);
  assert.equal(units.convert(1, "TiB", "GiB"), 1024);
  assert.equal(units.convert(1024, "GiB", "TiB"), 1);
  assert.equal(units.convert(512, "MiB", "GiB"), 0.5);
});

test("the field value is compact, with no float noise", () => {
  assert.equal(units.unitValue(2048, "MiB", "GiB"), "2");
  assert.equal(units.unitValue(10, "GiB", "GiB"), "10");
  assert.equal(units.unitValue(1536, "MiB", "GiB"), "1.5");
  assert.equal(units.unitValue(1024, "GiB", "TiB"), "1");
  assert.equal(units.unitValue(1, "MiB", "GiB"), "0.00097656");
  // An empty/invalid field must not render "NaN".
  assert.equal(units.unitValue(NaN, "GiB", "MiB"), "0");
});

test("unit options mark the selected unit", () => {
  const html = units.unitOptions(units.STORAGE_UNITS, "TiB");
  assert.match(html, /value="MiB"/);
  assert.match(html, /value="GiB"/);
  assert.match(html, /value="TiB" selected/);
});
