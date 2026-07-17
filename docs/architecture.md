# React i18next Lens architecture

## Status

This document defines the implemented architecture for the breaking fork of
`intl-lens`. The product is intentionally narrowed to React applications that
use `react-i18next`, `i18next`, or `next-i18next`.

The core migration is complete: Oxc analysis, static configuration,
span-preserving JSON catalog, audit, mutation planning, and immutable workspace
snapshots are shared by the LSP, CLI, and MCP adapters. Dynamic key resolution
remains an explicit future feature.

The public product name is **React i18next Lens** and machine-facing names use
`react-i18next-lens`.

## Product boundary

Supported source files:

- JavaScript, JSX, TypeScript, and TSX
- `react-i18next`, `i18next`, and `next-i18next` APIs
- statically resolvable translation keys

Supported translation resources:

- strict i18next JSON v4
- nested and flat keys
- namespace-per-file and single-file layouts described by resource templates

Explicitly unsupported:

- Vue, Svelte, Angular, PHP, Blade, Dart, Flutter, and unrelated i18n APIs
- YAML, ARB, PHP, and executable translation resources
- user-defined regex extraction patterns
- guessing dynamic translation keys
- automatic deletion of unused keys

Dynamic expressions are retained as unresolved analysis facts with source spans
and reasons. They are not counted as used keys until a future resolver can prove
their values.

## Package topology

The workspace contains one Rust package whose library owns all product logic.
Three binaries are delivery adapters:

```text
react-i18next-lens
├── library
│   ├── domain
│   ├── configuration
│   ├── analysis
│   ├── catalog
│   ├── audit
│   ├── mutation
│   └── workspace
├── react-i18next-lens       LSP binary
├── react-i18next-lens-cli   CLI binary
└── react-i18next-lens-mcp   MCP binary
```

Binary crates may translate transport types, render output, and manage process
lifecycle. They must not parse React source, interpret i18next configuration,
read translation semantics, or implement audit rules.

## Dependency rule

Dependencies point toward the domain:

```text
LSP / CLI / MCP
       │
       ▼
Workspace application API
       │
       ├── React source analysis
       ├── Configuration normalization
       ├── Translation catalog
       ├── Audit
       └── Mutation planning
                    │
                    ▼
                  Domain
```

Oxc AST and semantic types stop at the React source analyzer boundary. Tower
LSP, CLI, and MCP protocol types stop at their adapter boundaries. The domain
uses owned Rust values.

## Domain model

### Identity

`TranslationKey` is the canonical identity of one message:

```text
TranslationKey {
  namespace: Namespace,
  path: KeyPath,
}
```

Source spellings such as `common:buttons.save`, a `useTranslation("common")`
binding followed by `t("buttons.save")`, and an `ns` option followed by the same
path normalize to the same identity. Formatting a key back into source syntax is
separate from identity.

### Coordinates

All internal parser and JSON spans are UTF-8 byte ranges:

```text
ByteSpan { start, end }
```

LSP positions are UTF-16 line/character coordinates:

```text
TextRange { start: TextPosition, end: TextPosition }
```

Conversion occurs exactly once at the LSP adapter boundary using the document
text associated with the same snapshot generation.

### Source analysis

`ReactSourceAnalyzer` parses one JS/TS/JSX/TSX document using Oxc and returns:

```text
SourceAnalysis {
  usages: Vec<TranslationUsage>,
  unresolved: Vec<UnresolvedUsage>,
  diagnostics: Vec<AnalysisDiagnostic>,
}
```

Initial semantic coverage:

- `useTranslation()` with one namespace or a namespace array
- `keyPrefix`
- `t(key)` and per-call `ns`
- selector calls such as `t($ => $.buttons.save)`
- imported or instance `i18next.t(key)`
- `getFixedT(language, namespace, keyPrefix)`
- `<Trans i18nKey="..." ns="..." />`
- static `defaultValue`

The analyzer resolves imports and bindings before treating an identifier named
`t` as a translation function. An unrelated local `t` function must not produce
a translation usage.

### Translation catalog

The catalog preserves both semantic values and exact JSON source locations:

```text
CatalogEntry {
  key: TranslationKey,
  locale: Locale,
  value: MessageValue,
  file: ResourceFile,
  key_span: ByteSpan,
  value_span: ByteSpan,
}
```

Each resource file is read once per generation. Hover, definition, audit, and
mutation planning consume catalog data and never reread files independently.

### Key resolution

Static and dynamic expressions are explicit variants:

```text
KeyResolution::Static(TranslationKey)
KeyResolution::Dynamic {
  span: ByteSpan,
  reason: DynamicReason,
}
```

Dynamic usages do not suppress unused-key diagnostics. Audits must state when
dynamic usages make an unused result provisional.

## Configuration

The zero-configuration path discovers exactly one root-level
`next-i18next.config.*`, `i18next.config.*`, or `i18n.config.*` source. An
optional `react-i18next-lens.json` may select a source or provide overrides:

