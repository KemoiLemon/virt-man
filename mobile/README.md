# VirtMan for iOS

A native SwiftUI client for the virt-man web UI. It talks to `virt-man-web`
over HTTPS, never to a daemon directly, and it is not a webview.

## Install and update with LiveContainer

The workflow publishes an **unsigned** IPA and an **AltStore source** on the
rolling `mobile-latest` release. Install it through LiveContainer (or
SideStore); the app is not signed for direct installation.

Requirements: LiveContainer 3.7 or newer (it added AltStore source support),
and a **public** repository so the source and IPA can be downloaded.

Source URL:

```
https://github.com/KemoiLemon/virt-man/releases/download/mobile-latest/altsource.json
```

Deep link (opens LiveContainer's source manager):

```
livecontainer://sources?url=https%3A%2F%2Fgithub.com%2FKemoiLemon%2Fvirt-man%2Freleases%2Fdownload%2Fmobile-latest%2Faltsource.json
```

Or in LiveContainer: **Sources → + → paste the source URL → Install VirtMan**.
When a new build is published, open Sources and choose **Update**. LiveContainer
does not update silently, and updating keeps the app's data (servers, keys).

Every push to `dev` that touches `mobile/**` republishes the release, so the
source always offers the newest build. You can also download
[VirtMan.ipa](https://github.com/KemoiLemon/virt-man/releases/download/mobile-latest/VirtMan.ipa)
and install it by hand.

## First run

1. **Add Server** — enter `https://<web-host>:8006` (or just `host:8006`).
2. The app shows the server certificate's SHA-256 on first connect. Compare it
   with the web server:

   ```bash
   openssl x509 -in /var/lib/virt-man/tls/cert.pem -noout -fingerprint -sha256
   ```

   If it matches, tap **Trust**. The fingerprint is pinned; if the server
   certificate changes later, the app refuses to connect until you delete and
   re-add the server.
3. Sign in with an account (the app mints a personal API key and stores only
   that) or paste an API key from the web UI's Profile page.

The web UI must be reachable from the phone and must have an account set up
(a fresh server says "finish the first-run setup in a browser"). iOS asks for
local-network permission on the first LAN connection.

## What it does

- Lists daemons (`GET /connections`), VMs and LXC containers.
- Start, shut down, reboot and force-stop both, with live state.
- Guest and host CPU/memory readouts.
- The per-user activity log.
- A native VNC console for VNC guests: touch pointer, long-press right click,
  Esc/Tab/arrow keys, sticky Ctrl/Alt, Ctrl+Alt+Del, and a "type text" box.
  SPICE guests are not rendered; the app says so, as the web UI does.
- Container text consoles, creating/editing guests, storage/network
  management, uploads and charts are not in this version.

The console uses the web server's existing WebSocket splice
(`/proxy/{conn}/domains/{name}/console/stream`) and speaks RFB 3.8 with Raw,
CopyRect and DesktopSize encodings.

## Building

The Xcode project is generated from `project.yml` with
[XcodeGen](https://github.com/yonaskolb/XcodeGen); nothing generated is
committed.

```bash
brew install xcodegen
cd mobile && xcodegen generate
xcodebuild -project VirtMan.xcodeproj -scheme VirtMan \
  -sdk iphonesimulator -destination 'platform=iOS Simulator,name=iPhone 16' test
```

The release path is `.github/workflows/mobile.yml`: it tests, builds an
unsigned arm64 device app with `CODE_SIGNING_ALLOWED=NO`, writes the version
into the built `Info.plist`, zips `Payload/VirtMan.app` into `VirtMan.ipa`,
generates `altsource.json` (`scripts/make-source.py`), and uploads both to the
`mobile-latest` release. `mobile/Artwork/AppIcon.png` is rendered from
`web/public/logo.svg`.
