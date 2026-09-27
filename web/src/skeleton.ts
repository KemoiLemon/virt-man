// Placeholder shapes painted while a page's first read is in flight.
//
// They reuse the real layout classes (.cards, .sumgrid, .card-grid,
// .storage-card) so the skeleton occupies the same space as the content that
// replaces it, and the page does not jump when the answer arrives. The bars
// themselves carry no text: they are decoration, hidden from screen readers,
// with one live "Loading" label for the region.

export type SkeletonShape = "dashboard" | "cards" | "table" | "page";

const bar = (w: string, h = 10) => `<span class="sk" style="width:${w};height:${h}px"></span>`;

const statCard = () =>
  `<div class="card"><span class="sk sk-ic"></span><div class="sk-col">${bar("34px", 14)}${bar("58px", 8)}</div></div>`;

const metric = () =>
  `<div class="metric"><div class="metric-head"><span class="sk sk-ic"></span>${bar("30%", 11)}</div>` +
  `${bar("100%", 7)}${bar("100%", 70)}${bar("60%", 8)}</div>`;

const resourceCard = () =>
  `<div class="storage-card"><div class="storage-card-head"><span class="sk sk-ic"></span>${bar("55%", 12)}</div>` +
  `${bar("80%", 9)}${bar("65%", 9)}${bar("100%", 26)}</div>`;

const row = () => `<div class="sk-row">${bar("22%")}${bar("30%")}${bar("14%")}</div>`;

export function skeleton(shape: SkeletonShape): string {
  let body: string;
  switch (shape) {
    case "dashboard":
      body =
        `<div class="cards">${statCard().repeat(4)}</div>` +
        `<div class="sumgrid">${metric().repeat(2)}</div>` +
        `<div class="sk-rows">${row().repeat(3)}</div>`;
      break;
    case "cards":
      // The caller's container is already the grid (or list) the real cards
      // go into; the wrapper is display:contents, so these become its items.
      body = resourceCard().repeat(3);
      break;
    case "page":
      body =
        `<div class="dhead">${bar("20px", 20)}${bar("180px", 16)}</div>` +
        `<div class="sumgrid">${metric().repeat(2)}</div>` +
        `<div class="sk-rows">${row().repeat(4)}</div>`;
      break;
    case "table":
      body = `<div class="sk-rows">${row().repeat(5)}</div>`;
      break;
  }
  return `<div class="skeleton" role="status" aria-label="Loading" aria-busy="true">${body}</div>`;
}
