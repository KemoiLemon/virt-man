// The container text console's geometry.
//
// xterm is sized to the stage, and that same size is sent to the daemon, which
// writes it onto the container's pty with TIOCSWINSZ. libvirt leaves the pty at
// 0x0, so without this the shell's line editor redraws the bottom line instead
// of scrolling. The arithmetic is small but it is what the pty gets, so it is
// pinned here.
//
// Run: pnpm test

import assert from "node:assert/strict";
import test from "node:test";

const term = await import("./_build/console-geometry.js");

test("the grid is the stage divided into whole cells, one column short", () => {
  // A stage that divides evenly still gives up a column: a whole number of
  // cells rarely fits, and the remainder would scroll the terminal sideways.
  assert.deepEqual(term.fitSize(808, 425, { width: 8, height: 17 }), { cols: 100, rows: 25 });
  // The remainder is floored, never rounded up past the stage.
  assert.deepEqual(term.fitSize(800, 170, { width: 8, height: 17 }), { cols: 99, rows: 10 });
});

test("a collapsed stage keeps a usable minimum", () => {
  // Before layout (or a hidden pane) the stage measures 0; a 1x1 terminal
  // would then be sent to the pty and every line redrawn in place.
  assert.deepEqual(term.fitSize(0, 0, { width: 8, height: 17 }), { cols: 20, rows: 5 });
});

test("a wider stage is more columns, never fewer", () => {
  const a = term.fitSize(400, 300, { width: 8, height: 17 });
  const b = term.fitSize(800, 300, { width: 8, height: 17 });
  assert.ok(b.cols > a.cols, `${b.cols} > ${a.cols}`);
  assert.equal(b.rows, a.rows, "same height, same rows");
});
