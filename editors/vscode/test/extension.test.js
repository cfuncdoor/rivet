'use strict';

// Unit tests for the pure logic (run: `npm test` / `node --test test/`).
// No vscode API is touched here; `configureProject` gets a stub runner.

const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { describe, it } = require('node:test');

const ext = require('../extension.js');

function tmpProject(files = { 'rivet.json': '{}' }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rivet-ext-'));
  for (const [name, content] of Object.entries(files)) {
    const p = path.join(dir, name);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, content);
  }
  return dir;
}

describe('findProjectRoot', () => {
  it('finds rivet.json upward', () => {
    const root = tmpProject();
    const deep = path.join(root, 'src', 'nested');
    fs.mkdirSync(deep, { recursive: true });
    assert.strictEqual(ext.findProjectRoot(deep), root);
    assert.strictEqual(ext.findProjectRoot(root), root);
  });

  it('returns null past the root', () => {
    const misses = [os.tmpdir(), path.parse(os.tmpdir()).root];
    for (const dir of misses) {
      if (!fs.existsSync(path.join(dir, 'rivet.json'))) {
        assert.strictEqual(ext.findProjectRoot(dir), null);
      }
    }
  });
});

describe('dbPathFor', () => {
  it('maps dev->debug, release->release', () => {
    assert.strictEqual(
      ext.dbPathFor('/proj', 'dev'),
      path.join('/proj', 'target', 'debug', 'compile_commands.json')
    );
    assert.strictEqual(
      ext.dbPathFor('/proj', 'release'),
      path.join('/proj', 'target', 'release', 'compile_commands.json')
    );
  });
});

describe('mergeCppProperties', () => {
  it('creates a fresh doc', () => {
    const doc = ext.mergeCppProperties(null, '/db.json');
    assert.strictEqual(doc.version, '4');
    assert.deepStrictEqual(doc.configurations, [{ name: 'rivet', compileCommands: '/db.json' }]);
  });

  it('keeps other configs and updates rivet in place', () => {
    const existing = {
      version: '4',
      configurations: [
        { name: 'Linux', includePath: ['/x'] },
        { name: 'rivet', compileCommands: '/old.json', extra: 1 },
      ],
    };
    const doc = ext.mergeCppProperties(existing, '/new.json');
    assert.strictEqual(doc.configurations.length, 2);
    assert.deepStrictEqual(doc.configurations[0], { name: 'Linux', includePath: ['/x'] });
    assert.deepStrictEqual(doc.configurations[1], {
      name: 'rivet',
      compileCommands: '/new.json',
      extra: 1,
    });
  });
});

describe('configureProject', () => {
  it('writes props without running rivet when the db exists', async () => {
    const root = tmpProject();
    const db = ext.dbPathFor(root, 'dev');
    fs.mkdirSync(path.dirname(db), { recursive: true });
    fs.writeFileSync(db, '[]');
    let ran = 0;
    const out = await ext.configureProject(
      root,
      { binaryPath: 'rivet', profile: 'dev' },
      { run: async () => { ran += 1; } }
    );
    assert.strictEqual(ran, 0);
    const props = JSON.parse(fs.readFileSync(out.props, 'utf8'));
    assert.strictEqual(props.configurations[0].compileCommands, db);
  });

  it('runs rivet when the db is missing and fails loudly otherwise', async () => {
    const root = tmpProject();
    await assert.rejects(
      ext.configureProject(root, { binaryPath: 'rivet', profile: 'dev' }, { run: async () => {} }),
      /no compile database/
    );
  });

  it('propagates rivet failures', async () => {
    const root = tmpProject();
    await assert.rejects(
      ext.configureProject(
        root,
        { binaryPath: 'rivet', profile: 'dev' },
        {
          run: async () => {
            throw new Error('Error: no toolchain');
          },
        }
      ),
      /no toolchain/
    );
  });
});
