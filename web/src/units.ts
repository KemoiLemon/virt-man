// Units for the memory and storage size fields.
//
// A size is stored in one canonical unit -- MiB for guest memory, GiB for
// disks, volumes and quotas -- so the unit selector is presentation only. It
// changes how the number is written, never what it means; a field left alone
// therefore round-trips exactly, and only an edit is converted back.

export const MEMORY_UNITS = ["MiB", "GiB"] as const;
export const STORAGE_UNITS = ["MiB", "GiB", "TiB"] as const;
export type MemoryUnit = (typeof MEMORY_UNITS)[number];
export type StorageUnit = (typeof STORAGE_UNITS)[number];
export type Unit = MemoryUnit | StorageUnit;

/// Memory reads naturally in MiB; storage in GiB, the unit the daemon and the
/// quota API already speak.
export const MEMORY_DEFAULT: MemoryUnit = "MiB";
export const STORAGE_DEFAULT: StorageUnit = "GiB";

/// Each unit in MiB, the common denominator.
const MIB: Record<Unit, number> = { MiB: 1, GiB: 1024, TiB: 1024 * 1024 };

export function convert(value: number, from: Unit, to: Unit): number {
  if (!Number.isFinite(value)) return 0;
  return (value * MIB[from]) / MIB[to];
}

/// A converted value for an `<input type="number">`.
///
/// Capped at eight decimals so 1024 MiB shows as `1` GiB rather than
/// `0.9999999999999999`, while still keeping sub-GiB values meaningful. The
/// canonical value is what is stored, so trimming here never loses precision
/// unless the user edits the converted number.
export function unitValue(value: number, from: Unit, to: Unit): string {
  const n = convert(value, from, to);
  if (!Number.isFinite(n)) return "";
  return String(Number(n.toFixed(8)));
}

/// Options for a unit select. The strings are fixed, so no escaping is needed.
export function unitOptions(units: readonly Unit[], selected: Unit): string {
  return units
    .map((u) => `<option value="${u}"${u === selected ? " selected" : ""}>${u}</option>`)
    .join("");
}
