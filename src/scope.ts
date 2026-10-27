import path from 'node:path';

/** Strips a leading "./" and turns a trailing "/" into "/**". */
export function normalizeGlob(glob: string): string {
  let g = glob.startsWith('./') ? glob.slice(2) : glob;
  if (g.endsWith('/')) g += '**';
  return g;
}

/** True when `file` (absolute) lies under `root` and matches one of `globs` (relative to root). */
export function inScope(file: string, root: string, globs: string[]): boolean {
  const rel = path.relative(root, path.resolve(file));
  if (rel === '' || rel === '..' || rel.startsWith('..' + path.sep) || path.isAbsolute(rel)) return false;
  const posix = rel.split(path.sep).join('/');
  return globs.some((raw) => {
    const g = normalizeGlob(raw);
    if (g === '**') return true;
    // matchesGlob skips dotfiles, so treat "<prefix>/**" as a plain prefix test.
    if (g.endsWith('/**') && posix.startsWith(g.slice(0, -2))) return true;
    return path.matchesGlob(posix, g);
  });
}

/** The leading path segments of a glob that contain no glob characters. */
export function staticPrefix(glob: string): string {
  const out: string[] = [];
  for (const seg of normalizeGlob(glob).split('/')) {
    if (/[*?[\]{}()!+@\\]/.test(seg)) break;
    out.push(seg);
  }
  return out.join('/');
}

/**
 * Conservative overlap test: two scopes overlap when any pair of static prefixes is
 * empty, equal or nested. "src/ap*" has prefix "src", so it overlaps everything under src/.
 */
export function scopesOverlap(a: string[], b: string[]): boolean {
  return a.some((x) =>
    b.some((y) => {
      const p = staticPrefix(x);
      const q = staticPrefix(y);
      return p === '' || q === '' || p === q || p.startsWith(q + '/') || q.startsWith(p + '/');
    }),
  );
}
