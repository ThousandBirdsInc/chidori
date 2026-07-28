'use client';

/**
 * The docs VM's filesystem: a tiny in-memory Unix-ish tree shared by the
 * fake-Linux terminal, the in-browser `chidori` CLI, and the runner's
 * `chidori.workspace.*` shim — a file an example writes is the same file
 * `cat` prints in the terminal.
 *
 * Contents = build-time seeds (public/vm-seed.json: the repo's example
 * agents plus agent files reconstructed from the docs pages) + the reader's
 * own writes, which persist for the tab in sessionStorage as an overlay so
 * navigating between docs pages doesn't lose their work.
 */

export interface VfsFile {
  kind: 'file';
  content: string;
  mtime: number;
}

export interface VfsDir {
  kind: 'dir';
  mtime: number;
}

export type VfsNode = VfsFile | VfsDir;

const OVERLAY_STORAGE = 'chidori-docs-vm-fs-overlay';

export const HOME = '/home/user';
export const PROJECT = `${HOME}/project`;

/** Collapse `.`/`..`, force absolute. `base` must be absolute. */
export function resolvePath(base: string, path: string): string {
  let raw = path.startsWith('/') ? path : path === '~' || path.startsWith('~/') ? HOME + path.slice(1) : `${base}/${path}`;
  const parts: string[] = [];
  for (const seg of raw.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') parts.pop();
    else parts.push(seg);
  }
  return `/${parts.join('/')}`;
}

export class Vfs {
  private nodes = new Map<string, VfsNode>();
  private overlay = new Map<string, VfsNode | null>();

  constructor(seeds: Record<string, string>) {
    this.mkdirp(HOME);
    this.mkdirp(PROJECT);
    for (const [rel, content] of Object.entries(seeds)) {
      this.seedFile(rel.startsWith('/') ? rel : `${PROJECT}/${rel}`, content);
    }
    this.loadOverlay();
  }

  private loadOverlay(): void {
    try {
      const raw = sessionStorage.getItem(OVERLAY_STORAGE);
      if (!raw) return;
      for (const [path, node] of Object.entries(JSON.parse(raw) as Record<string, VfsNode | null>)) {
        this.overlay.set(path, node);
        if (node === null) this.nodes.delete(path);
        else {
          this.nodes.set(path, node);
          if (node.kind === 'file') this.mkdirp(dirname(path));
        }
      }
    } catch {
      /* no persistence — still a working fs */
    }
  }

  private saveOverlay(): void {
    try {
      sessionStorage.setItem(OVERLAY_STORAGE, JSON.stringify(Object.fromEntries(this.overlay)));
    } catch {
      /* storage full/blocked — keep going in memory */
    }
  }

  private seedFile(path: string, content: string): void {
    this.mkdirp(dirname(path));
    this.nodes.set(path, { kind: 'file', content, mtime: 0 });
  }

  mkdirp(path: string): void {
    const parts = path.split('/').filter(Boolean);
    let cur = '';
    for (const part of parts) {
      cur += `/${part}`;
      if (!this.nodes.has(cur)) this.nodes.set(cur, { kind: 'dir', mtime: Date.now() });
      else if (this.nodes.get(cur)!.kind === 'file') throw new Error(`not a directory: ${cur}`);
    }
  }

  stat(path: string): VfsNode | null {
    if (path === '/' || path === '') return { kind: 'dir', mtime: 0 };
    return this.nodes.get(path) ?? null;
  }

  isDir(path: string): boolean {
    return this.stat(path)?.kind === 'dir';
  }

  read(path: string): string | null {
    const node = this.nodes.get(path);
    return node?.kind === 'file' ? node.content : null;
  }

  write(path: string, content: string): void {
    const parent = dirname(path);
    this.mkdirp(parent);
    const node: VfsFile = { kind: 'file', content, mtime: Date.now() };
    this.nodes.set(path, node);
    this.overlay.set(path, node);
    this.saveOverlay();
  }

  delete(path: string): boolean {
    const node = this.nodes.get(path);
    if (!node) return false;
    for (const key of [...this.nodes.keys()]) {
      if (key === path || key.startsWith(`${path}/`)) {
        this.nodes.delete(key);
        this.overlay.set(key, null);
      }
    }
    this.saveOverlay();
    return true;
  }

  /** Direct children names of a directory. */
  list(path: string): { name: string; node: VfsNode }[] {
    const prefix = path === '/' ? '/' : `${path}/`;
    const out: { name: string; node: VfsNode }[] = [];
    for (const [key, node] of this.nodes) {
      if (!key.startsWith(prefix) || key === path) continue;
      const rest = key.slice(prefix.length);
      if (!rest || rest.includes('/')) continue;
      out.push({ name: rest, node });
    }
    return out.sort((a, b) => a.name.localeCompare(b.name));
  }

  /** Every file path under a directory (recursive). */
  walk(path: string): string[] {
    const prefix = path === '/' ? '/' : `${path}/`;
    const out: string[] = [];
    for (const [key, node] of this.nodes) {
      if (node.kind === 'file' && key.startsWith(prefix)) out.push(key);
    }
    return out.sort();
  }
}

export function dirname(path: string): string {
  const i = path.lastIndexOf('/');
  return i <= 0 ? '/' : path.slice(0, i);
}

export function basename(path: string): string {
  return path.slice(path.lastIndexOf('/') + 1);
}

// ---------------------------------------------------------------------------
// The page-wide VFS singleton, seeded from public/vm-seed.json.

const BASE = process.env.NEXT_PUBLIC_BASE_PATH ?? '';

let vfsPromise: Promise<Vfs> | null = null;

export function loadVfs(): Promise<Vfs> {
  vfsPromise ??= fetch(`${BASE}/vm-seed.json`)
    .then((res) => (res.ok ? res.json() : { files: {} }))
    .then((json: { files?: Record<string, string> }) => new Vfs(json.files ?? {}))
    .catch(() => new Vfs({}));
  return vfsPromise;
}
