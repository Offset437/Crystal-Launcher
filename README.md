# Crystal Launcher

A Windows launcher for FINAL FANTASY XI, built in Rust with a native iced interface. Still being tested; releases are currently private.

Crystal manages accounts, Vanilla and Windower/Ashita launches, PlayOnline friends and messages, official news, game settings, controllers, and installation checks and updates. The updater handles FFXI and PlayOnline together, with resumable downloads and backups of replaced files.

Retail launches use the installed PlayOnline core for authentication and session services. Crystal replaces the Viewer interface, but still requires PlayOnline and FFXI to be installed. Complete equivalence with the official Viewer has not been established. The US Windows client is the current tested configuration; launcher translations do not change the game's region. The former experimental native client is disabled.

Crystal is an independent project, unaffiliated with and not endorsed by Square Enix, Windower, or Ashita.

## Getting started

Install PlayOnline and FFXI using the official installer, or use an existing installation. Crystal detects FFXI, PlayOnline, Windower and Ashita from local registrations, shortcuts and common install folders. Valid saved paths take priority; use Find game or Detect to search again, or Browse to choose another copy. The Downloads page can download the official installer and check or update the installed files. Add your retail accounts under Accounts and choose Vanilla to launch directly, or choose Windower or Ashita.

Accounts → Import accounts reads a local PlayOnline `login_w.bin` or Forest `creds.dat` into a review screen. PlayOnline import recovers its saved POL password on the original PC; keep `noramim.bin` beside `login_w.bin`. Square Enix passwords are not saved in that member file. Forest can supply the Square Enix password saved by this Windows user; match its member slots with the PlayOnline file to combine both passwords. Complete any remaining fields in the account editor. Imports keep populated saved fields and leave their source files unchanged; authenticator secrets are not imported.

Drag an account's handle to change its position; Crystal saves the order. Account details and launch preferences can be edited while playing and apply to the next fresh launch. The running session keeps its login identity. “Edit profiles in Windower” opens Windower's own profile editor.

Settings → Graphics offers optional ReShade for Vanilla, Windower and Ashita. Enable dgVoodoo, close games and sign out PlayOnline sessions, then choose Set up ReShade. Crystal downloads the standard official setup tool and selects its 32-bit game host and DirectX API. Choose the effects you want, finish setup, then turn ReShade on. Choose preset imports a copy of a preset `.ini`; its required effects can be installed through setup. Home opens ReShade's in-game menu. Presets, shaders and configuration are kept in Crystal's data folder through launcher updates and reinstalls; ReShade binaries and shaders are not bundled with Crystal.

Each account has an optional automatic character login setting, disabled by default. Slot 1 selects the first character; choose another row for a different character. Navigation runs only while that game window is active. Unknown game layouts, unavailable characters and unexpected menus leave login to you.

The Home page and account editor let you choose each account's launch method. Settings → System sets the default for accounts using “Use default.” Vanilla starts Crystal's game host directly, uses the game's settings, and does not start Windower or Ashita or load their addons. Sign out of an existing PlayOnline session before switching its launch method; an already running host retains its original tool.

Packaged builds keep settings and the Windows-user-encrypted credential vault under `%LOCALAPPDATA%\Crystal Launcher`. Updates and reinstalling preserve saved logins and settings for the same Windows user; they replace only the launcher package. One-time passwords are not saved. A development checkout may retain its existing local data location. Do not share credential vaults, login files, or unredacted account logs.

Use the setup executable for Desktop and Start menu shortcuts and an entry in Windows Installed apps, or extract the portable ZIP wherever you prefer. Setup installs for your Windows account; uninstalling removes the launcher and its notices while keeping accounts, settings, and game files.

Menu resolution applied in Crystal is retained for Vanilla and Ashita launches. Choose Automatic to use the game's menu size for Vanilla or the launch profile's menu size for Ashita. Windower uses its selected profile's resolution and UI scale so the game and its plugins share the same dimensions. FFXIAH searches show prices for the selected world inside Crystal; website links use the world saved in your browser.

Update and repair require closed game clients and signed-out PlayOnline sessions. Update preserves modified FFXI files; Repair can replace them and keeps backups. Texture packs are not included.

