# red bird app lol

<img src="web/plugins/sunbird.png" alt="red bird app logo" width="112">

Red bird app is a community fork of [Sunbird](https://github.com/sunbird-tools/sunbird).
It contains an extension runtime and TypeScript plugins for a separately obtained Phoenix client.

The Linux fork retains startup and plugin settings fixes for Phoenix build 2233, updater status
fixes, and one tray icon.

The Linux installer supports a locally managed loader through `SUNBIRD_LOCAL_LOADER`.
Set `SUNBIRD_TRAY=1` if you want the additional Sunbird tray menu.

## Plugins

Plugins live in `plugins/*/src/index.ts` and use the [Phoenix plugin API](crates/runtime/phoenix.d.ts).
On desktop, open **Settings → Sunbird** to manage installed plugins. Plugin updates use the
**Updates** section in that page.

See [AGENTS.md](AGENTS.md) for the repository layout and build and test commands.

## Upstream contributors

- [mantikafasi](https://github.com/mantikafasi)
- [Amaan Qureshi](https://github.com/amaanq)
