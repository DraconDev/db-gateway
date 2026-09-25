import { spawn, type ChildProcess } from 'node:child_process';

export interface TunnelOptions {
  target: string;
  localPort: number;
  remoteHost?: string;
  remotePort?: number;
  identityFile?: string;
  command?: string;
}
export interface TunnelProcess { command: string; args: string[] }
export type SpawnLike = (command: string, args: string[], options: { stdio: ['ignore', 'ignore', 'pipe'] }) => ChildProcess;
export type TunnelState = 'not_configured' | 'stopped' | 'starting' | 'running' | 'stopping' | 'failed';
export interface TunnelStatus { state: TunnelState; pid?: number; error?: string }

export function tunnelCommand(options: TunnelOptions): TunnelProcess {
  if (!/^[a-z0-9._-]+@[a-z0-9.-]+$/.test(options.target)) throw new Error('Tunnel target must be user@host');
  if (!Number.isInteger(options.localPort) || options.localPort < 1024 || options.localPort > 65535) throw new Error('Tunnel local port must be 1024-65535');
  const remoteHost = options.remoteHost ?? '127.0.0.1';
  if (!/^[a-z0-9.-]+$/.test(remoteHost)) throw new Error('Invalid tunnel remote host');
  const remotePort = options.remotePort ?? 7432;
  if (!Number.isInteger(remotePort) || remotePort < 1 || remotePort > 65535) throw new Error('Invalid tunnel remote port');
  const command = options.command ?? 'ssh';
  const args = ['-N', '-T', '-o', 'ExitOnForwardFailure=yes', '-L', `127.0.0.1:${options.localPort}:${remoteHost}:${remotePort}`];
  if (options.identityFile) args.push('-i', options.identityFile);
  args.push(options.target);
  return { command, args };
}

export function startTunnel(options: TunnelOptions, spawnLike: SpawnLike = spawn): ChildProcess {
  const process = tunnelCommand(options);
  const child = spawnLike(process.command, process.args, { stdio: ['ignore', 'ignore', 'pipe'] });
  // Prevent an unhandled EventEmitter error if a caller uses startTunnel
  // directly. TunnelController installs its own listener immediately after.
  child.once('error', () => undefined);
  return child;
}

function isRunning(child: ChildProcess | undefined): boolean {
  return Boolean(child && child.exitCode === null && child.signalCode === null);
}

export async function stopTunnel(child: ChildProcess | undefined, graceMs = 2_000): Promise<void> {
  if (!child || !isRunning(child)) return;
  await new Promise<void>((resolve, reject) => {
    let settled = false;
    let killTimer: ReturnType<typeof setTimeout> | undefined;
    let deadlineTimer: ReturnType<typeof setTimeout> | undefined;
    const finish = (error?: Error) => {
      if (settled) return;
      settled = true;
      if (killTimer) clearTimeout(killTimer);
      if (deadlineTimer) clearTimeout(deadlineTimer);
      if (error) reject(error); else resolve();
    };
    child.once('exit', () => finish());
    child.once('error', (error) => finish(error instanceof Error ? error : new Error('tunnel process error')));
    try { child.kill('SIGTERM'); } catch (error) { finish(error instanceof Error ? error : new Error('tunnel kill failed')); return; }
    killTimer = setTimeout(() => {
      if (isRunning(child)) {
        try { child.kill('SIGKILL'); } catch (error) { finish(error instanceof Error ? error : new Error('tunnel kill failed')); }
      }
    }, graceMs);
    deadlineTimer = setTimeout(() => finish(new Error('tunnel did not exit after SIGKILL')), graceMs + 1_000);
    killTimer.unref();
    deadlineTimer.unref();
  });
}

export class TunnelController {
  private child: ChildProcess | undefined;
  private state: TunnelState;
  private error: string | undefined;
  private stopping: Promise<TunnelStatus> | undefined;

  constructor(private readonly options: TunnelOptions | undefined, private readonly spawnLike: SpawnLike = spawn) {
    this.state = options ? 'stopped' : 'not_configured';
  }

  async start(): Promise<TunnelStatus> {
    if (!this.options) return this.status();
    if (this.stopping) { await this.stopping; return this.start(); }
    if (this.child) {
      if (isRunning(this.child)) return this.status();
      this.child = undefined;
    }
    this.error = undefined;
    this.state = 'starting';
    try {
      const child = startTunnel(this.options, this.spawnLike);
      this.child = child;
      child.once('spawn', () => { if (this.child === child) this.state = 'running'; });
      child.once('error', (error) => {
        if (this.child === child) { this.state = 'failed'; this.error = error.message; }
      });
      child.once('exit', (code, signal) => {
        if (this.child !== child) return;
        this.child = undefined;
        if (this.state !== 'stopping') this.state = code === 0 ? 'stopped' : 'failed';
        if (code !== 0 && this.state === 'failed') this.error = `ssh exited (${code ?? signal ?? 'unknown'})`;
      });
    } catch (error) {
      this.state = 'failed';
      this.error = error instanceof Error ? error.message : 'tunnel start failed';
      this.child = undefined;
    }
    return this.status();
  }

  async stop(): Promise<TunnelStatus> {
    if (this.stopping) return this.stopping;
    const child = this.child;
    if (!child || !isRunning(child)) {
      this.child = undefined;
      this.state = this.options ? 'stopped' : 'not_configured';
      return this.status();
    }
    this.state = 'stopping';
    this.stopping = (async () => {
      try {
        await stopTunnel(child);
        if (this.child === child) this.child = undefined;
        if (this.state === 'stopping') this.state = 'stopped';
        return this.status();
      } catch (error) {
        this.state = 'failed';
        this.error = error instanceof Error ? error.message : 'tunnel stop failed';
        return this.status();
      }
    })().finally(() => { this.stopping = undefined; });
    return this.stopping;
  }

  status(): TunnelStatus {
    const value: TunnelStatus = { state: this.state };
    if (this.child?.pid) value.pid = this.child.pid;
    if (this.error) value.error = this.error;
    return value;
  }
}
