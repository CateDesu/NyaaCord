# Nyaacord

Linux & Windows · [Downloads](https://github.com/CateDesu/NyaaCord/releases) · [Discord](https://discord.com/invite/TQJrbZcgKF)

<img src="web/plugins/nyaacord.gif" alt="Nyaacord icon" width="96">

Nyaacord is a community fork of [Sunbird](https://github.com/sunbird-tools/sunbird) for Discord Phoenix on Linux and Windows. Each release packages Discord Phoenix, the Nyaacord native runtime, default desktop plugins, and an updater into a single executable.

Set `SUNBIRD_TRAY=1` if you want the extra Nyaacord tray menu.

## Download

- Latest Linux AppImage: [Nyaacord-x86_64.AppImage](https://github.com/CateDesu/NyaaCord/releases/latest/download/Nyaacord-x86_64.AppImage)
- Latest Windows executable: [Nyaacord-x86_64.exe](https://github.com/CateDesu/NyaaCord/releases/latest/download/Nyaacord-x86_64.exe)
- Release page: [v0.2.2](https://github.com/CateDesu/NyaaCord/releases/tag/v0.2.2)
- Checksums: [SHA256SUMS.txt](https://github.com/CateDesu/NyaaCord/releases/latest/download/SHA256SUMS.txt)

On Linux, make the AppImage executable and run it:

```sh
chmod +x Nyaacord-x86_64.AppImage
./Nyaacord-x86_64.AppImage
```

On Windows, download and run `Nyaacord-x86_64.exe`. It extracts itself and Discord Phoenix into `%LOCALAPPDATA%\Nyaacord`, creates Start Menu and Desktop shortcuts, and keeps your Discord logins and settings in `%LOCALAPPDATA%\DiscordPhoenix`.

## What each package includes

**Linux AppImage (`Nyaacord-x86_64.AppImage`)**

- Discord Phoenix Linux x86-64 client, build 2233
- Nyaacord launcher and whole-AppImage updater: `AppRun` and `nyaacord-updater`
- Nyaacord native runtime `libphoenix.so` with Linux build 2233 startup and plugin settings fixes, single tray behavior, Nyaacord branding, and icon
- 14 bundled default desktop plugins: `clear-urls`, `dearrow`, `fakenitro`, `fix-embeds`, `global-badges`, `message-latency`, `message-logger`, `no-profile-themes`, `no-reply-mention`, `silent-typing`, `spoiler-mode`, `text-replace`, `translate`, `view-raw`

**Windows executable (`Nyaacord-x86_64.exe`)**

- Discord Phoenix Windows x86-64 client, build 2208
- Self-contained launcher, updater, and tray icon in one static-CRT executable
- Nyaacord native runtime `phoenix.dll`
- Same 14 bundled default desktop plugins (`libplm.plugins.so`)

Plugins live in `plugins/*/src/index.ts` and use the [Phoenix plugin API](crates/runtime/phoenix.d.ts).

## Updates

Nyaacord checks `CateDesu/NyaaCord` GitHub Releases automatically on startup and supports manual **Check for updates** and **Update Nyaacord** actions in **Settings → Sunbird** (`General & Updates`), the dashboard **Updates** tab, and on the command line:

```sh
./Nyaacord-x86_64.AppImage --check-update
./Nyaacord-x86_64.AppImage --update
./Nyaacord-x86_64.AppImage --rollback
```

Updates replace the entire AppImage so the client, launcher, runtime, and default plugins always stay on the same version. Your Discord data, Nyaacord settings, and installed plugins stay untouched. Before replacing the AppImage, the updater saves a `.previous` backup and rolls back automatically if an update fails to start. Older superseded GitHub Releases are automatically pruned when a new stable release is published while keeping their git tags.

The native sidebar label has a fixed length, so the settings tab keeps the **Sunbird** label.

<details>
<summary>Build and install from source</summary>

Build the AppImage from a cloned checkout:

```sh
./scripts/build-appimage.sh --client /path/to/discord-phoenix.AppImage
```

Output goes to `dist/Nyaacord-x86_64.AppImage` along with `dist/SHA256SUMS.txt`. Pass `--version`, `--out`, `--comp gzip|zstd`, or `--skip-cargo` as needed.

Install the launcher and desktop entry from a local checkout, a built AppImage, or the latest release:

```sh
./scripts/nyaacord-linux.sh
```

The installed launcher in `~/.local/share/sunbird/sunbird` checks for updates automatically on startup and supports manual CLI checks:

```sh
~/.local/share/sunbird/sunbird --check-update
~/.local/share/sunbird/sunbird --update
~/.local/share/sunbird/sunbird --rollback
```

Cross-compile the self-contained Windows x86-64 executable from Linux:

```sh
./scripts/build-windows.sh --client /path/to/DiscordPhoenixSetup.exe
```

Output goes to `dist/Nyaacord-x86_64.exe` along with `dist/Nyaacord-x86_64.exe.sha256` and `dist/SHA256SUMS.txt`. Pass `--no-client` for a thin launcher-only build, or manage an existing install via CLI options (`--install-only`, `--check-update`, `--update`, `--rollback`, `--uninstall`).

Run the automation tests from the program root:

```sh
python3 .github/test_discord_notify.py
python3 .github/test_prune_releases.py
```

Prune older superseded releases manually after verifying `dist/`:

```sh
python3 .github/prune_releases.py --dir dist
```

See [AGENTS.md](AGENTS.md) for the repository layout and additional build commands.

</details>

## Upstream contributors

- [mantikafasi](https://github.com/mantikafasi)
- [Amaan Qureshi](https://github.com/amaanq)
