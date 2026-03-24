import * as vscode from 'vscode';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';
import { spawn } from 'child_process';
import {
    LanguageClient,
    LanguageClientOptions,
    ServerOptions,
    TransportKind,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;
let outputChannel: vscode.OutputChannel;
let extensionContext: vscode.ExtensionContext;  // Store context globally

const BUNDLED_BINARY_PATHS: Record<string, string> = {
    'darwin-arm64': path.join('bin', 'darwin-arm64', 't-linter'),
    'darwin-x64': path.join('bin', 'darwin-x64', 't-linter'),
    'linux-x64': path.join('bin', 'linux-x64', 't-linter'),
    'win32-x64': path.join('bin', 'win32-x64', 't-linter.exe'),
};

type BundledServerLookup =
    | { kind: 'found'; path: string }
    | { kind: 'unsupported-platform'; platform: string }
    | { kind: 'missing-artifact'; path: string }
    | { kind: 'not-executable'; path: string };

export async function activate(context: vscode.ExtensionContext) {
    extensionContext = context;  // Save context for later use
    outputChannel = vscode.window.createOutputChannel('T-Linter');
    context.subscriptions.push(outputChannel);

    const config = vscode.workspace.getConfiguration('t-linter');
    if (!config.get<boolean>('enabled', true)) {
        outputChannel.appendLine('T-Linter is disabled');
        return;
    }

    // Register commands
    context.subscriptions.push(
        vscode.commands.registerCommand('t-linter.restart', async () => {
            await restartServer();
        })
    );

    context.subscriptions.push(
        vscode.commands.registerCommand('t-linter.showStats', async () => {
            await showStats();
        })
    );

    // Start the language server
    try {
        await startLanguageServer(context);
    } catch (error) {
        outputChannel.appendLine(`Failed to start t-linter: ${error}`);
        vscode.window.showErrorMessage(`Failed to start t-linter: ${error}`);
    }
}

export function deactivate(): Thenable<void> | undefined {
    if (!client) {
        return undefined;
    }
    return client.stop();
}

async function startLanguageServer(context: vscode.ExtensionContext) {
    const serverPath = await findServerPath(context);
    if (!serverPath) {
        throw new Error('t-linter executable not found. Please install it or configure t-linter.serverPath');
    }

    outputChannel.appendLine(`Starting t-linter from: ${serverPath}`);

    const serverOptions: ServerOptions = {
        run: {
            command: serverPath,
            args: ['lsp'],
            transport: TransportKind.stdio
        },
        debug: {
            command: serverPath,
            args: ['lsp'],
            transport: TransportKind.stdio,
            options: {
                env: {
                    ...process.env,
                    RUST_LOG: 'debug'
                }
            }
        }
    };

    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ scheme: 'file', language: 'python' }],
        synchronize: {
            configurationSection: 't-linter',
            fileEvents: vscode.workspace.createFileSystemWatcher('**/*.py')
        },
        outputChannel,
        initializationOptions: {
            enableTypeChecking: vscode.workspace.getConfiguration('t-linter').get<boolean>('enableTypeChecking', true),
            highlightUntyped: vscode.workspace.getConfiguration('t-linter').get<boolean>('highlightUntyped', true)
        }
    };

    client = new LanguageClient(
        't-linter',
        'T-Linter Language Server',
        serverOptions,
        clientOptions
    );

    // Register middleware to handle initialization
    client.clientOptions.initializationFailedHandler = (error) => {
        outputChannel.appendLine(`Server initialization failed: ${error.message}`);
        client?.error('Server initialization failed.', error);
        return false;
    };

    // Start the client
    await client.start();
    outputChannel.appendLine('T-Linter language server started');

    // Show status bar item
    const statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    statusBarItem.text = '$(check) T-Linter';
    statusBarItem.tooltip = 'T-Linter is running';
    statusBarItem.command = 't-linter.showStats';
    statusBarItem.show();
    context.subscriptions.push(statusBarItem);
}

