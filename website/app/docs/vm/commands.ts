'use client';

/**
 * The docs VM's command set, minus the `chidori` CLI itself (chidori-cli.ts):
 * the coreutils the docs' shell blocks use, plus believable stand-ins for
 * the heavyweight tools a real machine would have (cargo, npm, git, fly,
 * tael) — those print honest "simulated by the docs VM" output rather than
 * pretending to do work they can't.
 */

import type { CommandIo, ShellCommand, ShellContext } from './shell';
import { HOME, basename, resolvePath } from './vfs';

const fmtDate = (ms: number) =>
  new Date(ms || Date.parse('2026-01-01T00:00:00Z')).toISOString().slice(0, 16).replace('T', ' ');

function flagsOf(argv: string[]): { flags: Set<string>; args: string[] } {
  const flags = new Set<string>();
  const args: string[] = [];
  for (const a of argv.slice(1)) {
    if (/^-[a-zA-Z]+$/.test(a)) for (const c of a.slice(1)) flags.add(c);
    else args.push(a);
  }
  return { flags, args };
}

const simulated = (name: string, lines: string[]): ShellCommand => (argv, io) => {
  io.out(lines.join('\n') + '\n');
  void argv;
  return 0;
};

export function makeCoreCommands(): Record<string, ShellCommand> {
  const commands: Record<string, ShellCommand> = {};

  commands.pwd = (_argv, io, ctx) => {
    io.out(`${ctx.cwd}\n`);
    return 0;
  };

  commands.cd = (argv, io, ctx) => {
    const target = argv[1] ? resolvePath(ctx.cwd, argv[1]) : HOME;
    if (!ctx.vfs.isDir(target)) {
      io.err(`cd: ${argv[1] ?? target}: No such file or directory\n`);
      return 1;
    }
    ctx.cwd = target;
    return 0;
  };

  commands.ls = (argv, io, ctx) => {
    const { flags, args } = flagsOf(argv);
    const target = resolvePath(ctx.cwd, args[0] ?? '.');
    const node = ctx.vfs.stat(target);
    if (!node) {
      io.err(`ls: cannot access '${args[0] ?? target}': No such file or directory\n`);
      return 2;
    }
    if (node.kind === 'file') {
      io.out(`${args[0] ?? basename(target)}\n`);
      return 0;
    }
    let entries = ctx.vfs.list(target).filter((e) => flags.has('a') || !e.name.startsWith('.'));
    if (flags.has('t')) entries = entries.sort((a, b) => b.node.mtime - a.node.mtime);
    if (flags.has('l')) {
      for (const e of entries) {
        const isDir = e.node.kind === 'dir';
        const size = e.node.kind === 'file' ? e.node.content.length : 4096;
        io.out(`${isDir ? 'drwxr-xr-x' : '-rw-r--r--'} 1 user user ${String(size).padStart(8)} ${fmtDate(e.node.mtime)} ${e.name}${isDir ? '/' : ''}\n`);
      }
    } else if (entries.length) {
      io.out(entries.map((e) => e.name).join(flags.has('1') || flags.has('t') ? '\n' : '  ') + '\n');
    }
    return 0;
  };

  commands.cat = (argv, io, ctx) => {
    const { args } = flagsOf(argv);
    if (args.length === 0) {
      io.out(io.stdin);
      return 0;
    }
    let code = 0;
    for (const arg of args) {
      const content = ctx.vfs.read(resolvePath(ctx.cwd, arg));
      if (content === null) {
        io.err(`cat: ${arg}: No such file or directory\n`);
        code = 1;
      } else {
        io.out(content.endsWith('\n') || content === '' ? content : `${content}\n`);
      }
    }
    return code;
  };

  commands.head = (argv, io, ctx) => {
    let count = 10;
    const args: string[] = [];
    for (let i = 1; i < argv.length; i++) {
      if (argv[i] === '-n') count = Number(argv[++i] ?? 10);
      else if (/^-\d+$/.test(argv[i])) count = Number(argv[i].slice(1));
      else args.push(argv[i]);
    }
    const text = args.length ? (ctx.vfs.read(resolvePath(ctx.cwd, args[0])) ?? '') : io.stdin;
    const lines = text.split('\n');
    io.out(lines.slice(0, count).join('\n') + (lines.length > 1 ? '\n' : ''));
    return 0;
  };

  commands.tail = (argv, io, ctx) => {
    let count = 10;
    const args: string[] = [];
    for (let i = 1; i < argv.length; i++) {
      if (argv[i] === '-n') count = Number(argv[++i] ?? 10);
      else if (/^-\d+$/.test(argv[i])) count = Number(argv[i].slice(1));
      else args.push(argv[i]);
    }
    const text = args.length ? (ctx.vfs.read(resolvePath(ctx.cwd, args[0])) ?? '') : io.stdin;
    const lines = text.replace(/\n$/, '').split('\n');
    io.out(lines.slice(-count).join('\n') + '\n');
    return 0;
  };

  commands.echo = (argv, io) => {
    const args = argv[1] === '-n' ? argv.slice(2) : argv.slice(1);
    io.out(args.join(' ') + (argv[1] === '-n' ? '' : '\n'));
    return 0;
  };

  commands.printf = (argv, io) => {
    // The docs use printf for simple "%s\n" formatting — cover that.
    const fmt = (argv[1] ?? '').replace(/\\n/g, '\n').replace(/\\t/g, '\t');
    let i = 2;
    io.out(fmt.replace(/%[sd]/g, () => argv[i++] ?? ''));
    return 0;
  };

  commands.mkdir = (argv, _io, ctx) => {
    const { args } = flagsOf(argv);
    for (const arg of args) ctx.vfs.mkdirp(resolvePath(ctx.cwd, arg));
    return 0;
  };

  commands.touch = (argv, _io, ctx) => {
    const { args } = flagsOf(argv);
    for (const arg of args) {
      const abs = resolvePath(ctx.cwd, arg);
      if (!ctx.vfs.stat(abs)) ctx.vfs.write(abs, '');
    }
    return 0;
  };

  commands.rm = (argv, io, ctx) => {
    const { flags, args } = flagsOf(argv);
    let code = 0;
    for (const arg of args) {
      const abs = resolvePath(ctx.cwd, arg);
      const node = ctx.vfs.stat(abs);
      if (!node) {
        if (!flags.has('f')) {
          io.err(`rm: cannot remove '${arg}': No such file or directory\n`);
          code = 1;
        }
      } else if (node.kind === 'dir' && !flags.has('r')) {
        io.err(`rm: cannot remove '${arg}': Is a directory\n`);
        code = 1;
      } else ctx.vfs.delete(abs);
    }
    return code;
  };

  commands.cp = (argv, io, ctx) => {
    const { args } = flagsOf(argv);
    const [src, dst] = [resolvePath(ctx.cwd, args[0] ?? ''), resolvePath(ctx.cwd, args[1] ?? '')];
    const content = ctx.vfs.read(src);
    if (content === null) {
      io.err(`cp: cannot stat '${args[0]}': No such file or directory\n`);
      return 1;
    }
    ctx.vfs.write(ctx.vfs.isDir(dst) ? `${dst}/${basename(src)}` : dst, content);
    return 0;
  };

  commands.mv = (argv, io, ctx) => {
    const code = commands.cp(argv, io, ctx);
    if (code === 0) ctx.vfs.delete(resolvePath(ctx.cwd, flagsOf(argv).args[0]));
    return code;
  };

  commands.grep = (argv, io, ctx) => {
    const { flags, args } = flagsOf(argv);
    if (!args.length) return 2;
    const pattern = args[0];
    let re: RegExp;
    try {
      re = new RegExp(flags.has('E') ? pattern : pattern.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), flags.has('i') ? 'i' : '');
    } catch {
      io.err(`grep: invalid pattern\n`);
      return 2;
    }
    const sources = args.length > 1 ? args.slice(1).map((f) => [f, ctx.vfs.read(resolvePath(ctx.cwd, f))] as const) : ([['(stdin)', io.stdin]] as const);
    let hit = false;
    for (const [name, content] of sources) {
      if (content === null) {
        io.err(`grep: ${name}: No such file or directory\n`);
        continue;
      }
      for (const line of content.replace(/\n$/, '').split('\n')) {
        const match = re.test(line);
        if (flags.has('v') ? !match : match) {
          hit = true;
          io.out(`${args.length > 2 ? `${name}:` : ''}${line}\n`);
        }
      }
    }
    return hit ? 0 : 1;
  };

  commands.sed = (argv, io, ctx) => {
    // Enough sed for docs one-liners: sed 's/a/b/[g]' [file], -i in place.
    const { flags, args } = flagsOf(argv);
    const script = args[0] ?? '';
    const m = script.match(/^s([/|#])(.*?)\1(.*?)\1(g?)$/);
    const file = args[1] ? resolvePath(ctx.cwd, args[1]) : null;
    const text = file ? (ctx.vfs.read(file) ?? '') : io.stdin;
    if (!m) {
      io.out(text);
      return 0;
    }
    const out = text.replace(new RegExp(m[2], m[4] ? 'g' : ''), m[3]);
    if (flags.has('i') && file) ctx.vfs.write(file, out);
    else io.out(out);
    return 0;
  };

  commands.wc = (argv, io, ctx) => {
    const { flags, args } = flagsOf(argv);
    const text = args.length ? (ctx.vfs.read(resolvePath(ctx.cwd, args[0])) ?? '') : io.stdin;
    const lines = (text.match(/\n/g) ?? []).length;
    if (flags.has('l')) io.out(`${lines}\n`);
    else io.out(`${lines} ${text.split(/\s+/).filter(Boolean).length} ${text.length}${args[0] ? ` ${args[0]}` : ''}\n`);
    return 0;
  };

  commands.tree = (argv, io, ctx) => {
    const { args } = flagsOf(argv);
    const root = resolvePath(ctx.cwd, args[0] ?? '.');
    const walk = (dir: string, prefix: string) => {
      const entries = ctx.vfs.list(dir).filter((e) => !e.name.startsWith('.'));
      entries.forEach((e, i) => {
        const last = i === entries.length - 1;
        io.out(`${prefix}${last ? '└── ' : '├── '}${e.name}\n`);
        if (e.node.kind === 'dir') walk(`${dir}/${e.name}`, `${prefix}${last ? '    ' : '│   '}`);
      });
    };
    io.out(`${args[0] ?? '.'}\n`);
    walk(root, '');
    return 0;
  };

  commands.export = (argv, _io, ctx) => {
    for (const arg of argv.slice(1)) {
      const eq = arg.indexOf('=');
      if (eq > 0) ctx.env.set(arg.slice(0, eq), arg.slice(eq + 1));
    }
    return 0;
  };

  commands.unset = (argv, _io, ctx) => {
    for (const arg of argv.slice(1)) ctx.env.delete(arg);
    return 0;
  };

  commands.env = (_argv, io, ctx) => {
    for (const [k, v] of [...ctx.env].sort((a, b) => a[0].localeCompare(b[0]))) {
      if (k !== '?') io.out(`${k}=${v}\n`);
    }
    return 0;
  };

  commands.which = (argv, io, ctx) => {
    const name = argv[1] ?? '';
    if (ctx.commands[name]) {
      io.out(`/usr/bin/${name}\n`);
      return 0;
    }
    return 1;
  };

  commands.whoami = (_argv, io) => {
    io.out('user\n');
    return 0;
  };

  commands.id = (_argv, io) => {
    io.out('uid=1000(user) gid=1000(user) groups=1000(user)\n');
    return 0;
  };

  commands.uname = (argv, io) => {
    io.out(argv.includes('-a') ? 'Linux chidori-docs-vm 6.6.0-docs #1 SMP wasm32 GNU/Linux\n' : 'Linux\n');
    return 0;
  };

  commands.hostname = (_argv, io) => {
    io.out('chidori-docs-vm\n');
    return 0;
  };

  commands.date = (_argv, io) => {
    io.out(`${new Date().toString()}\n`);
    return 0;
  };

  commands.sleep = async (argv) => {
    // Real seconds capped: nobody wants a docs page to actually block.
    const s = Math.min(Number(argv[1] ?? 0) || 0, 2);
    await new Promise((r) => setTimeout(r, s * 1000));
    return 0;
  };

  commands.true = () => 0;
  commands.false = () => 1;
  commands.kill = (argv, io) => {
    io.out(`(docs vm) kill ${argv.slice(1).join(' ')}: background jobs here stop when their command ends\n`);
    return 0;
  };

  commands.sh = (argv, io) => {
    // `curl … | sh` — the docs' install one-liner. The binary is built in.
    if (io.stdin.includes('install') || io.stdin.length > 0) {
      io.out('chidori is already installed in the docs VM: /usr/bin/chidori\n');
      return 0;
    }
    io.err('sh: interactive subshells are not supported in the docs VM\n');
    void argv;
    return 1;
  };
  commands.bash = commands.sh;

  commands.clear = (_argv, io) => {
    io.out('\x0c'); // form feed — the terminal component clears on it
    return 0;
  };

  commands.help = (_argv, io, ctx) => {
    io.out(
      'This is a simulated Linux shell running in your browser (no server).\n' +
        'The chidori CLI is real: `chidori run <agent.ts>` executes the file on the\n' +
        'wasm build of the chidori engine, journaling every host call.\n\n' +
        `Available commands: ${Object.keys(ctx.commands).sort().join(', ')}\n`,
    );
    return 0;
  };

  // ---- believable stand-ins for tools the docs mention -------------------

  commands.cargo = (argv, io) => {
    const sub = argv[1] ?? '';
    if (sub === 'build') {
      io.out('   Compiling chidori v0.4.0 (/home/user/project)\n    Finished `release` profile [optimized] target(s) in 2m 41s (simulated by the docs VM)\n');
      return 0;
    }
    if (sub === 'fmt' || sub === 'clippy' || sub === 'test' || sub === 'check') {
      io.out(`    Finished cargo ${sub} — clean (simulated by the docs VM)\n`);
      return 0;
    }
    io.out(`cargo ${sub}: simulated by the docs VM\n`);
    return 0;
  };

  commands.npm = simulated('npm', ['(docs vm) npm is simulated here — chidori itself manages packages: try `chidori add zod`']);
  commands.node = simulated('node', ['(docs vm) node is not installed — chidori runs TypeScript directly: try `chidori run agent.ts`']);

  commands.git = (argv, io) => {
    const sub = argv[1] ?? '';
    if (sub === 'status') {
      io.out('On branch main\nnothing to commit, working tree clean (docs VM)\n');
      return 0;
    }
    if (sub === 'add' || sub === 'commit' || sub === 'push' || sub === 'tag') {
      io.out(`(docs vm) git ${sub}: recorded — this sandbox has no remote\n`);
      return 0;
    }
    io.out(`(docs vm) git ${argv.slice(1).join(' ')}: simulated\n`);
    return 0;
  };

  commands.fly = simulated('fly', [
    '==> Verifying app config  ✓',
    '==> Building image        ✓ (simulated by the docs VM)',
    '==> Deploying chidori-agents',
    '    1 machine started; state: started, checks passing',
  ]);

  commands.tael = (argv, io) => {
    const joined = argv.slice(1).join(' ');
    io.out(
      `tael ${joined}\n` +
        '  ── the docs VM ships a stand-in for tael (the observability CLI). It reads\n' +
        '  .chidori/runs/<run_id>/ journals; run an agent first, then `chidori trace`.\n',
    );
    return 0;
  };

  commands.vi = (argv, io) => {
    io.out(`(docs vm) ${argv[0]}: no full-screen editor here — \`cat ${argv[1] ?? '<file>'}\` to read it; edits happen through the runnable examples\n`);
    return 0;
  };
  commands.vim = commands.vi;
  commands.nano = commands.vi;

  const script = (name: string, out: string): void => {
    commands[name] = simulated(name, [out]);
  };
  script('./scripts/check-npm-drift.sh', 'sdk versions in sync ✓ (simulated by the docs VM)');
  script('./scripts/check-sdk-versions.sh', 'sdk versions match Cargo.toml ✓ (simulated by the docs VM)');
  script('scripts/test262.sh', 'test262: 48123 passed, 0 failed (cached summary — simulated by the docs VM)');

  return commands;
}

/** Also let `curl` reach the fake session server + the real network. */
export function makeCurl(
  routeLocal: (method: string, url: string, body: string | null, headers: Record<string, string>) => Promise<{ status: number; body: string } | null>,
): ShellCommand {
  return async (argv, io) => {
    let method = 'GET';
    let data: string | null = null;
    let url = '';
    const headers: Record<string, string> = {};
    let silent = false;
    for (let i = 1; i < argv.length; i++) {
      const a = argv[i];
      if (a === '-s' || a === '-fsSL' || a === '-sS' || a === '-f' || a === '-L') silent = true;
      else if (a === '-X') method = argv[++i] ?? 'GET';
      else if (a === '-H') {
        const h = argv[++i] ?? '';
        const colon = h.indexOf(':');
        if (colon > 0) headers[h.slice(0, colon).trim().toLowerCase()] = h.slice(colon + 1).trim();
      } else if (a === '-d' || a === '--data' || a === '--data-raw') {
        data = argv[++i] ?? '';
        if (method === 'GET') method = 'POST';
      } else if (!a.startsWith('-')) url = a;
    }
    if (!url) {
      io.err('curl: no URL specified\n');
      return 2;
    }
    if (!/^https?:\/\//.test(url)) url = `http://${url}`;
    const parsed = new URL(url);
    if (parsed.hostname === 'localhost' || parsed.hostname === '127.0.0.1' || parsed.hostname === '0.0.0.0') {
      const local = await routeLocal(method, url, data, headers);
      if (local === null) {
        io.err(`curl: (7) Failed to connect to ${parsed.hostname} port ${parsed.port || 80}: Connection refused\n`);
        io.err('      (no server is listening in the docs VM — start one with `chidori serve <agent.ts> --port <port>`)\n');
        return 7;
      }
      io.out(local.body.endsWith('\n') ? local.body : `${local.body}\n`);
      return local.status >= 400 ? 22 : 0;
    }
    // The install one-liner and other public URLs: really fetch when we can.
    try {
      const res = await fetch(url, { method, body: data ?? undefined, headers });
      const text = await res.text();
      io.out(text.endsWith('\n') ? text : `${text}\n`);
      return res.ok ? 0 : 22;
    } catch {
      if (!silent) io.err(`curl: (6) Could not resolve host (browser sandbox network limits) — returning a simulated response\n`);
      io.out(`#!/bin/sh\necho "chidori installer (simulated: ${url} is unreachable from the browser sandbox)"\n`);
      return 0;
    }
  };
}
