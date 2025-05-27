# Change Log

All notable changes to the "t-linter" extension will be documented in this file.

Check [Keep a Changelog](http://keepachangelog.com/) for recommendations on how to structure this file.

## [Unreleased]

## [0.1.0] - 2025-01-28

### Added
- Initial release of t-linter VSCode extension
- Intelligent syntax highlighting for Python template strings (PEP 750)
- Support for embedded languages: HTML, SQL, JavaScript, CSS, JSON
- Type-based language detection using `Annotated[Template, "language"]` syntax
- Support for Python 3.12+ type aliases (`type html = Annotated[Template, "html"]`)
- Function parameter type inference for template language detection
- Language Server Protocol (LSP) integration
- Commands:
  - `t-linter.restart`: Restart the language server
  - `t-linter.showStats`: Show template string statistics for current file
- Configuration options:
  - `t-linter.enabled`: Enable/disable the extension
  - `t-linter.trace.server`: Control LSP trace verbosity
  - `t-linter.serverPath`: Custom path to t-linter executable
  - `t-linter.highlightUntyped`: Highlight untyped template strings
  - `t-linter.enableTypeChecking`: Enable Python type checker integration

### Known Issues
- Cross-file type resolution not yet implemented
- Limited to single-file analysis scope

### Future Enhancements
- Cross-file type definition support
- Additional embedded language support
- Real-time validation and linting
- Quick fixes and code actions