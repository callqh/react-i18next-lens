# React i18next Lens

React i18next Lens brings inline translations, multilingual hover previews, and
resource navigation to React projects in Zed.

It statically analyzes JavaScript and TypeScript with Oxc and connects resolved
translation keys to i18next JSON resources through a focused, read-only language
server.

> React i18next Lens is a breaking, React-focused fork of
> [nguyenphutrong/intl-lens](https://github.com/nguyenphutrong/intl-lens).
> Thank you to [Trong Nguyen](https://github.com/nguyenphutrong) and the original
> contributors for building the foundation of this project.

## Features

- Low-emphasis inlay hints display the selected locale beside translation keys.
- Hover previews show every available locale.
- Each hover translation links to its exact JSON resource value.
- Go to Definition opens the translation resource directly.
- Diagnostics report missing keys and incomplete locale coverage.
- JSON resource changes reload automatically.
- Existing i18next and next-i18next configuration is discovered without a Lens
  project file.

## Install

### Zed Extension Gallery

Open the Extension Gallery with `Ctrl+Shift+X`, search for
`React i18next Lens`, and select **Install**.

The extension downloads the matching language-server binary for macOS, Linux,
or Windows. No separate CLI or MCP server is required.

### Development build

Until the marketplace submission is merged, or when testing local changes:

1. Build the language server:

   ```sh
   cargo build --release -p react-i18next-lens
   ```

2. Put `target/release/react-i18next-lens` on the `PATH` visible to Zed.
3. Run `zed: install dev extension` from the command palette.
4. Select the `crates/intl-lens-extension` directory.

## Quick start

Open a React project that has an i18next configuration and JSON resources. Lens
automatically discovers one root-level configuration named:

```text
next-i18next.config.*
i18next.config.*
i18n.config.*
```

Supported configuration suffixes are:

```text
.js .jsx .cjs .mjs .ts .tsx .cts .mts .json
```

The configuration is statically analyzed and is never executed. For example,
this standard `next-i18next.config.js` works without extra Lens configuration:

```js
module.exports = {
  i18n: {
    defaultLocale: "en",
    locales: ["en", "ja", "zh-CN"],
  },
  localePath: "public/locales",
};
```

With resources such as:

```text
public/locales/en/common.json
public/locales/ja/common.json
public/locales/zh-CN/common.json
```

Lens resolves static usages including:

```tsx
const { t } = useTranslation("common");

<Button>{t("buttons.save")}</Button>
<Trans i18nKey="buttons.save" ns="common" />
```

It also supports `keyPrefix`, per-call `ns`, selector syntax, `i18next.t`,
`getFixedT`, static `defaultValue`, and project i18n wrapper modules.

## Choose the inlay-hint locale

Inlay hints use the first configured locale by default. To persist another
locale locally, add this to Zed settings:

```jsonc
{
  "lsp": {
    "react-i18next-lens": {
      "initialization_options": {
        "inlayLocale": "zh-CN"
      }
    }
  }
}
```

Run `zed: restart language server` after changing the value. When the requested
locale does not exist in the project, Lens safely falls back to the first
configured locale.

This is editor-local state: it does not add a configuration file to the project
or affect other contributors.

## Optional project overrides

Most projects should not create a Lens-specific file. Use
`react-i18next-lens.json` only when automatic discovery is ambiguous or dynamic
configuration values require a static override:

```json
{
  "extends": "./next-i18next.config.js"
}
```

A project without an existing i18next configuration can declare normalized
values directly:

```json
{
  "sourceLocale": "en",
  "locales": ["en", "ja", "zh-CN"],
  "resources": ["public/locales/{locale}/{namespace}.json"],
  "defaultNamespace": "common"
}
```

Resource templates use `{locale}` and `{namespace}`. The i18next placeholders
`{{lng}}` and `{{ns}}` are normalized automatically. Translation resources must
be JSON; physical locale files determine coverage, regardless of `fallbackLng`.

## Supported scope

React i18next Lens intentionally focuses on React:

- JavaScript, JSX, TypeScript, and TSX
- `react-i18next`, `i18next`, and `next-i18next`
- i18next JSON v4 resources
- statically resolvable translation keys

Dynamic keys are recorded as unresolved facts for a future resolver. Lens does
not guess them, and it never deletes translations automatically.

Vue, Svelte, Angular, PHP, Blade, Dart, Flutter, YAML, ARB, custom regex
extractors, and PHP resource files are outside the project scope.

Zed currently exposes Lens through standard LSP capabilities. It can display a
translation as an inlay hint, but cannot fully replace the source key and reveal
the original only when selected as some VS Code decoration-based extensions do.

## Troubleshooting

- **Hover works but inlay hints do not:** enable inlay hints in Zed, then run
  `zed: restart language server`.
- **The displayed language is unexpected:** set `inlayLocale` to an exact locale
  from the project's configuration and restart the language server.
- **No translations are detected:** verify that exactly one supported root-level
  i18next configuration is discoverable and that its source locale has JSON
  resources.
- **The language server does not start:** run `zed: open log` and look for
  `react-i18next-lens` installation or configuration errors.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Architecture and parser decisions are documented in
[`docs/architecture.md`](docs/architecture.md) and
[`docs/research/source-parser-options.md`](docs/research/source-parser-options.md).

## License

MIT. See [`LICENSE`](LICENSE).
