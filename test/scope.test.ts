import assert from 'node:assert/strict';
import { test } from 'node:test';
import { inScope, scopesOverlap, staticPrefix } from '../src/scope.ts';

test('inScope matches globs relative to the worktree root', () => {
  assert.equal(inScope('/w/src/a.ts', '/w', ['src/**']), true);
  assert.equal(inScope('/w/src/deep/a.ts', '/w', ['src/**']), true);
  assert.equal(inScope('/w/lib/a.ts', '/w', ['src/**']), false);
  assert.equal(inScope('/w/README.md', '/w', ['README.md', 'docs/*.md']), true);
});

test('inScope rejects paths outside the root', () => {
  assert.equal(inScope('/other/src/a.ts', '/w', ['src/**']), false);
  assert.equal(inScope('/w/../w2/src/a.ts', '/w', ['**']), false);
});

test('staticPrefix stops at the first glob segment', () => {
  assert.equal(staticPrefix('src/api/**/*.ts'), 'src/api');
  assert.equal(staticPrefix('src/a.ts'), 'src/a.ts');
  assert.equal(staticPrefix('**/*.md'), '');
});

test('scopesOverlap is conservative on shared prefixes', () => {
  assert.equal(scopesOverlap(['src/a.ts'], ['src/b.ts']), false);
  assert.equal(scopesOverlap(['src/**'], ['src/a.ts']), true);
  assert.equal(scopesOverlap(['src/api/**'], ['src/ui/**']), false);
  assert.equal(scopesOverlap(['src/ap*'], ['src/api.ts']), true);
  assert.equal(scopesOverlap(['**/*.md'], ['src/x.ts']), true);
});
