'use strict';

// Rivet VSCode extension: point the Microsoft C/C++ extension at rivet's
// compile database so project + dependency headers resolve (no squiggles).
//
// How it works: `rivet compile-commands` already emits a standard
// compile_commands.json carrying every include dir (own + adapters +
// scripts). This extension runs that command when needed and records the
// database path in `.vscode/c_cpp_properties.json` (merged, never
// clobbering your other configs). `vscode` is required lazily so the
// pure logic below unit-tests under plain node.

const { execFile } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

/**
 * Nearest ancestor (inclusive) of `startDir` containing `rivet.json`,
 * or null past the filesystem root.
 * @param {string} startDir
 * @returns {string|null}
 */
function findProjectRoot(startDir) {
  let dir = path.resolve(startDir);
  for (;;) {
    try {
      if (fs.statSync(path.join(dir, 'rivet.json')).isFile()) {
        return dir;
      }
    } catch {
      // Missing or unreadable: keep walking up.
    }
    const parent = path.dirname(dir);
    if (parent === dir) {
      return null;
    }
    dir = parent;
  }
}

/**
 * Where `rivet compile-commands [--release]` writes the database.
 * Mirrors rivet's `target/debug|release` layout (`dev` -> `debug`).
 * @param {string} projectRoot
 * @param {string} profile `dev` or `release`
 * @returns {string}
 */
function dbPathFor(projectRoot, profile) {
  const sub = profile === 'release' ? 'release' : 'debug';
  return path.join(projectRoot, 'target', sub, 'compile_commands.json');
}

/**
 * Merge a `rivet` config carrying `compileCommands` into parsed
 * c_cpp_properties.json (or a fresh doc). Existing configs are kept;
 * an existing `rivet` entry is updated in place. Always sets a version.
 * @param {unknown} existing parsed JSON or null
 * @param {string} dbPath absolute database path
 * @returns {object}
 */
function mergeCppProperties(existing, dbPath) {
  const doc =
    existing && typeof existing === 'object' && !Array.isArray(existing)
      ? { ...existing }
      : {};
  const configs = Array.isArray(doc.configurations) ? [...doc.configurations] : [];
  const entry = { name: 'rivet', compileCommands: dbPath };
  const at = configs.findIndex((c) => !!c && c.name === 'rivet');
  if (at >= 0) {
    configs[at] = { ...configs[at], ...entry };
  } else {
    configs.push(entry);
  }
  doc.configurations = configs;
  if (typeof doc.version !== 'string') {
    doc.version = '4';
  }
  return doc;
}

/**
 * Run the rivet binary; rejects with combined output on failure.
 * @param {string} binary
 * @param {string[]} args
 * @param {string} cwd
 * @returns {Promise<string>} stdout
 */
function runRivet(binary, args, cwd) {
  return new Promise((resolve, reject) => {
    execFile(binary, args, { cwd }, (error, stdout, stderr) => {
      if (error) {
        reject(new Error(`${stdout}${stderr}`.trim() || error.message));
      } else {
        resolve(stdout);
      }
    });
  });
}

/**
 * Bring one project up to date: generate the database when missing,
 * then (re)point `.vscode/c_cpp_properties.json` at it.
 * @param {string} projectRoot
 * @param {{ binaryPath: string, profile: string }} opts
 * @param {{ run?: Function }} [deps] injectable runner (tests)
 * @returns {Promise<{ db: string, props: string }>}
 */
async function configureProject(projectRoot, opts, deps = {}) {
  const run = deps.run || runRivet;
  const args = ['compile-commands'];
  if (opts.profile === 'release') {
    args.push('--release');
  }
  const db = dbPathFor(projectRoot, opts.profile);
  if (!fs.existsSync(db)) {
    await run(opts.binaryPath, args, projectRoot);
    if (!fs.existsSync(db)) {
      throw new Error(
        `rivet produced no compile database at ${db} (build the project once with \`rivet build\`)`
      );
    }
  }
  const vscodeDir = path.join(projectRoot, '.vscode');
  fs.mkdirSync(vscodeDir, { recursive: true });
  const propsPath = path.join(vscodeDir, 'c_cpp_properties.json');
  let existing = null;
  try {
    existing = JSON.parse(fs.readFileSync(propsPath, 'utf8'));
  } catch (err) {
    if (err && err.code !== 'ENOENT') {
      // Preserve the broken file before replacing it.
      fs.writeFileSync(`${propsPath}.bak`, fs.readFileSync(propsPath));
    }
  }
  fs.writeFileSync(propsPath, `${JSON.stringify(mergeCppProperties(existing, db), null, 4)}\n`);
  return { db, props: propsPath };
}

/**
 * @param {{ subscriptions: object[], commands: object, window: object, workspace: object }} vscode
 */
function activate(context) {
  // Lazy: keeps the module loadable under plain node for unit tests.
  const vscode = require('vscode');

  async function runForRoots(roots, opts, silent) {
    let configured = 0;
    for (const root of roots) {
      try {
        await configureProject(root, opts);
        configured += 1;
      } catch (err) {
        const message = err && err.message ? err.message : `${err}`;
        if (silent) {
          console.warn(`[rivet] IntelliSense auto-configure failed for ${root}: ${message}`);
        } else {
          void vscode.window.showErrorMessage(`Rivet: IntelliSense setup failed: ${message}`);
        }
      }
    }
    return configured;
  }

  function currentOpts() {
    const cfg = vscode.workspace.getConfiguration('rivet');
    return {
      binaryPath: cfg.get('binaryPath', 'rivet'),
      profile: cfg.get('profile', 'dev'),
    };
  }

  function candidateRoots() {
    const seen = new Set();
    const roots = [];
    const folders = vscode.workspace.workspaceFolders || [];
    const editorPath = vscode.window.activeTextEditor?.document.uri.fsPath;
    const starts = [];
    if (editorPath) {
      starts.push(path.dirname(editorPath));
    }
    for (const folder of folders) {
      starts.push(folder.uri.fsPath);
    }
    for (const start of starts) {
      const root = findProjectRoot(start);
      if (root && !seen.has(root)) {
        seen.add(root);
        roots.push(root);
      }
    }
    return roots;
  }

  context.subscriptions.push(
    vscode.commands.registerCommand('rivet.configureIntelliSense', async () => {
      const roots = candidateRoots();
      if (roots.length === 0) {
        void vscode.window.showErrorMessage(
          'Rivet: no rivet.json found above the open file or workspace folders'
        );
        return;
      }
      const done = await runForRoots(roots, currentOpts(), false);
      if (done > 0) {
        void vscode.window.showInformationMessage(
          `Rivet: IntelliSense now follows compile_commands.json (${done} project${done === 1 ? '' : 's'})`
        );
      }
    })
  );

  // Auto-configure on activation (opening a C/C++ file): cheap after the
  // first run, since an existing database is reused, never rebuilt.
  try {
    if (vscode.workspace.getConfiguration('rivet').get('autoConfigure', true)) {
      void runForRoots(candidateRoots(), currentOpts(), true);
    }
  } catch {
    // Configuration API hiccups must never break activation.
  }
}

function deactivate() {}

module.exports = {
  activate,
  deactivate,
  findProjectRoot,
  dbPathFor,
  mergeCppProperties,
  runRivet,
  configureProject,
};
