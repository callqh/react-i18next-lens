# React i18next Lens

React i18next analysis for editors, CI, and AI agents.

React i18next Lens connects statically resolved translation usages in
JavaScript/TypeScript React code to i18next JSON resources. It provides a Zed
language server, a CLI, and an MCP server backed by the same Rust core.

Repository: [callqh/react-i18next-lens](https://github.com/callqh/react-i18next-lens)

> This project is a breaking, React-focused fork of
> [nguyenphutrong/intl-lens](https://github.com/nguyenphutrong/intl-lens).
> Thank you to [Trong Nguyen](https://github.com/nguyenphutrong) and the original
> contributors for the foundation. The fork intentionally narrows the product
> scope and is not presented as an upstream continuation.

## Scope

Supported:

- JavaScript, JSX, TypeScript, and TSX
- `react-i18next`, `i18next`, and `next-i18next`
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

- source-locale messages as low-emphasis inlay hints
- hover previews across locales
- go to exact translation definition
- missing and incomplete translation diagnostics
- explicit code actions for supported safe edits
- automatic reload when JSON resources change

Zed exposes these features through standard LSP capabilities. It cannot replace
the source key visually and reveal it only on selection; a future editor client
with a decoration API can render that experience using the same core annotation
data.

## Configuration

The project entry point is `react-i18next-lens.json`:

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

Lens-specific overrides belong in the same project file. `sourceLocale` must be
resolved explicitly; the runtime does not silently assume English.

Translation resources remain strict JSON regardless of the configuration file
extension.

## Build

```sh
git clone https://github.com/callqh/react-i18next-lens.git
cd react-i18next-lens
cargo build --release -p react-i18next-lens
```

The resulting programs are:

```text
target/release/react-i18next-lens
target/release/react-i18next-lens-cli
target/release/react-i18next-lens-mcp
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
