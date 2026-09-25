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
export type TunnelState = 'not_configured' | 'stopped' | 'starting' | 'running' | 'failed';
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
  child.once('error', () => undefined);
  return child;
}

export function stopTunnel(child: ChildProcess | undefined): void {
  if (!child || child.killed) return;
  child.kill('SIGTERM');
  setTimeout(() => { if (!child.killed) child.kill('SIGKILL'); }, 2_000).unref();
}

export class TunnelController {
  private child: ChildProcess | undefined;
  private state: TunnelState;
  private error: string | undefined;
  constructor(private readonly options: TunnelOptions | undefined, private readonly spawnLike: SpawnLike = spawn) {
    this.state = options ? 'stopped' : 'not_configured';
  }
  start(): TunnelStatus {
    if (!this.options) return this.status();
    if (this.child && !this.child.killed) return this.status();
    this.error = undefined;
    this.state = 'starting';
    try {
      this.child = startTunnel(this.options, this.spawnLike);
      this.child.once('spawn', () => { this.state = 'running'; });
      this.child.once('error', (error) => { this.state = 'failed'; this.error = error.message; });
      this.child.once('exit', (code, signal) => {
        if (this.state !== 'stopped') {
          this.state = code === 0 ? 'stopped' : 'failed';
          if (code !== 0) this.error = `ssh exited (${code ?? signal ?? 'unknown'})`;
        }
        this.child = undefined;
      });
    } catch (error) {
      this.state = 'failed';
      this.error = error instanceof Error ? error.message : 'tunnel start failed';
      this.child = undefined;
    }
    return this.status();
  }
  stop(): TunnelStatus {
    if (this.child) {
      this.state = 'stopped';
      stopTunnel(this.child);
      this.child = undefined;
    }
    return this.status();
  }
  status(): TunnelStatus {
    const value: TunnelStatus = { state: this.state };
    if (this.child?.pid) value.pid = this.child.pid;
    if (this.error) value.error = this.error;
    return value;
  }
}
