# Changelog

All notable changes to this project are documented in this file.
This changelog is automatically generated from GitHub Releases.

---
## [0.4.0](https://github.com/koxudaxi/t-linter/releases/tag/0.4.0) - 2026-03-23

## Breaking Changes

### Language Detection Changes
* HTML validation backend switched from tree-sitter to `tstring-html` - HTML templates annotated with `"html"` are now validated through the `tstring-html` Rust backend instead of tree-sitter parsing. This may produce different diagnostics (different error messages, positions, or severity) for existing HTML templates that previously passed or failed validation. (#21)

### Default Behavior Changes
* HTML formatting now applied where previously none existed - The `format` command now formats HTML templates via `tstring_html::format_template`. Previously, HTML templates were not formatted (no match arm existed). Users running `t-linter format` may see unexpected changes to their HTML template strings. (#21)
* HTML and T-HTML formatting no longer support configurable line length - The `FormatOptions` struct and all `*_with_options` formatting functions (`format_document_with_options`, `format_document_in_file_with_options`, `format_document_range_with_options`) have been removed from `t-linter-core`. HTML and T-HTML templates are now formatted using the upstream `tstring_html::format_template` default behavior. Code depending on the `t-linter-core` Rust API for these functions will fail to compile. (#25)
* Removed `project_config` module from `t-linter-core` public API - The `ProjectConfig`, `find_config_root`, `load_project_config`, and `load_project_config_for_path` exports have been removed. The config-loading logic for discovery (exclude/extend-exclude/ignore-file) has been inlined into the CLI crate. External consumers of these APIs will fail to compile. (#25)

### CLI Changes
* Removed `--line-length` flag from `format` command - The `t-linter format --line-length <N>` option has been removed. Users who relied on this flag to control HTML/T-HTML formatter print width will get an unrecognized argument error. (#25)

### Configuration Changes
* Removed `line-length` from `pyproject.toml` configuration - The `tool.t-linter.line-length` key is no longer recognized by the core library. Existing `pyproject.toml` files with this key will silently ignore it (the discovery module uses `serde(default)` deserialization). (#25)

### LSP Protocol Changes
* Removed `printWidth` and `lineLength` formatting options from LSP - The LSP server no longer reads custom `printWidth` or `lineLength` properties from `textDocument/formatting` requests. Editors configured to pass these options will have them silently ignored; HTML/T-HTML formatting now uses upstream library defaults. (#25)

## What's Changed
* Add html thtml backend support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/21
* docs: add HTML and T-HTML references across all documentation by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/23
* Add backend regression tests by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/22
* Generalize installed package inference by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/25


**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/0.3.0...0.4.0

---

## [0.3.0](https://github.com/koxudaxi/t-linter/releases/tag/0.3.0) - 2026-03-19

## Breaking Changes

### Language Detection Changes
* Template language inference from annotated call parameters - Templates passed as arguments to functions or class constructors with `Annotated[Template, "lang"]` parameters now inherit the annotated language. This applies to both local and imported callables (when Python source or stubs can be resolved). Previously unlinted templates may now produce diagnostics (e.g., YAML validation errors), which could cause CI pipelines to fail. (#20)

### Default Behavior Changes
* Linting and formatting now use file-path context for import resolution - `lint_source` and the CLI formatter now resolve the file's directory to follow imports and infer template languages from external callable signatures. This means linting/formatting results may differ from previous runs even on unchanged files if imported modules contain annotated parameters. (#20)

## What's Changed
* Infer template languages from annotated call parameters by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/20


**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/0.2.1...0.3.0

---

## [0.2.1](https://github.com/koxudaxi/t-linter/releases/tag/0.2.1) - 2026-03-18

## What's Changed
* Fix serverPath description: cargo -> pip by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/16
* Fix --version to use Cargo.toml version instead of hardcoded 0.1.0 by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/17
* Add CLI format subcommand by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/18
* Use structured-data backend updates for YAML validation by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/19


**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/0.2.0...0.2.1

---

## [0.2.0](https://github.com/koxudaxi/t-linter/releases/tag/0.2.0) - 2026-03-18

## What's Changed
* Update dependencies by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/1
* Add YAML and TOML support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/2
* Add check command by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/3
* Add structured-data backend integration for check and format by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/5
* Add automated release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/6
* Unify Python and VSCode release tags by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/7
* Add documentation site with Zensical + Cloudflare Pages by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/8
* Handle legacy VSCode tags in release draft versioning by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/9
* Add documentation site link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/10
* Add maintainer open-to-work link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/11
* Replace release-draft with Claude Code Action based workflow by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/12
* Fix release workflows: upgrade Node.js 18 to 20, add verbose PyPI logging by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/13
* Remove v-prefix tag guards from release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/14
* Fix sdist missing LICENSE and VSCode engine version mismatch by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/15

## New Contributors
* @koxudaxi made their first contribution in https://github.com/koxudaxi/t-linter/pull/1

**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/v0.1.0...0.2.0

---

## [0.2.0](https://github.com/koxudaxi/t-linter/releases/tag/0.2.0) - 2026-03-18

## What's Changed
* Update dependencies by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/1
* Add YAML and TOML support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/2
* Add check command by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/3
* Add structured-data backend integration for check and format by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/5
* Add automated release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/6
* Unify Python and VSCode release tags by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/7
* Add documentation site with Zensical + Cloudflare Pages by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/8
* Handle legacy VSCode tags in release draft versioning by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/9
* Add documentation site link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/10
* Add maintainer open-to-work link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/11
* Replace release-draft with Claude Code Action based workflow by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/12
* Fix release workflows: upgrade Node.js 18 to 20, add verbose PyPI logging by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/13
* Remove v-prefix tag guards from release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/14

## New Contributors
* @koxudaxi made their first contribution in https://github.com/koxudaxi/t-linter/pull/1

**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/v0.1.0...0.2.0

---

## [0.2.0](https://github.com/koxudaxi/t-linter/releases/tag/0.2.0) - 2026-03-18

## What's Changed
* Update dependencies by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/1
* Add YAML and TOML support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/2
* Add check command by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/3
* Add structured-data backend integration for check and format by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/5
* Add automated release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/6
* Unify Python and VSCode release tags by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/7
* Add documentation site with Zensical + Cloudflare Pages by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/8
* Handle legacy VSCode tags in release draft versioning by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/9
* Add documentation site link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/10
* Add maintainer open-to-work link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/11
* Replace release-draft with Claude Code Action based workflow by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/12
* Fix release workflows: upgrade Node.js 18 to 20, add verbose PyPI logging by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/13

## New Contributors
* @koxudaxi made their first contribution in https://github.com/koxudaxi/t-linter/pull/1

**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/vscode-v0.1.3...0.2.0

---

## [0.2.0](https://github.com/koxudaxi/t-linter/releases/tag/0.2.0) - 2026-03-18

## What's Changed
* Update dependencies by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/1
* Add YAML and TOML support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/2
* Add check command by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/3
* Add structured-data backend integration for check and format by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/5
* Add automated release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/6
* Unify Python and VSCode release tags by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/7
* Add documentation site with Zensical + Cloudflare Pages by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/8
* Handle legacy VSCode tags in release draft versioning by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/9
* Add documentation site link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/10
* Add maintainer open-to-work link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/11
* Replace release-draft with Claude Code Action based workflow by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/12
* Fix release workflows: upgrade Node.js 18 to 20, add verbose PyPI logging by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/13

## New Contributors
* @koxudaxi made their first contribution in https://github.com/koxudaxi/t-linter/pull/1

**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/v0.1.0...0.2.0

---

## [v0.2.0](https://github.com/koxudaxi/t-linter/releases/tag/v0.2.0) - 2026-03-18

## What's Changed
* Update dependencies by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/1
* Add YAML and TOML support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/2
* Add check command by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/3
* Add structured-data backend integration for check and format by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/5
* Add automated release workflows by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/6
* Unify Python and VSCode release tags by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/7
* Add documentation site with Zensical + Cloudflare Pages by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/8
* Handle legacy VSCode tags in release draft versioning by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/9
* Add documentation site link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/10
* Add maintainer open-to-work link to README by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/11
* Replace release-draft with Claude Code Action based workflow by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/12

## New Contributors
* @koxudaxi made their first contribution in https://github.com/koxudaxi/t-linter/pull/1

**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/vscode-v0.1.3...0.2.0

---

