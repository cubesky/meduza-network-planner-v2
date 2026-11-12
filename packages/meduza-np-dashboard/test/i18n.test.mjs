import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import ts from '../web/node_modules/typescript/lib/typescript.js';

const root = fileURLToPath(new URL('../web/src/', import.meta.url));
const walk = dir => readdirSync(dir, { withFileTypes: true }).flatMap(entry => entry.isDirectory() ? walk(join(dir, entry.name)) : [join(dir, entry.name)]);

test('Dashboard UI uses English message keys rather than hardcoded Chinese labels', () => {
  const problems = [];
  for (const path of walk(root).filter(path => /\.tsx?$/.test(path))) {
    const source = ts.createSourceFile(path, readFileSync(path, 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    const visit = node => {
      if ((ts.isStringLiteral(node) || ts.isJsxText(node) || ts.isNoSubstitutionTemplateLiteral(node)) && /[\u3400-\u9fff]/.test(node.text)) {
        problems.push(`${path}: ${node.text}`);
      }
      ts.forEachChild(node, visit);
    };
    visit(source);
  }
  assert.deepEqual(problems, []);
});

test('Dashboard dictionaries preserve every message placeholder in both Chinese variants', () => {
  const placeholders = text => [...text.matchAll(/\{(\w+)\}/g)].map(match => match[1]).sort();
  for (const locale of ['zh-Hans', 'zh-Hant']) {
    const dictionary = JSON.parse(readFileSync(join(root, 'locales', `${locale}.json`), 'utf8'));
    for (const [key, value] of Object.entries(dictionary)) {
      assert.ok(value, `${locale}: ${key}`);
      assert.deepEqual(placeholders(value), placeholders(key), `${locale}: ${key}`);
    }
  }
});
