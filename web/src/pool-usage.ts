// A pool's filesystem usage, kept in one place so every surface agrees.
//
// `allocation` is the blocks held by the regular files directly in the pool
// root; `capacity` and `available` come from statvfs for the filesystem that
// holds it. A directory pool shares that filesystem with the OS and everything
// else, so showing the pool's own file blocks beside the filesystem total and
// free made a 930 GiB disk with 222 GiB used read as 0.0%. `capacity -
// available` is the figure in the same scope as the two it sits beside.
// Definitions without filesystem figures keep the old block-count answer.
export function poolUsed(p: {
  capacity?: number | null;
  available?: number | null;
  allocation?: number | null;
}): number {
  if (p.capacity != null && p.available != null) {
    return Math.max(0, p.capacity - p.available);
  }
  return p.allocation ?? 0;
}
