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

    const possiblePaths = [
        path.join(context.extensionPath, 'server', 't-linter'),
        path.join(context.extensionPath, 'bin', 't-linter'),

        't-linter',
        path.join(os.homedir(), '.cargo', 'bin', 't-linter'),
        path.join(os.homedir(), '.local', 'bin', 't-linter'),
        '/usr/local/bin/t-linter',
        '/usr/bin/t-linter'
    ];

    if (process.platform === 'win32') {
        possiblePaths.push(
            path.join(context.extensionPath, 'server', 't-linter.exe'),
            path.join(context.extensionPath, 'bin', 't-linter.exe'),
            't-linter.exe',
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
        const choice = await vscode.window.showErrorMessage(
            't-linter binary not found. Please install it first.',
            'Install Instructions',
            'Set Path'
        );

        if (choice === 'Install Instructions') {
            vscode.env.openExternal(vscode.Uri.parse('https://github.com/koxudaxi/t-linter#installation'));
        } else if (choice === 'Set Path') {
            vscode.commands.executeCommand('workbench.action.openSettings', 't-linter.serverPath');
        }
    }

    return pathResult;
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
                const path = stdout.trim().split('\n')[0];
                resolve(path);
            } else {
                resolve(undefined);
            }
        });
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