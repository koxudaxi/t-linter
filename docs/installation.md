# Installation

t-linter can be installed in three ways depending on your use case.

## Option 1: VSCode Extension (Recommended for VSCode users)

**Step 1: Install the VSCode extension**

Install the extension from the Visual Studio Code Marketplace:

1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X / Cmd+Shift+X)
3. Search for "t-linter"
4. Click Install on "T-Linter - Python Template Strings Highlighter & Linter" by koxudaxi

The extension bundles `t-linter` binaries for Linux x64, macOS x64/arm64, and Windows x64, so those platforms do not need a separate CLI installation.

**[Install from VSCode Marketplace](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)**

### Optional: Use a custom t-linter binary

If you want to override the bundled binary, or if you are on an unsupported platform, install `t-linter` separately and set `t-linter.serverPath`:

=== "uv"

    ```bash
    uv add t-linter
    ```

=== "pip"

    ```bash
    pip install t-linter
    ```

## Option 2: PyPI Package Only (CLI tool and LSP server)

For command-line usage or integration with other editors:

```bash
pip install t-linter
```

Or add to your project's dependencies:

=== "uv"

    ```bash
    uv add t-linter
    ```

=== "pip"

    ```bash
    pip install t-linter
    ```

=== "pyproject.toml"

    ```toml
    [project]
    dependencies = [
        "t-linter",
    ]
    ```

This provides the `t-linter` command-line tool and LSP server without the VSCode extension.

**[View on PyPI](https://pypi.org/project/t-linter/)**

## Option 3: Build from Source

For development or bleeding-edge features:

```bash
git clone https://github.com/koxudaxi/t-linter
cd t-linter
cargo install --path crates/t-linter
```

!!! note
    Building from source requires the [Rust toolchain](https://www.rust-lang.org/tools/install).
