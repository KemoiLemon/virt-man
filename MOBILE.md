# MOBILE.md

How the web UI works on a phone. The target is an **iPhone 12 mini** in
**Orion** (Kagi's WebKit browser), reached over the LAN at
`https://<host>:8006`.

`DESIGN.md` covers the architecture; `AGENTS.md` covers how to work in the
repo. This file covers one viewport, and — more usefully — the reasons the
phone build is shaped the way it is, because several of them are not
discoverable from the code.

## The target

| | Portrait | Landscape |
| --- | --- | --- |
| CSS viewport | 375 × 812 | 812 × 375 |
| Device pixel ratio | 3 | 3 |

Two consequences drive every number below:

- **Orion for iOS is WebKit** (WKWebView), so iOS Safari behaviour is the
  behaviour to design against — including its *bugs*. The one that matters here
  is that **`contextmenu` never fires on iOS WebKit**
  ([WebKit #213953](https://bugs.webkit.org/show_bug.cgi?id=213953)); a long
  press opens the magnifier / native callout instead.
- **Landscape is 812 px wide.** A phone-only breakpoint has to be wider than
  that, or turning the phone sideways silently drops the mobile layout and the
  375 × 375 px content pane that comes with it.

## What was actually broken

Four things, all of them blocking rather than cosmetic:

1. **The content pane was ~35 px wide.** `.layout` was
   `grid-template-columns: minmax(300px, 21vw) 1fr`: below 375 px the 300 px
   minimum wins and the sidebar takes the screen.
2. **No resource action was reachable.** Every VM, network, pool, server and
   root action lives in one `.ctxmenu` built by `vmMenu`/`netMenu`/`poolMenu`/
   `serverMenu`/`rootMenu`, and it was opened **only** from a `contextmenu`
   listener. On iOS that listener never runs, so Start, Stop, Delete, Edit and
   Add Resource were all unreachable — there is no other surface that offers
   them.
3. **The console only spoke mouse.** Pointer input was `mousemove`/`mousedown`/
   `mouseup`, there was no pinch (a page pinch zooms the *page*, and worse, it
   does not zoom back), and there was nothing focusable that is editable, so
   **iOS raised no software keyboard** and the guest could not be typed into.
4. **iOS input hygiene.** Fields were 12–13 px, below the 16 px threshold where
   **iOS zooms the page on focus and never zooms back** — one tap left the
   whole app stuck at 1.3×. Nothing set `autocapitalize`/`autocorrect`, so
   hostnames, tokens and pool names arrived capitalised. And the command
   palette was Ctrl+K only, unreachable without an external keyboard.

## The shape of the fix

### The drawer, and why it closes in `syncUrl`

Below the breakpoint the sidebar becomes an off-canvas drawer
(`position: fixed` + `translateX(-100%)`), shown by a hamburger in the topbar
or a tap on the scrim.

The drawer closes from **`syncUrl()`, and nowhere else**. `syncUrl` is the one
function every navigation passes through — the tree, cards, tabs, the context
menu, the palette, and every renderer — and it starts with

```ts
if (location.pathname === path) return;
```

so a write only happens on a real view change. That early return is what makes
this the right place: the 1 s poll re-renders views constantly and calls
`syncUrl` every time, and any closer placement (or a `document` click handler)
would shut the drawer a second after it opened. One line in one funnel replaces
a `closeDrawer()` in every navigation call site, which is the version that goes
stale the first time someone adds a way to navigate.

### Long press is the context menu

`main.ts` builds the menu in a `menuAt(target, x, y)` function; the
`contextmenu` listener calls it, and `touch.ts`'s `attachLongPress` calls it
too. So a long press opens exactly the same menu, built by the same builders,
as a right-click — one code path, and nothing in `ctxmenu.ts` changed.

Two details are load-bearing:

- **`ctxmenu.ts` closes on any `click` that is not inside the menu.** A long
  press is a `pointerdown`; the finger lifting afterwards emits a `click`, and
  the menu would be dismissed in the frame after it appeared. `attachLongPress`
  therefore swallows exactly one following click with a capture-phase
  `once` listener.
- **The existing guard stays.** `contentMenuItems` already returns `null` for
  `input, textarea, select, .cmd, pre, .select-popover`, so the gesture is
  inert on form fields and in the XML and log panes, where the native long-press
  copy callout has to survive. Plain text keeps the native menu, as designed.

`pointerdown` is also the long press's own cancel signal on chart hovers
(`history.ts`), so the readout is reachable by tap — it was `pointermove` only.

### Popovers clamp to `visualViewport`

`listbox.ts` and `ctxmenu.ts` measure against `window.innerHeight`.
`innerHeight` does not change while the software keyboard is up, so a select
opened near the bottom used to land *behind* the keyboard. Both now use
`visualHeight()` from `touch.ts` (`visualViewport.height ?? innerHeight`).

### The console: absolute touch, deferred press

The viewer gained a touch model, and the rules live in `touch.ts` as pure
functions so `console.ts` and `main.ts` cannot drift apart on thresholds:

| Gesture | Sent to the guest |
| --- | --- |
| Tap | button 1 pressed and released at the touch point |
| Drag | nothing at `pointerdown`; button 1 pressed once movement exceeds the slop, then follows the finger |
| Long press (500 ms, ≤ 8 px) | button 2 (right click) |
| Two-finger drag | scroll when the zoomed canvas overflows, otherwise wheel (buttons 8/16) |
| Two-finger tap | button 2 (right click) |
| Pinch | local zoom, clamped to the same 0.25–4 as the toolbar |

**Why "deferred press" rather than classifying the gesture up front:** nothing
is sent until the intent is known. A press sent at `pointerdown` would put a
button-1 down into the guest before we know whether the gesture is a click, a
drag or a right-click, and every wrong guess is a stray click in someone's
guest — the kind of bug that is invisible in review and infuriating in use.

Mouse input now uses pointer events with `setPointerCapture`, so there is one
code path rather than a mouse path plus a touch path.

### The software keyboard, and why keys are still sent from `keydown`

iOS raises the keyboard only for a focused element it considers editable. When
`(pointer: coarse)` matches, the viewer appends a 1 px transparent
`<textarea class="console-kbd">` **inside** `.console-stage` and focuses it on
`pointerdown`. It must be inside the stage: the Tab guard in `focus.ts` and the
Ctrl+K guard in `main.ts` both check `closest(".console-stage")` to leave the
keyboard to the guest, and that check has to keep matching.

**All key reporting stays on the existing `keydown` listener**; the textarea
exists only to raise the keyboard. `onKeyDown` already calls `preventDefault()`
unconditionally, so the field never accumulates text — no duplicate sends, no
`input`/`beforeinput` handler to keep in sync, and no autocorrect or undo state
building up behind it. It must not be `display: none` or `visibility: hidden`,
or iOS declines to show the keyboard.

That keyboard sits above the bottom of the screen, which on a 375 px-wide
viewport covers most of it — hence `visualHeight()` and the compact `.console-bar`
below the breakpoint.

On-screen keys are for what a touch keyboard cannot produce: `Esc`, `Tab`, the
arrows, and the existing `Ctrl+Alt+Del`. They are built in `console.ts` rather
than in `main.ts`'s console toolbar because tapping a toolbar button moves DOM
focus to that button, which dismisses the software keyboard — so the tap has to
put focus back, and `console.ts` is the only place that knows where the
keyboard's focus belongs. They report through the same `postKey` path a real
keyboard uses, so a tapped key and a typed one cannot behave differently.
Without `Esc`, `Tab` and the arrows a guest shell is unusable, so they are not
optional.

The daemon shell (`terminal.ts`) mounts in the same `.console-stage` and talks
to xterm's own textarea, so the software keyboard covers it without a second
path; the terminal shows the host's own `login:` prompt, so there is no form
above it and its fields take the 16 px `(pointer: coarse)` size from the same
rule as every other field.

### Touch hygiene

`base.css` turns off the tap highlight and sets `touch-action: manipulation`,
which drops the double-tap-zoom delay while keeping pinch-zoom. It deliberately
does **not** set `-webkit-touch-callout: none`: that would also suppress the
native long-press copy callout in the XML and log panes, which is the only way
to copy out of them. Instead the surfaces that own the long press — the sidebar
rows and the content-pane resources and tabs, exactly the set `menuAt` matches —
name themselves in `mobile.css`.
`:focus-visible` handles touch for free (WebKit applies it for keyboard,
matching heuristics for touch), so the focus rings needed no change.

`setHtml()` — which every render path already funnels through — stamps
`autocapitalize="off" autocorrect="off" spellcheck="false"` on the inputs and
textareas in its subtree. One place, after sanitisation, covering login, setup,
Add Server, every modal and every form. `autocomplete` is deliberately left
alone so password managers keep working.

Fields go to 16 px under `(pointer: coarse)` — that is not a size preference,
it is the threshold below which iOS zooms the page on focus and does not zoom
back.

## What was deliberately skipped

- **Swipe-right to open the drawer.** Redundant with the hamburger, and it
  fights the native edge-swipe-back gesture. Edge-swipe-back already works:
  it fires `popstate`, which `applyRoute` + `renderCurrent` handle.
- **PWA / add-to-home-screen.** Standalone mode has no browser back button, so
  it also needs a new in-app Back affordance before it is a net win.
- **Trackpad (relative-pointer) console mode**, and hover support for guests
  that open menus on hover. Absolute tap/drag/long-press covers the common
  cases.
- **A card-per-row layout for the wide tables** (volumes, access log, users).
  They scroll inside their own table instead; revisit a card layout if those
  tables turn out to be unusable at 375 px.
- **A browser-driven test.** The suites are jsdom, which does no layout and
  evaluates no media queries; adding Playwright or Puppeteer to test a media
  query is not worth the dependency. What *is* tested is the pure gesture
  vocabulary and the presence of the CSS contract (see below).

## Verification

Automated, in `pnpm test`:

- `test/touch.test.mjs` — the gesture vocabulary: long-press timing and slop
  boundaries, the two-finger window, `pinchFactor` at distance 0, and
  `clampZoom` at both ends including from `base = 0` (fit).
- `test/console.test.mjs` — the deferred-press state machine as a pure reducer:
  tap sends press+release, drag sends nothing until the slop is passed and then
  button 1 exactly once, long press sends button 2 and **never** button 1.
- `test/mobile.test.mjs` — the CSS/DOM contract that cannot be seen in jsdom:
  `viewport-fit=cover`, `mobile.css` loaded after `density.css`, the drawer
  transform, `100dvh`, `env(safe-area-inset-*)`, `touch-action`, and the
  16 px coarse-pointer field rule. (A regression in that one number is exactly
  what re-breaks focus-zoom, and nothing else would catch it.)

Not automatable here — check by hand on the device:

1. No horizontal scroll; the sidebar is hidden; the hamburger opens it, and the
   scrim and a row tap close it.
2. Long-press a tree row: `.ctxmenu` opens **and stays open**, with the same
   items as a desktop right-click. Long press in the XML pane: native copy
   callout, no app menu.
3. Tap a guest card, then scroll a dashboard and watch several 1 s poll ticks:
   the scroll position must not jump.
4. Console: tap clicks, drag drags a window, two-finger drag scrolls, pinch
   zooms, and tapping the screen raises the software keyboard — typed
   characters appear in the guest.
5. Turn the phone sideways: the drawer still works and the console still fits.
6. With an external keyboard: Tab, Shift+Tab, arrows, Enter and Ctrl+K all
   work, and tapping a text field does not zoom the page (the URL bar's
   zoom indicator stays at 100%).
