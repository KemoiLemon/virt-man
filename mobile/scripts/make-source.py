#!/usr/bin/env python3
"""Generate the AltStore source JSON for a built VirtMan IPA.

LiveContainer >= 3.7 reads AltStore sources; the source is published as a
release asset so `downloadURL` can point at the rolling IPA asset.
"""

import argparse
import datetime
import json
import os
import sys

REPO = "https://github.com/KemoiLemon/virt-man"
RAW = "https://raw.githubusercontent.com/KemoiLemon/virt-man/dev/mobile/Artwork/AppIcon.png"
IPA_URL = f"{REPO}/releases/download/mobile-latest/VirtMan.ipa"


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--version", required=True)
    p.add_argument("--build", required=True)
    p.add_argument("--ipa", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--notes", default="")
    args = p.parse_args()

    size = os.path.getsize(args.ipa)
    if size <= 0:
        print("empty IPA", file=sys.stderr)
        return 1
    date = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    source = {
        "name": "Virt-Man",
        "identifier": "top.kemoi.virtman.source",
        "nsfw": False,
        "subtitle": "Native iOS client for virt-man",
        "description": "Lists daemons, VMs and containers, with power actions, stats, activity and a native VNC console.",
        "iconURL": RAW,
        "headerURL": RAW,
        "website": REPO,
        "tintColor": "#fe6565",
        "featuredApps": ["top.kemoi.virtman"],
        "apps": [
            {
                "name": "VirtMan",
                "bundleIdentifier": "top.kemoi.virtman",
                "developerName": "KemoiLemon",
                "subtitle": "virt-man on iPhone",
                "localizedDescription": "Native SwiftUI client for virt-man over HTTPS.",
                "iconURL": RAW,
                "tintColor": "#fe6565",
                "category": "utilities",
                "appPermissions": {"entitlements": [], "privacy": {}},
                "versions": [
                    {
                        "version": args.version,
                        "buildVersion": args.build,
                        "date": date,
                        "localizedDescription": args.notes or f"Rolling build {args.build}",
                        "downloadURL": f"{IPA_URL}?v={args.build}",
                        "size": size,
                        "minOSVersion": "16.0",
                    }
                ],
            }
        ],
        "news": [],
    }

    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(source, f, indent=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
