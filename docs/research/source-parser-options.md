# Source parser decision for React i18next Lens

Date: 2026-07-17

## Product scope

React i18next Lens targets a **React i18next workspace** only:

- JavaScript, TypeScript, JSX, and TSX source files;
- `react-i18next`;
- the underlying `i18next` interface;
- `next-i18next` projects using the same runtime semantics.

Vue, Svelte, Angular, PHP, Blade, Dart, Flutter, and other i18n frameworks are outside the target scope.

## Decision

Use **Oxc parser + Oxc semantic analysis** for source extraction. Do not keep a generic multi-language extractor registry and do not adopt Yuku now.

The source-analysis implementation should be one deep `ReactSourceAnalyzer` module. There is only one parser implementation, so a public parser trait would create a hypothetical seam without a second adapter.

```text
ReactSourceAnalyzer
├── Oxc parser
├── Oxc semantic analysis
├── React i18next binding resolution
├── canonical translation-key resolution
└── exact UTF-8 byte spans
```

This is primarily a correctness and locality decision. Performance must be validated against React i18next Lens workloads rather than upstream parser benchmarks.

## Why the current regex implementation should be replaced

The current `KeyFinder` runs every configured regular expression across the complete document and uses additional regular expressions for dynamic templates and namespace inference.

That implementation cannot reliably distinguish:

- executable calls from syntax-looking text in comments or strings;
- a React i18next `t` binding from an unrelated function named `t`;
- lexical aliases and destructured hook results;
- nested, multiline, or selector-style expressions;
- the relationship between `useTranslation`, `getFixedT`, an `ns` override, and a later translation call.

The default patterns are also duplicated between `config.rs` and `key_finder.rs`, which allows behaviour to drift.

## Why Oxc

Oxc provides native Rust crates for parsing and traversing modern ECMAScript, TypeScript, JSX, and TSX. Its parser takes an allocator, source text, and source type, returns an AST with diagnostics, and records byte-oriented spans. Scope binding and symbol resolution are deliberately handled by Oxc semantic analysis rather than guessed by the parser.

Primary sources:

- [Oxc parser rustdoc](https://docs.rs/oxc_parser/latest/oxc_parser/)
- [Oxc parser architecture](https://oxc.rs/docs/learn/architecture/parser)
- [Oxc repository](https://github.com/oxc-project/oxc)

Oxc aligns with the existing Rust release process and does not require Node.js, WebAssembly, Zig, or a sidecar process.

## Why not Yuku now

Yuku is a high-performance JavaScript and TypeScript toolchain written in Zig. Its official production interfaces currently target Zig and Node.js; the project does not document a first-party Rust crate suitable for direct use by React i18next Lens.

Adoption would therefore add Zig/FFI or another runtime interface to the macOS, Linux, and Windows release matrix. Its claimed parser speed does not offset that integration and distribution cost for a Rust-native language server.

Primary sources:

- [Yuku documentation](https://yuku.fyi/)
- [Yuku repository](https://github.com/yuku-toolchain/yuku)

Yuku can be reconsidered if it publishes a stable Rust interface and demonstrates a material advantage on React i18next Lens fixtures.

## Why not Tree-sitter now

Tree-sitter has an official Rust binding, incremental parsing, error recovery, and many language grammars. Those strengths matter most for a multi-language editor engine that retains syntax trees across edits.

React i18next Lens has deliberately narrowed its product scope to React i18next. Adding Tree-sitter plus multiple grammar packages would create more implementation and build surface than the product needs. Oxc provides a more direct typed AST and semantic model for the one supported language family.

Primary sources:

- [Tree-sitter introduction](https://tree-sitter.github.io/)
- [Tree-sitter query documentation](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/index.html)

## Required React i18next coverage

The analyzer should resolve these source shapes structurally:

- `useTranslation()` with no namespace;
- `useTranslation("common")` and namespace arrays;
- destructured and tuple hook results;
- aliased `t` bindings;
- `keyPrefix` and per-call `ns` overrides;
- `t("key")` and static template literals;
- selector calls such as `t($ => $.account.title)`;
- `i18next.t("key")`;
- functions returned by `getFixedT`;
- `<Trans i18nKey="key" ns="common" />`;
- explicit namespace spelling such as `common:buttons.save`;
- static dynamic-prefix cases represented as a canonical prefix rather than a complete key.

These shapes are documented by the upstream projects:

- [react-i18next useTranslation](https://react.i18next.com/latest/usetranslation-hook)
- [react-i18next Trans](https://react.i18next.com/latest/trans-component)
- [i18next interface](https://www.i18next.com/overview/api)

## Internal model

Oxc spans remain UTF-8 byte offsets. The analyzer should return canonical domain values and must not expose Oxc AST types:

```rust
pub struct ReactDocumentAnalysis {
    pub occurrences: Vec<KeyOccurrence>,
    pub diagnostics: Vec<SourceDiagnostic>,
}

pub struct KeyOccurrence {
    pub key: CanonicalKey,
    pub source_spelling: String,
    pub span: ByteSpan,
    pub kind: OccurrenceKind,
}
```

The Text coordinates module converts byte spans to negotiated LSP positions. Replacing regex with an AST does not remove the UTF-8 to UTF-16 requirement.

## Migration

1. Build characterization fixtures from existing React/i18next tests and real project examples.
2. Add false-positive fixtures for comments, strings, unrelated `t` functions, aliases, incomplete JSX, Unicode, and malformed code.
3. Introduce `CanonicalKey`, `ByteSpan`, and the Text coordinates module.
4. Implement `ReactSourceAnalyzer` with Oxc parser traversal while retaining current behaviour behind tests.
5. Add semantic binding resolution for `useTranslation`, `i18next`, and `getFixedT`.
6. Route LSP document analysis and whole-workspace audit through the same implementation.
7. Remove generic `functionPatterns` and unsupported-framework defaults as an intentional product-scope change.
8. Delete the regex `KeyFinder` after parity and correctness acceptance.

## Acceptance gates

- Current React i18next fixtures remain supported.
- Comments and unrelated strings never produce translation-key occurrences.
- Unrelated functions named `t` are not treated as React i18next bindings.
- Namespace and `keyPrefix` resolution produce one canonical translation key.
- Selector-style translation calls are supported.
- Incomplete editor code returns useful diagnostics without crashing.
- CJK, emoji, combining characters, and CRLF produce correct LSP ranges.
- Benchmarks record cold workspace audit time, open-document analysis latency, peak memory, and release binary size.
- LSP, CLI, and MCP consume the same analysis results without importing Oxc types.
