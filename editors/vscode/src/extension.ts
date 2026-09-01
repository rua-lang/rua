import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import {
    LanguageClient,
    LanguageClientOptions,
    RevealOutputChannelOn,
    ServerOptions,
    TransportKind,
} from 'vscode-languageclient/node';

/**
 * The extension is two independent halves.
 *
 * The grammar in `syntaxes/` is contributed declaratively and needs nothing
 * from this file; highlighting works with no `rua-lsp` anywhere on the
 * machine. Everything here is the other half: it starts the server when it can
 * find one, and says plainly why it did not when it cannot. A missing server
 * is an ordinary state, not an error - the binary is built separately from the
 * editor - so it is reported once, in the output channel, without a modal.
 */

let client: LanguageClient | undefined;
let log: vscode.OutputChannel;

/** Warn about a missing binary once per path, so editing does not nag. */
const warned = new Set<string>();

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    log = vscode.window.createOutputChannel('rua Language Server');
    context.subscriptions.push(log);

    context.subscriptions.push(
        vscode.commands.registerCommand('rua.showServerLog', () => log.show(true)),
        vscode.commands.registerCommand('rua.restartServer', async () => {
            warned.clear();
            await stopClient();
            await startClient(context);
        }),
    );

    // A change to how the server is launched only takes effect on a restart,
    // so do the restart rather than leaving the setting looking ignored.
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(async (e) => {
            if (
                e.affectsConfiguration('rua.server.enabled') ||
                e.affectsConfiguration('rua.server.path') ||
                e.affectsConfiguration('rua.server.args') ||
                e.affectsConfiguration('rua.server.env')
            ) {
                warned.clear();
                await stopClient();
                await startClient(context);
            }
        }),
    );

    await startClient(context);
}

export async function deactivate(): Promise<void> {
    await stopClient();
}

async function startClient(context: vscode.ExtensionContext): Promise<void> {
    const config = vscode.workspace.getConfiguration('rua');

    if (!config.get<boolean>('server.enabled', true)) {
        log.appendLine('rua.server.enabled is false: not starting a language server. Syntax highlighting is unaffected.');
        return;
    }

    const configured = (config.get<string>('server.path', 'rua-lsp') || 'rua-lsp').trim();
    const command = resolveServer(configured);
    if (!command) {
        reportMissing(configured);
        return;
    }

    const args = config.get<string[]>('server.args', []) ?? [];
    const env = config.get<Record<string, string>>('server.env', {}) ?? {};

    const executable = {
        command,
        args,
        transport: TransportKind.stdio,
        options: {
            env: { ...process.env, ...env },
            cwd: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
        },
    };
    const serverOptions: ServerOptions = { run: executable, debug: executable };

    // One watcher for the life of the extension: a restart reuses it rather
    // than leaving the old one behind.
    const watcher = vscode.workspace.createFileSystemWatcher('**/*.rua');
    context.subscriptions.push(watcher);

    const clientOptions: LanguageClientOptions = {
        documentSelector: [
            { scheme: 'file', language: 'rua' },
            { scheme: 'untitled', language: 'rua' },
        ],
        synchronize: { fileEvents: watcher },
        outputChannel: log,
        // The server may well die on a malformed document while it is young;
        // do not steal focus for that.
        revealOutputChannelOn: RevealOutputChannelOn.Never,
    };

    client = new LanguageClient('rua', 'rua Language Server', serverOptions, clientOptions);

    try {
        log.appendLine(`starting: ${command}${args.length ? ' ' + args.join(' ') : ''}`);
        await client.start();
        context.subscriptions.push({ dispose: () => void stopClient() });
        log.appendLine('language server ready');
    } catch (err) {
        // `start()` rejects when the process cannot be spawned, and also when
        // it exits before it finishes the handshake. Either way the editor
        // keeps the grammar, which is what most of the value is.
        client = undefined;
        const reason = err instanceof Error ? err.message : String(err);
        log.appendLine(
            `could not start the rua language server (${command}): ${reason}\n` +
                'Syntax highlighting still works; diagnostics, hover, completion and the outline need the server.\n' +
                'Build it with `cargo install --path crates/rua-lsp` (or point `rua.server.path` at the binary), then run "rua: Restart Language Server".',
        );
    }
}

async function stopClient(): Promise<void> {
    const c = client;
    client = undefined;
    if (!c) {
        return;
    }
    try {
        await c.stop();
    } catch {
        // A server that is already gone cannot be stopped politely; that is
        // not something the user needs to hear about.
    }
}

function reportMissing(configured: string): void {
    log.appendLine(
        `rua language server not found: \`${configured}\` is not on PATH and is not an executable file.\n` +
            'Syntax highlighting still works. The server adds diagnostics, hover, completion and the outline.\n' +
            'Install it with `cargo install --path crates/rua-lsp`, or set `rua.server.path` to the binary, then run "rua: Restart Language Server".',
    );
    if (warned.has(configured)) {
        return;
    }
    warned.add(configured);
    void vscode.window
        .showWarningMessage(
            `rua: could not find the language server \`${configured}\`. Highlighting works; other features need it.`,
            'Show Log',
            'Open Settings',
        )
        .then((choice) => {
            if (choice === 'Show Log') {
                log.show(true);
            } else if (choice === 'Open Settings') {
                void vscode.commands.executeCommand('workbench.action.openSettings', 'rua.server.path');
            }
        });
}

/**
 * Find the server before spawning it, so a missing binary is a sentence in the
 * log rather than an ENOENT out of the language client's innards.
 *
 * A value with a separator is taken literally (`~` expanded); a bare name is
 * looked up on PATH, with PATHEXT applied on Windows.
 */
function resolveServer(configured: string): string | undefined {
    if (!configured) {
        return undefined;
    }

    const expanded = configured.startsWith('~' + path.sep) || configured === '~'
        ? path.join(homeDir(), configured.slice(1))
        : configured;

    if (expanded.includes('/') || expanded.includes(path.sep)) {
        const abs = path.isAbsolute(expanded)
            ? expanded
            : path.resolve(vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? process.cwd(), expanded);
        return isExecutable(abs) ? abs : undefined;
    }

    const dirs = (process.env.PATH ?? '').split(path.delimiter).filter(Boolean);
    const exts = process.platform === 'win32'
        ? (process.env.PATHEXT ?? '.EXE;.CMD;.BAT;.COM').split(';').filter(Boolean)
        : [''];

    for (const dir of dirs) {
        for (const ext of exts) {
            const candidate = path.join(dir, expanded + ext);
            if (isExecutable(candidate)) {
                return candidate;
            }
        }
    }
    return undefined;
}

function isExecutable(file: string): boolean {
    try {
        const stat = fs.statSync(file);
        if (!stat.isFile()) {
            return false;
        }
        if (process.platform !== 'win32') {
            fs.accessSync(file, fs.constants.X_OK);
        }
        return true;
    } catch {
        return false;
    }
}

function homeDir(): string {
    return process.env.HOME ?? process.env.USERPROFILE ?? '';
}
