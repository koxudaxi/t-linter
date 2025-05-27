# t-linter 🐍✨

Intelligent syntax highlighting and validation for Python template strings (PEP 750).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Features

- 🎨 **Smart Syntax Highlighting** - Detects embedded languages in `t"..."` strings
- 🔍 **Type-based Detection** - Understands `Annotated[Template, "html"]` annotations
- 🚀 **Fast** - Built with Rust and Tree-sitter for optimal performance
- 🔧 **Extensible** - Support for HTML, SQL, JavaScript, CSS, and more

## Installation

Install using pip:

```bash
pip install t-linter
```

## Usage

Run the language server:

```bash
t-linter lsp
```

Check files:

```bash
t-linter check file.py
```

## Development

For development, you can also build from source:

```bash
git clone https://github.com/koxudaxi/t-linter
cd t-linter
cargo install --path crates/t-linter
```