Controller navigation is enabled by default. The D-pad or left stick moves focus, the bottom face button selects, the right face button goes back, shoulders change pages, and Start activates the main Play/Update button. Select a dropdown or slider to adjust its value, then confirm again. Navigation pauses outside Crystal and during game-button binding. Text entry and Windows file dialogs use the keyboard and mouse. Disable navigation under Settings → System.

Crystal checks for signed launcher releases in the background. An available release offers **Update and restart**; running games continue. Settings → System provides a manual check and the automatic-check preference. The update helper replaces this launcher and its notices after the window exits, retaining the previous package. Launcher updates become available after a signed release is published.

## Building

Use PowerShell 7 on Windows, Python 3, Rust with the MSVC toolchain, and Visual Studio Build Tools with C++ and Windows SDK support. Both x64 and x86 tools are needed.

```powershell
rustup target add i686-pc-windows-msvc
./tools/build-release.ps1
```

The script builds the 32-bit game host first, embeds it in the launcher, and copies the executable and notices to `target`. It also collects dependency license texts into THIRD_PARTY_LICENSES.txt. It respects `CARGO_TARGET_DIR`. Use `-OutputPath` to choose the executable location, `-LoaderTargetDir` to reuse a separate host build cache, and `-Offline` when dependencies are already cached. Keep the supplied notices with distributed builds and provide their corresponding source under the GPL.

To build the optional installer, pass `-InstallerCompiler` with the path to [NSIS](https://nsis.sourceforge.io/Download)'s `makensis.exe`. The output executable must be named `Crystal-Launcher.exe`; setup is written alongside it. The installer uses the same packaged launcher and notices as the portable release.

For launcher releases, increment the package version, build the package, then run `tools/sign-launcher-release.ps1` with its `-PackageFolder`, the external `-PrivateKeyPath`, matching `-Version`, and a fresh `-OutputFolder`. Publish both generated files, the signed ZIP and `launcher-update.json`, under the matching `v<version>` GitHub release. Signing creates local artifacts; it does not publish anything.

The update trust key is `assets/update-public-key.hex`. The corresponding private key is kept outside the repository and release packages. To establish a new key before distributing a launcher, its headless `--create-launcher-update-key` command takes an absolute private-key path, writes a new PKCS#8 key and prints only the public key; put that public key in the asset and rebuild. Keep the private key backed up securely. Changing the embedded public key disconnects already distributed launchers from later signatures.

## Tests

```powershell
$env:CRYSTAL_DATA_ROOT = Join-Path $env:TEMP 'crystal-launcher-tests'
cargo test --locked
cargo test --locked --manifest-path crystal-loader/Cargo.toml --target i686-pc-windows-msvc
cargo test --locked --manifest-path crates/pol-auth/Cargo.toml
```

The ordinary suites use local fixtures. Ignored live tests require explicit selection; updater benchmarks can modify the installation path supplied to them. Passing local tests does not establish live-game compatibility.

## Reporting a problem

Report problems through [GitHub Issues](https://github.com/Offset437/Crystal-Launcher/issues). Include the build number, Windows version, launch route, exact error, and steps to reproduce it. Redact account IDs, passwords, one-time codes, friend messages, and session credentials from attachments.

For an account or billing problem, use [Square Enix support](https://support.na.square-enix.com/main.php?id=20&la=1).

## Source layout

- `src/`: launcher, account storage, settings, friends, installer, and updater.
- `crystal-loader/`: 32-bit game host and installed POL core integration.
- `crates/pol-auth/`: shared credential codecs, records, and protocol fixtures.
- `tools/`: release build, installer source, dependency notices, and update signing.

## License and ownership

Crystal's code and original SVG interface icons are licensed under [GPL-3.0-only](LICENSE), with no warranty. [Third-party notices](THIRD_PARTY.md) identify dependencies and separately sourced artwork. The code license does not grant rights to Square Enix's game, artwork, logos, news images, music, or trademarks.

FINAL FANTASY XI © 2002 - 2020 SQUARE ENIX CO., LTD. All Rights Reserved.

FINAL FANTASY is a registered trademark of Square Enix Holdings Co., Ltd. PlayOnline and related Square Enix marks belong to their respective owners.

See the [FINAL FANTASY XI Materials Usage License](https://support.na.square-enix.com/rule.php?id=20&la=1&tag=authc) for Square Enix's conditions. Attribution is not a grant of permission.
