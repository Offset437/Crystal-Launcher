# Third-party notices

The root LICENSE applies to Crystal's GPL-3.0-only code and original SVG interface icons. It does not relicense Square Enix artwork, logos, trademarks, proprietary game files, or dependencies that have their own licenses.

## Rust dependencies and external tools

Cargo.lock files record Rust dependencies and versions. Each dependency retains its own license; release builds collect their required notices into a single THIRD_PARTY_LICENSES.txt. The small files under `licenses/cargo/` supply texts omitted from some upstream crate packages. Keep the generated notices with distributed builds. Projects consulted only as technical references are not bundled dependencies. Windower, Ashita, and dgVoodoo are separate projects with their own distribution terms; their executables are not included in this source tree.

The optional Windows installer is built with [NSIS](https://nsis.sourceforge.io/License) using its zlib compressor.

## Square Enix materials

The `assets/portal-art-*` and `assets/portal-wordmark-*` images contain FINAL FANTASY XI artwork and logos belonging to Square Enix. These assets and news text and images loaded from PlayOnline at runtime are excluded from Crystal's code license. News parser test fixtures contain independently authored fictional material. The copyright and trademark notices appear in README.md and in the launcher. Square Enix's [FFXI Materials Usage License](https://support.na.square-enix.com/rule.php?id=20&la=1&tag=authc) describes its conditions for community uses; it is not a blanket license for software redistribution.

FFXI and PlayOnline executables, DLLs, DAT files, installer archives, music, and account data are not supplied in this repository. Crystal downloads official installation and update files directly from Square Enix to the user's chosen installation. Those files remain subject to Square Enix's terms.
