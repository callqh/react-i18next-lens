# React i18next Lens architecture

## Product boundary

React i18next Lens is a focused, read-only Zed language server for React
applications that use `react-i18next`, `i18next`, or `next-i18next`.

Supported:

- JavaScript, JSX, TypeScript, and TSX
- statically resolvable translation keys
- strict i18next JSON v4 resources
- nested and flat keys
- namespace-per-file and single-file resource layouts

Deferred:

- dynamic key evaluation
- full-workspace audit and CI integration
- CLI and MCP delivery adapters
- translation-resource mutation, automatic translation, rename, and deletion
- non-React frameworks and non-JSON resources

Dynamic expressions remain explicit unresolved analysis facts. They are never
guessed.

## Package topology

```text
Zed extension
     │ launches
     ▼
react-i18next-lens LSP
     │
     ▼
Rust library
├── domain
├── configuration
├── analysis
├── catalog
└── workspace
```

The Zed extension only locates and launches the binary. LSP protocol types stay
in the backend adapter; Oxc types stay inside source analysis; the domain uses
owned Rust values.

## Startup and document lifecycle

LSP initialization must remain proportional to translation configuration and
resource size, not repository source size.

```text
initialize
  ├── statically normalize i18next configuration
  ├── load span-preserving JSON resources
  └── publish an empty document index

didOpen / didChange
  ├── analyze that document with Oxc
  └── atomically publish document + analysis

didClose
  └── remove that document + analysis
```

The LSP never scans the complete JavaScript/TypeScript source tree at startup.
This prevents large React workspaces from blocking the LSP handshake and making
Hover or Inlay Hints appear unavailable after an editor restart.

Translation-resource or configuration changes rebuild the catalog and
reanalyze only documents currently open in Zed.

## Domain model

`TranslationKey` is the canonical identity of one message:

```text
TranslationKey {
  namespace: Namespace,
  path: KeyPath,
}
```

Source spellings such as `common:buttons.save`, a `useTranslation("common")`
binding followed by `t("buttons.save")`, and a per-call `ns` option normalize to
the same identity.

Static and dynamic expressions are explicit variants:

```text
KeyResolution::Static(TranslationKey)
KeyResolution::Dynamic {
  span: ByteSpan,
  reason: DynamicReason,
}
```

All parser and JSON spans are UTF-8 byte ranges. Conversion to UTF-16
line/character positions occurs at the LSP boundary using text from the same
workspace generation.

## React source analysis

`ReactSourceAnalyzer` parses one open document using Oxc. Initial semantic
coverage includes:

- `useTranslation()` with one namespace or a namespace array
- `keyPrefix`
- `t(key)` and per-call `ns`
- selector calls such as `t($ => $.buttons.save)`
- imported or instance `i18next.t(key)`
- `getFixedT(language, namespace, keyPrefix)`
- `<Trans i18nKey="..." ns="..." />`
- static `defaultValue`

Imports and bindings are resolved before an identifier named `t` is accepted as
a translation function.

## Translation catalog

The catalog reads each configured resource once per generation and preserves
semantic values plus exact JSON locations:

```text
CatalogEntry {
  key: TranslationKey,
  locale: String,
  value: MessageValue,
  file: PathBuf,
  key_span: ByteSpan,
  value_span: ByteSpan,
}
```

Hover, Inlay Hints, completion, definitions, and diagnostics all consume this
shared catalog.

## Configuration

The zero-configuration path discovers exactly one root-level
`next-i18next.config.*`, `i18next.config.*`, or `i18n.config.*` source. An
optional `react-i18next-lens.json` may disambiguate discovery or provide static
overrides.

Configuration is statically interpreted and project code is never executed.
Supported configuration suffixes are:

```text
.js .jsx .cjs .mjs .ts .tsx .cts .mts .json
```

`sourceLocale` is required after normalization. The inlay locale is separate:
it may be persisted through Zed's local LSP `initialization_options`, defaults
to the first configured locale, and never changes source-locale diagnostics.

## Coherent workspace state

Readers observe one immutable snapshot:

```text
WorkspaceSnapshot {
  generation: Generation,
  config: WorkspaceConfig,
  documents: AnalyzedDocumentIndex,
  catalog: TranslationCatalog,
}
```

Reload and document changes build a complete candidate before one atomic swap.
A request cannot combine an old configuration with a new catalog or a new open
buffer with stale analysis.

## LSP behavior

- Inlay Hints show the selected locale's message.
- Hover lists all physical locale values with clickable JSON locations.
- Go to Definition returns exact translation-resource spans.
- Diagnostics report missing source and target translations.
- Completion suggests statically known translation keys.
- LSP initialization options select the locally persisted inlay locale.
- JSON/configuration file changes refresh catalog-backed editor features.

Visual replacement of source keys is outside the standard LSP contract. The
server remains read-only and never edits translation resources.

## Verification gates

1. Workspace loading produces no source documents or analyses before `didOpen`.
2. Oxc fixtures cover supported React i18next forms and reject unrelated `t`
   identifiers.
3. Unicode fixtures prove UTF-8 span to UTF-16 LSP conversion.
4. Catalog fixtures prove exact nested and flat JSON definition ranges.
5. Snapshot concurrency tests prevent mixed document and analysis generations.
6. Add LSP protocol tests for Hover, Inlay Hints, definitions, and locale changes
   before treating those protocol paths as a completed automated gate.
7. Release output contains only the LSP binary and the Zed extension resolves it.
