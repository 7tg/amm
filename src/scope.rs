use globset::GlobBuilder;
use std::path::{Component, Path, PathBuf};

/// Strips a leading "./" and turns a trailing "/" into "/**".
pub fn normalize_glob(glob: &str) -> String {
    let mut g = glob.strip_prefix("./").unwrap_or(glob).to_string();
    if g.ends_with('/') {
        g.push_str("**");
    }
    g
}

/// Lexical equivalent of Node's `path.resolve`: joins onto the cwd when relative and
/// collapses `.` and `..` without touching the filesystem.
fn resolve(p: &Path) -> PathBuf {
    let base = if p.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    let mut out = PathBuf::new();
    for c in base.join(p).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when `file` (absolute) lies under `root` and matches one of `globs` (relative to root).
pub fn in_scope(file: &str, root: &str, globs: &[String]) -> bool {
    let file = resolve(Path::new(file));
    let root = resolve(Path::new(root));
    let Ok(rel) = file.strip_prefix(&root) else {
        return false;
    };
    let posix = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    if posix.is_empty() {
        return false;
    }
    globs.iter().any(|raw| {
        let g = normalize_glob(raw);
        if g == "**" {
            return true;
        }
        // Treat "<prefix>/**" as a plain prefix test, like the TS (Node's matcher skips dotfiles).
        if let Some(prefix) = g.strip_suffix("**")
            && prefix.ends_with('/')
            && posix.starts_with(prefix)
        {
            return true;
        }
        // An unparseable glob (e.g. extglob "!(b)") matches nothing.
        GlobBuilder::new(&g)
            .literal_separator(true)
            .build()
            .map(|glob| glob.compile_matcher().is_match(&posix))
            .unwrap_or(false)
    })
}

/// The leading path segments of a glob that contain no glob characters.
pub fn static_prefix(glob: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for seg in normalize_glob(glob).split('/') {
        if seg.chars().any(|c| "*?[]{}()!+@\\".contains(c)) {
            break;
        }
        out.push(seg.to_string());
    }
    out.join("/")
}

/// Conservative overlap test: two scopes overlap when any pair of static prefixes is
/// empty, equal or nested. "src/ap*" has prefix "src", so it overlaps everything under src/.
pub fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| {
        b.iter().any(|y| {
            let p = static_prefix(x);
            let q = static_prefix(y);
            p.is_empty()
                || q.is_empty()
                || p == q
                || p.starts_with(&format!("{q}/"))
                || q.starts_with(&format!("{p}/"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn in_scope_matches_globs_relative_to_the_worktree_root() {
        assert!(in_scope("/w/src/a.ts", "/w", &v(&["src/**"])));
        assert!(in_scope("/w/src/deep/a.ts", "/w", &v(&["src/**"])));
        assert!(!in_scope("/w/lib/a.ts", "/w", &v(&["src/**"])));
        assert!(in_scope("/w/README.md", "/w", &v(&["README.md", "docs/*.md"])));
    }

    #[test]
    fn in_scope_rejects_paths_outside_the_root() {
        assert!(!in_scope("/other/src/a.ts", "/w", &v(&["src/**"])));
        assert!(!in_scope("/w/../w2/src/a.ts", "/w", &v(&["**"])));
    }

    #[test]
    fn static_prefix_stops_at_the_first_glob_segment() {
        assert_eq!(static_prefix("src/api/**/*.ts"), "src/api");
        assert_eq!(static_prefix("src/a.ts"), "src/a.ts");
        assert_eq!(static_prefix("**/*.md"), "");
    }

    #[test]
    fn scopes_overlap_is_conservative_on_shared_prefixes() {
        assert!(!scopes_overlap(&v(&["src/a.ts"]), &v(&["src/b.ts"])));
        assert!(scopes_overlap(&v(&["src/**"]), &v(&["src/a.ts"])));
        assert!(!scopes_overlap(&v(&["src/api/**"]), &v(&["src/ui/**"])));
        assert!(scopes_overlap(&v(&["src/ap*"]), &v(&["src/api.ts"])));
        assert!(scopes_overlap(&v(&["**/*.md"]), &v(&["src/x.ts"])));
    }

    #[test]
    fn globs_are_normalized_leading_dot_slash_and_trailing_slash() {
        assert!(in_scope("/w/src/a.ts", "/w", &v(&["./src/**"])));
        assert!(in_scope("/w/src/a.ts", "/w", &v(&["src/"])));
        assert_eq!(static_prefix("./src/a/**"), "src/a");
    }

    #[test]
    fn extglob_and_other_glob_characters_end_the_static_prefix() {
        assert_eq!(static_prefix("src/!(b).ts"), "src");
        assert!(scopes_overlap(&v(&["src/!(b).ts"]), &v(&["src/a.ts"])));
    }

    #[test]
    fn dotfiles_match_recursive_globs_dot_dot_name_is_not_outside_the_root() {
        assert!(in_scope("/w/src/.env", "/w", &v(&["src/**"])));
        assert!(in_scope("/w/src/x/.env", "/w", &v(&["src/**"])));
        assert!(in_scope("/w/.env", "/w", &v(&["**"])));
        assert!(in_scope("/w/..env", "/w", &v(&["**"])));
        assert!(!in_scope("/w/../w2/a", "/w", &v(&["**"])));
    }
}
