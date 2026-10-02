// A container text console's grid size, kept apart from `terminal.ts` so it can
// be unit-tested without loading xterm (which is a browser module and will not
// import into bare Node).
//
// The same figures are sent to the daemon, which writes them onto the
// container's pty; xterm and the pty must agree or the shell redraws against
// the wrong geometry.

/// xterm's grid for a stage of `width`x`height` CSS pixels, given one character
/// cell. Minus one column: a whole number of cells rarely fits into the stage,
/// and the remainder would give the terminal a horizontal scrollbar. Floors
/// keep it inside the stage; the minimums keep a stage that has not been laid
/// out yet (measured 0) from producing a 1x1 terminal.
export function fitSize(width: number, height: number, cell: { width: number; height: number }) {
  return {
    cols: Math.max(20, Math.floor(width / cell.width) - 1),
    rows: Math.max(5, Math.floor(height / cell.height)),
  };
}
