# Installation

t-linter can be installed in three ways depending on your use case.

## Option 1: VSCode Extension (Recommended for VSCode users)

**Step 1: Install the t-linter binary**

Install t-linter as a project dependency (recommended):

```bash
pip install t-linter
```

For better project isolation, add it to your project's requirements:

=== "uv (recommended)"

    ```bash
    uv add t-linter
    ```

=== "pip"

    ```bash
    echo "t-linter" >> requirements.txt
    pip install -r requirements.txt
    ```

**Step 2: Install the VSCode extension**

Install the extension from the Visual Studio Code Marketplace:

1. Open VSCode
2. Go to Extensions (Ctrl+Shift+X / Cmd+Shift+X)
3. Search for "t-linter"
4. Click Install on "T-Linter - Python Template Strings Highlighter & Linter" by koxudaxi

**[Install from VSCode Marketplace](https://marketplace.visualstudio.com/items?itemName=koxudaxi.t-linter)**

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
