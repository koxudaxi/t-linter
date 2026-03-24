const fs = require('fs');
const path = require('path');

function parseArgs(argv) {
    const args = {};

    for (let index = 0; index < argv.length; index += 1) {
        const key = argv[index];
        const value = argv[index + 1];

        if (!key.startsWith('--') || value === undefined) {
            continue;
        }

        args[key.slice(2)] = value;
        index += 1;
    }

    return args;
}

function getBinaryName(target) {
    return target.startsWith('win32-') ? 't-linter.exe' : 't-linter';
}

function main() {
    const { source, target } = parseArgs(process.argv.slice(2));

    if (!source || !target) {
        throw new Error('Usage: node scripts/prepare-binary.js --source <path> --target <vscode-target>');
    }

    if (!fs.existsSync(source)) {
        throw new Error(`Source binary does not exist: ${source}`);
    }

    const destinationDirectory = path.join(__dirname, '..', 'bin', target);
    const destinationPath = path.join(destinationDirectory, getBinaryName(target));

    fs.mkdirSync(destinationDirectory, { recursive: true });
    fs.copyFileSync(source, destinationPath);

    if (!target.startsWith('win32-')) {
        fs.chmodSync(destinationPath, 0o755);
    }

    console.log(`Staged ${source} -> ${destinationPath}`);
}

main();
