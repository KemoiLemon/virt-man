// Whether a touch gesture or its inertia is still moving something. A poll
// rebuilds whole tables with setHtml, and replacing the node the browser is
// scrolling cancels the gesture: on a phone the list stopped dead under the
// finger, every time a refresh landed mid-flick. Cheap enough to ask every
// tick, and a skipped tick is just one second's worth of staleness.
// Separate from polling.ts so a view can ask without importing the pollers.
let fingerDown = false;
let lastScrollAt = 0;
document.addEventListener("pointerdown", () => { fingerDown = true; }, true);
document.addEventListener("pointerup", () => { fingerDown = false; }, true);
document.addEventListener("pointercancel", () => { fingerDown = false; }, true);
// A lost pointer (tab hidden, window blurred mid-drag) never reports up.
window.addEventListener("blur", () => { fingerDown = false; });
document.addEventListener("scroll", () => { lastScrollAt = Date.now(); }, true);
export function busyScrolling(): boolean {
  return fingerDown || Date.now() - lastScrollAt < 250;
}
