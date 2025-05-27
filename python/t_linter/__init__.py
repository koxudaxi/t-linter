"""t-linter - Python template string linter for PEP 750."""

__version__ = "0.1.0"

def main():
    """Entry point for the t-linter CLI."""
    import sys
    import subprocess
    import os
    
    # Find the t-linter binary
    binary_name = "t-linter.exe" if sys.platform == "win32" else "t-linter"
    binary_path = os.path.join(os.path.dirname(__file__), binary_name)
    
    if not os.path.exists(binary_path):
        print("Error: t-linter binary not found", file=sys.stderr)
        sys.exit(1)
    
    # Run the binary with all arguments
    sys.exit(subprocess.call([binary_path] + sys.argv[1:]))

if __name__ == "__main__":
    main()