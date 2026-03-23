# Changelog

All notable changes to this project are documented in this file.
This changelog is generated from GitHub Releases and may include manual corrections when release metadata needs adjustment.

---
## [0.5.0](https://github.com/koxudaxi/t-linter/releases/tag/0.5.0) - 2026-03-23

## Fixed
* Fix `check` hanging when resolving `html_tstring` imports that combine package re-exports and module imports. (#26)
* Update `tstring-html` and `tstring-thtml` to `0.1.4` as part of the `html_tstring` integration fix. (#26)

## What's Changed
* Fix check hang with html_tstring imports by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/26


**Full Changelog**: https://github.com/koxudaxi/t-linter/compare/0.4.0...0.5.0

---

## [0.4.0](https://github.com/koxudaxi/t-linter/releases/tag/0.4.0) - 2026-03-23

## Breaking Changes

### Language Detection Changes
* HTML validation backend switched from tree-sitter to `tstring-html` - HTML templates annotated with `"html"` are now validated through the `tstring-html` Rust backend instead of tree-sitter parsing. This may produce different diagnostics, messages, or source locations for existing templates. (#21)

### Default Behavior Changes
* `t-linter format` now pretty-formats HTML and T-HTML templates by default. Projects that previously saw no formatting changes for these templates may now get rewrites. (#24)
* Installed package inference was generalized, so more imported annotations can influence template language detection during `check` and `format`. Previously untyped templates may now produce diagnostics or formatting changes. (#25)

### CLI Changes
* Added `--line-length <N>` to `t-linter format` for HTML and T-HTML formatting. CLI precedence is `--line-length > pyproject.toml > default 80`. (#24)

### Configuration Changes
* Added `tool.t-linter.line-length` support in `pyproject.toml` for HTML and T-HTML formatting. (#24)

### LSP Protocol Changes
* Added HTML and T-HTML formatting line-length support through LSP custom properties, with precedence `printWidth > lineLength > pyproject.toml > default 80`. (#24)

### Rust API Changes
* `t-linter-core` added `FormatOptions` plus `format_document_with_options`, `format_document_in_file_with_options`, and `format_document_range_with_options` so HTML and T-HTML formatting can be configured explicitly. (#24)
* `project_config` remained available and now includes `line_length` loading from `pyproject.toml`. (#24)

## What's Changed
* Add html thtml backend support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/21
* Add backend regression tests by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/22
* docs: add HTML and T-HTML references across all documentation by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/23
* Add formatter line length support by @koxudaxi in https://github.com/koxudaxi/t-linter/pull/24
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