async function findServerPath(context: vscode.ExtensionContext): Promise<string | undefined> {
    const config = vscode.workspace.getConfiguration('t-linter');
    const configuredPath = config.get<string>('serverPath');

    if (configuredPath && fs.existsSync(configuredPath)) {
        outputChannel.appendLine(`Using configured server path: ${configuredPath}`);
        return configuredPath;
    }

    if (configuredPath) {
        outputChannel.appendLine(`Configured server path does not exist: ${configuredPath}`);
    }

    const bundledLookup = await findBundledServerPath(context);
    if (bundledLookup.kind === 'found') {
        outputChannel.appendLine(`Using bundled server path: ${bundledLookup.path}`);
        return bundledLookup.path;
    }

    const possiblePaths = [
        path.join(os.homedir(), '.cargo', 'bin', 't-linter'),
        path.join(os.homedir(), '.local', 'bin', 't-linter'),
        '/usr/local/bin/t-linter',
        '/usr/bin/t-linter'
    ];

    if (process.platform === 'win32') {
        possiblePaths.push(
            path.join(os.homedir(), '.cargo', 'bin', 't-linter.exe')
        );
    }

    for (const p of possiblePaths) {
        outputChannel.appendLine(`Checking: ${p}`);
        if (await checkExecutable(p)) {
            outputChannel.appendLine(`Found server at: ${p}`);
            return p;
        }
    }

    const pathResult = await findInPath('t-linter');
    if (pathResult) {
        outputChannel.appendLine(`Found server in PATH: ${pathResult}`);
        return pathResult;
    }

    if (!pathResult) {
        const errorMessage = bundledBinaryErrorMessage(bundledLookup);
        const choice = await vscode.window.showErrorMessage(
            errorMessage,
            'Open Settings',
            'View Installation Docs'
        );

        if (choice === 'Open Settings') {
            vscode.commands.executeCommand('workbench.action.openSettings', 't-linter.serverPath');
        } else if (choice === 'View Installation Docs') {
            vscode.env.openExternal(vscode.Uri.parse('https://t-linter.koxudaxi.dev/installation/'));
        }
    }

    return pathResult;
}

function bundledBinaryErrorMessage(lookup: BundledServerLookup): string {
    switch (lookup.kind) {
        case 'unsupported-platform':
            return `This platform (${lookup.platform}) does not include a bundled t-linter binary. Install t-linter separately and configure t-linter.serverPath.`;
        case 'missing-artifact':
        case 'not-executable':
            return 'The bundled t-linter binary is unavailable. Reinstall the extension or configure t-linter.serverPath.';
        case 'found':
            return 'Bundled t-linter binary is available.';
    }
}

async function findBundledServerPath(context: vscode.ExtensionContext): Promise<BundledServerLookup> {
    const bundledRelativePath = BUNDLED_BINARY_PATHS[`${process.platform}-${process.arch}`];
    if (!bundledRelativePath) {
        const platform = `${process.platform}-${process.arch}`;
        outputChannel.appendLine(`No bundled binary mapping for ${platform}`);
        return { kind: 'unsupported-platform', platform };
    }

    const bundledPath = path.join(context.extensionPath, bundledRelativePath);
    try {
        await fs.promises.access(bundledPath, fs.constants.F_OK);
    } catch {
        outputChannel.appendLine(`Bundled binary is missing: ${bundledPath}`);
        return { kind: 'missing-artifact', path: bundledPath };
    }

    if (process.platform !== 'win32') {
        try {
            await fs.promises.chmod(bundledPath, 0o755);
        } catch (error) {
            outputChannel.appendLine(`Failed to set executable bit on bundled binary: ${error}`);
        }
    }

    if (await checkExecutable(bundledPath)) {
        return { kind: 'found', path: bundledPath };
    }

    outputChannel.appendLine(`Bundled binary is not executable: ${bundledPath}`);
    return { kind: 'not-executable', path: bundledPath };
}

async function checkExecutable(path: string): Promise<boolean> {
    try {
        await fs.promises.access(path, fs.constants.X_OK);
        return true;
    } catch {
        return false;
    }
}

async function findInPath(executable: string): Promise<string | undefined> {
    return new Promise((resolve) => {
        const cmd = process.platform === 'win32' ? 'where' : 'which';
        const child = spawn(cmd, [executable]);

        let stdout = '';
        child.stdout.on('data', (data) => {
            stdout += data.toString();
        });

        child.on('close', (code) => {
            if (code === 0 && stdout) {
                const resolvedPath = stdout
                    .trim()
                    .split(/\r?\n/)
                    .map((line) => line.trim())
                    .find((line) => line.length > 0);
                resolve(resolvedPath);
            } else {
                resolve(undefined);
            }
        });

        child.on('error', () => resolve(undefined));
    });
}

async function restartServer() {
    outputChannel.appendLine('Restarting t-linter server...');

    if (client) {
        await client.stop();
    }

    // Use the saved extension context
    await startLanguageServer(extensionContext);
}

async function showStats() {
    if (!client) {
        vscode.window.showWarningMessage('T-Linter server is not running');
        return;
    }

    // Send custom request to server for statistics
    try {
        const stats = await client.sendRequest('t-linter/stats', {
            uri: vscode.window.activeTextEditor?.document.uri.toString()
        });

        // Show stats in output channel
        outputChannel.clear();
        outputChannel.appendLine('Template String Statistics:');
        outputChannel.appendLine(JSON.stringify(stats, null, 2));
        outputChannel.show();
    } catch (error) {
        outputChannel.appendLine(`Failed to get statistics: ${error}`);
    }
}
