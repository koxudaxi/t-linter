# Installation

t-linter can be installed in three ways depending on your use case.

## Option 1: PyPI Package (Recommended)

Install t-linter for CLI usage and LSP server integration:

```bash
pip install t-linter
```

For better project isolation, add it to your project's dependencies:

=== "uv (recommended)"

    ```bash
    uv add t-linter
    ```

=== "pip"

    ```bash
    echo "t-linter" >> requirements.txt
    pip install -r requirements.txt
    ```

=== "pyproject.toml"

    ```toml
    [project]
    dependencies = [
        "t-linter",
    ]
    ```

This provides the `t-linter` command-line tool and LSP server.

**[View on PyPI](https://pypi.org/project/t-linter/)**

## pre-commit

Use the published hooks from this repository:

```yaml
repos:
  - repo: https://github.com/koxudaxi/t-linter
    rev: 0.9.1
    hooks:
      - id: t-linter
      - id: t-linter-format
```

For CI-only formatting checks, override the format hook args:

```yaml
      - id: t-linter-format
        args: [--check]
```

## Option 2: VSCode Extension

If you use VSCode, install the extension for seamless editor integration:

The extension bundles `t-linter` binaries for Linux x64, macOS x64/arm64, and Windows x64, so those platforms do not need a separate CLI installation.

**Step 1: Install the VSCode extension**

Install the extension from the Visual Studio Code Marketplace:

1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X / Cmd+Shift+X)
3. Search for "t-linter"
4. Click Install on "T-Linter - Python Template Strings Highlighter & Linter" by koxudaxi

**[Install from VSCode Marketplace](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)**

### Optional: Use a custom t-linter binary

If you want to override the bundled binary, or if you are on an unsupported platform, install `t-linter` separately and set `t-linter.serverPath`:

- `uv`: `uv add t-linter`
- `pip`: `pip install t-linter`

## Option 3: Build from Source

For development or bleeding-edge features:

```bash
git clone https://github.com/koxudaxi/t-linter
cd t-linter
cargo install --path crates/t-linter
```

!!! note
    Building from source requires the [Rust toolchain](https://www.rust-lang.org/tools/install).