```json
{
  "extends": "./next-i18next.config.js"
}
```

Configuration source suffixes:

```text
.js .jsx .cjs .mjs .ts .tsx .cts .mts .json
```

Configuration is statically interpreted with Oxc. Project code is never
executed by the editor tool. Direct CommonJS exports, ESM default exports,
`defineConfig`, `i18next.init`, literal arrays and strings, `path.resolve`, and
dynamic-import resource templates are normalized into one `WorkspaceConfig`.
Values hidden behind runtime branches, imported modules, or computed
expressions require Lens overrides.

Dynamic values produce actionable diagnostics and may be overridden in the Lens
configuration. `sourceLocale` is required after normalization and never silently
defaults to English.

Automatic source discovery feeds the same explicit normalized configuration to
LSP, CLI, and MCP. Ambiguous sources produce a diagnostic instead of being
selected silently.

## Coherent workspace state

Readers observe one immutable generation:

```text
WorkspaceSnapshot {
  generation: Generation,
  config: WorkspaceConfig,
  documents: DocumentIndex,
  analyses: SourceAnalysisIndex,
  catalog: TranslationCatalog,
  audit: AuditReport,
}
```

Reload builds a complete candidate snapshot away from readers. After validation,
one atomic swap publishes it. A request never combines old configuration with a
new catalog or new documents with old analysis.

Open editor buffers override disk source for their document version. File-system
events schedule a debounced rebuild; stale generations are discarded rather
than published after a newer rebuild.

## Workspace application API

The core exposes a typed application API instead of one transport-shaped request
enum:

```text
Workspace::open_document
Workspace::change_document
Workspace::close_document
Workspace::reload
Workspace::annotation_at
Workspace::definitions_at
Workspace::diagnostics_for
Workspace::audit
Workspace::preview_mutation
Workspace::apply_mutation
```

Every read response includes the generation used to compute it. Mutation apply
requires the preview generation and expected file fingerprints; stale previews
are rejected.

## Audit semantics

- A target locale is translated only when the key physically exists with a
  completed message in that locale.
- Runtime fallback may be reported but does not count as translation coverage.
- A key equal to the canonical key placeholder remains incomplete.
- Dynamic source usages are reported separately and make unused results
  provisional, never silently safe to delete.
- Placeholder/interpolation validation compares parsed placeholder sets between
  source and target messages.

## Safe mutation

The first release only adds missing static keys. It does not delete, rename,
translate, or overwrite messages.

Mutation is a two-phase operation:

1. Produce a preview containing target files, before/after edits, fingerprints,
   and validation results.
2. On explicit apply, revalidate every fingerprint, write temporary siblings,
   fsync where supported, and atomically replace all targets. If preparation
   fails, no target is replaced.

A static `defaultValue` supplies a new source-locale message. Otherwise the
canonical key is inserted as a visible incomplete placeholder. Target locales
are written only when the user provides real translated values.

## Adapter behavior

### LSP and Zed

Zed receives source-locale text as a low-emphasis inlay hint while source code
remains visible. Hover exposes locale values, definition jumps to exact JSON
spans, diagnostics report missing and incomplete messages, and user-invoked code
actions preview supported mutations.

Visual replacement of the source key with translated text is not part of the
standard LSP contract and is outside the Zed adapter boundary. Annotation data
remains editor-neutral so a future editor client with decoration APIs can render
that experience.

### CLI

CLI commands render core audit and mutation results. Machine-readable JSON is a
versioned schema. A mutation requires an explicit apply flag.

### MCP

MCP tools expose the same snapshot-backed queries and mutation previews. Applying
a mutation is a separate explicit tool call with the preview identifier.

## Verification gates

1. The workspace builds with one copy of every library module and test.
2. Unsupported language and regex-pattern fixtures no longer produce usages.
3. Oxc fixtures cover every supported React i18next form and reject unrelated
   identifiers named `t`.
4. Unicode fixtures prove UTF-8 span to UTF-16 LSP conversion.
5. Catalog fixtures prove exact definition ranges for nested and flat JSON v4.
6. Snapshot concurrency tests prove readers never observe mixed generations.
7. Mutation fault-injection tests prove stale previews and partial writes are
   rejected safely.
8. LSP, CLI, and MCP golden tests produce consistent results for one workspace.
9. A release build and the Zed extension resolve the renamed binary.

## Migration order

Each stage must compile and keep its focused tests passing:

1. Establish the library/adapters boundary and rename public artifacts.
2. Introduce domain identities, coordinate types, and normalized configuration.
3. Add `ReactSourceAnalyzer` behind compatibility conversion at existing call
   sites, then delete regex extraction.
4. Build the span-preserving JSON catalog and move hover/definition/audit reads.
5. Publish immutable snapshots and remove independently locked backend state.
6. Replace direct writes with preview/apply mutation.
7. Thin the LSP, CLI, and MCP adapters and delete compatibility code.
8. Rewrite product documentation and release assets with explicit upstream fork
   attribution.
