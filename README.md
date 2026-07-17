# React i18next Lens

React i18next analysis for Zed.

React i18next Lens connects statically resolved translation usages in
JavaScript/TypeScript React code to i18next JSON resources through a focused,
read-only Zed language server.

Repository: [callqh/react-i18next-lens](https://github.com/callqh/react-i18next-lens)

> This project is a breaking, React-focused fork of
> [nguyenphutrong/intl-lens](https://github.com/nguyenphutrong/intl-lens).
> Thank you to [Trong Nguyen](https://github.com/nguyenphutrong) and the original
> contributors for the foundation. The fork intentionally narrows the product
> scope and is not presented as an upstream continuation.

## Scope

Supported:

- JavaScript, JSX, TypeScript, and TSX
- `react-i18next`, `i18next`, `next-i18next`, and project `i18n` wrapper modules
- `useTranslation`, `t`, selector syntax, `i18next.t`, `getFixedT`, and `Trans`
- i18next JSON v4 resources
- static translation keys
- namespace, `keyPrefix`, per-call `ns`, and static `defaultValue`

Intentionally unsupported:

- Vue, Svelte, Angular, PHP, Blade, Dart, and Flutter integrations
- YAML, ARB, and PHP translation resources
- user-defined regex extraction patterns
- guessing dynamic keys
- automatic deletion of unused translations

Dynamic key expressions are retained as unresolved analysis facts for a future
resolver. They are never guessed or treated as safe evidence for deletion.

## Editor features

- selected-locale messages as low-emphasis inlay hints
- hover previews across locales with clickable links to each resource value
- go to exact translation definition
- missing and incomplete translation diagnostics
- automatic reload when JSON resources change

Zed exposes these features through standard LSP capabilities. It cannot replace
the source key visually and reveal it only on selection; a future editor client
with a decoration API can render that experience using the same core annotation
data.

The inlay-hint locale defaults to the first configured locale. Persist a
different locale locally in Zed without changing project configuration:

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

Restart the language server after changing this value. If the locale is not
part of the project's configured locale list, Lens safely falls back to the
first configured locale.

## Configuration

For standard projects, no Lens-specific configuration is required. The server
automatically discovers exactly one root-level `next-i18next.config.*`,
`i18next.config.*`, or `i18n.config.*` source.

Use the optional `react-i18next-lens.json` only when discovery is ambiguous or
dynamic project values need static overrides:

```json
{
  "extends": "./next-i18next.config.js"
}
```

Existing i18next or next-i18next configuration is statically analyzed rather
than executed. Configuration sources may use:

```text
.js .jsx .cjs .mjs .ts .tsx .cts .mts .json
```

Lens-specific overrides belong in that optional project file. `sourceLocale`
must still resolve from the existing i18next config or an explicit override;
the runtime does not silently assume English.

For a project without an existing config, declare the normalized values
directly:

```json
{
  "sourceLocale": "en",
  "locales": ["en", "ja", "zh-CN"],
  "resources": ["public/locales/{locale}/{namespace}.json"],
  "defaultNamespace": "common"
}
```

Resource templates use `{locale}` and `{namespace}`. i18next `{{lng}}` and
`{{ns}}` placeholders found in an extended config are normalized to those
names. Physical locale files determine coverage; `fallbackLng` does not make a
missing target-locale message count as translated.

Translation resources remain strict JSON regardless of the configuration file
extension.

The core publishes immutable workspace generations so configuration, open
documents, Oxc analysis, and JSON spans cannot be mixed across reloads. Startup
loads configuration and translation resources only; React source is analyzed
when Zed opens or changes a document.

## Build

```sh
git clone https://github.com/callqh/react-i18next-lens.git
cd react-i18next-lens
cargo build --release -p react-i18next-lens
```

The resulting program is:

```text
target/release/react-i18next-lens
```

For local Zed development, put `react-i18next-lens` on the environment `PATH`
seen by Zed, then install this repository as a dev extension.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

The target module boundaries and migration gates are documented in
[`docs/architecture.md`](docs/architecture.md). The Oxc parser decision is
documented in
[`docs/research/source-parser-options.md`](docs/research/source-parser-options.md).

## License

MIT. See [`LICENSE`](LICENSE).
