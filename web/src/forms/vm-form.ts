/// The smallest volume the form will ask for, in GiB: one MiB. Volumes may be
/// smaller than a gigabyte, and the unit selector can say so.
export const MIN_DISK_GIB = 1 / 1024;

/// A disk size in GiB, held to the floor and to an optional ceiling. The floor
/// matters because an empty field must not become zero, and the ceiling is the
/// caller's remaining storage so a create is not refused after the volume is
/// made.
export function clampGib(value: number, hi: number | null): number {
  const n = Number.isFinite(value) ? Math.max(MIN_DISK_GIB, value) : MIN_DISK_GIB;
  return hi == null ? n : Math.min(hi, n);
}
