# React i18next Lens

This context describes `react-i18next-lens`, a developer tool that understands translation usage in React code and connects it to i18next locale resources.

## Language

**React i18next workspace**:
A JavaScript or TypeScript React workspace that uses `react-i18next`, its underlying `i18next` interface, or `next-i18next`.
_Avoid_: Generic i18n workspace, multi-framework project

**Translation key**:
The canonical identity of one translated message, independent of whether source code spells its namespace with `:` or derives it from React i18next context.
_Avoid_: Raw key string, lookup variant

**Namespace**:
The i18next grouping that qualifies a translation key through `useTranslation`, `getFixedT`, an `ns` option, or an explicit namespace separator.
_Avoid_: File prefix

**Source locale**:
The locale whose message is shown as the default inline explanation and used as the reference for translation analysis.
_Avoid_: Default language

**Translation resource**:
An i18next JSON v4 document containing nested or flat translation keys, including namespaces, plurals, interpolation values, arrays, and objects.
_Avoid_: Generic locale file, YAML resource, ARB resource, PHP translation file

**Key resolution**:
The analyzer's classification of a translation expression as either a statically known translation key or a dynamic expression that cannot yet be resolved.
_Avoid_: Assuming every translation call contains a usable key

**Configuration source**:
An existing i18next or next-i18next JavaScript, TypeScript, or JSON module that is statically interpreted and normalized into workspace configuration without executing project code.
_Avoid_: Executable editor configuration, duplicated Lens-only i18next settings

**Mutation preview**:
A complete, validated description of translation-resource edits that must be explicitly accepted before any file is changed.
_Avoid_: Best-effort direct JSON writes, automatic cleanup

## Boundaries

- The product display name is `React i18next Lens`; repositories, Cargo packages, binaries, extension identifiers, and configuration files use `react-i18next-lens` where their naming rules allow it.
- Translation resources are JSON only.
- YAML, ARB, PHP, and other resource formats are intentionally unsupported.
- Resource discovery may support common i18next directory layouts, but every discovered resource must use i18next JSON v4 semantics.
- The first release resolves static translation keys only.
- Dynamic key expressions retain their source location and unresolved reason as analysis data, but do not count as used keys.
- Dynamic key evaluation is a planned feature; the current architecture must preserve this extension point without guessing runtime values.
- Locale resource discovery is an initialization aid, not a runtime source of truth.
- Runtime analysis uses explicit workspace configuration for locale paths, source locale, and resource layout.
- The project-root `react-i18next-lens.json` file is the single source of truth for domain configuration.
- `react-i18next-lens.json` may extend an existing i18next or next-i18next configuration source and contain only Lens-specific settings or explicit overrides for values that cannot be resolved statically.
- Configuration sources support `.js`, `.jsx`, `.cjs`, `.mjs`, `.ts`, `.tsx`, `.cts`, `.mts`, and `.json`.
- Static configuration analysis supports common CommonJS, ESM, `defineConfig`, and `i18next.init` forms; project configuration code is never executed.
- Values that remain dynamic after static analysis produce configuration diagnostics and require explicit overrides.
- `sourceLocale` is required. Initialization may suggest `en` when it is discovered, but runtime analysis never assumes English implicitly.
- A missing `sourceLocale` setting or missing source-locale resources makes the workspace configuration invalid and produces actionable diagnostics.
- Translation coverage is based on keys physically present in each target locale, not merely values resolvable through i18next fallback.
- When fallback configuration can resolve a physically missing key, diagnostics may report the effective fallback locale while still classifying the target locale as untranslated.
- LSP, CLI, and MCP adapters may accept workspace path, configuration path, and operational settings such as log level, but must not redefine locale paths, source locale, or analysis semantics independently.
- Automatic discovery may suggest configuration when exactly one common i18next layout is unambiguous; ambiguous candidates produce a configuration diagnostic instead of being selected silently.
- LSP, CLI, and MCP adapters must resolve the same workspace configuration into the same analysis behavior.
- The first release may add a missing static key to translation resources through an explicit mutation preview.
- When adding a key missing from the source locale, a statically resolved i18next `defaultValue` becomes its initial source message.
- Without a static `defaultValue`, the canonical key itself becomes a visible source-locale placeholder and remains an incomplete-message diagnostic until replaced.
- Target locales are changed only when the user provides an actual translated message; placeholders are never bulk-written across locales or counted as completed translations.
- A mutation validates every target before committing changes, preserves file style and key ordering, and uses atomic replacement so partial writes are not exposed.
- CLI and MCP mutations require an explicit apply operation; LSP mutations require a user-invoked code action.
- Unused keys are reported but never deleted automatically. Automatic translation, key renaming, and overwriting existing messages are outside the first-release mutation scope.
