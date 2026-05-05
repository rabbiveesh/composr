//! Composer's PSR-4 / PSR-0 admission rules for the optimized classmap.
//!
//! When `composer dump-autoload -o` scans a PSR-4 or PSR-0 root, it does NOT
//! blindly include every class declaration it finds. The class's FQCN must
//! map back to the file's location under that root using PSR-4 / PSR-0 rules.
//! Concretely:
//!
//! * **PSR-4** — file `<root>/Foo/Bar/Baz.php` must declare `<prefix>Foo\Bar\Baz`
//!   (after substituting `\` for the path separator). Other classes declared
//!   in the same file are silently dropped (Composer logs a warning).
//! * **PSR-0** — same idea, but the *class name* itself can encode directories
//!   via underscores, so a file at `<root>/HTMLPurifier/Lexer/PH5P.php` may
//!   declare `HTMLPurifier_Lexer_PH5P` (PSR-0 underscore form) or
//!   `HTMLPurifier\Lexer\PH5P` (namespaced form). Either is valid.
//! * **Classmap** — every declared class is included unconditionally.
//!
//! Without this filter we generate spurious entries like `OpenAI` (a class in
//! the global namespace declared inside a PSR-4 dir) or
//! `Tests\Foo\StubHelper` (a helper class living in a sibling test file).
//! Composer drops these; so do we.

use crate::parse::Declaration;
use crate::walk::PhpSource;
use std::path::Path;

/// True if this declaration should be admitted to the classmap given the
/// source it was reached through.
pub fn admit(decl: &Declaration, file: &Path, source: &PhpSource) -> bool {
    match source {
        PhpSource::Classmap => true,
        PhpSource::Psr4 { prefix, root } => {
            psr4_strict_match(&decl.fqcn, prefix.as_bytes(), root, file)
        }
        PhpSource::Psr0 { prefix, root } => {
            psr0_match(&decl.fqcn, prefix.as_bytes(), root, file)
        }
    }
}

fn psr4_strict_match(fqcn: &[u8], prefix: &[u8], root: &Path, file: &Path) -> bool {
    let Ok(rel) = file.strip_prefix(root) else {
        return false;
    };
    let Some(rel_s) = rel.to_str() else {
        return false;
    };
    let stem = rel_s.strip_suffix(".php").unwrap_or(rel_s);
    let mut expected: Vec<u8> = Vec::with_capacity(prefix.len() + stem.len());
    expected.extend_from_slice(prefix);
    for c in stem.chars() {
        if c == std::path::MAIN_SEPARATOR {
            expected.push(b'\\');
        } else {
            // ASCII path component, push raw byte (matches `c.to_string()`
            // for the cases we care about; FQCN ASCII characters here come
            // out of file paths which are guaranteed valid UTF-8 by `to_str`).
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            expected.extend_from_slice(s.as_bytes());
        }
    }
    fqcn == expected.as_slice()
}

fn psr0_match(fqcn: &[u8], prefix: &[u8], root: &Path, file: &Path) -> bool {
    let starts = fqcn.starts_with(prefix)
        || (!prefix.ends_with(b"\\") && {
            let mut p_back = prefix.to_vec();
            p_back.push(b'\\');
            let mut p_under = prefix.to_vec();
            p_under.push(b'_');
            fqcn.starts_with(&p_back) || fqcn.starts_with(&p_under)
        });
    if !starts {
        return false;
    }

    // PSR-0: namespace separators (`\`) and underscores in the *class name*
    // (last `\\`-segment) both map to directory separators.
    let (ns, cls) = match memrchr_byte(fqcn, b'\\') {
        Some(idx) => (&fqcn[..idx], &fqcn[idx + 1..]),
        None => (b"".as_slice(), fqcn),
    };
    let mut expected_rel: Vec<u8> = Vec::new();
    if !ns.is_empty() {
        for &b in ns {
            expected_rel.push(if b == b'\\' { b'/' } else { b });
        }
        expected_rel.push(b'/');
    }
    for &b in cls {
        expected_rel.push(if b == b'_' { b'/' } else { b });
    }
    expected_rel.extend_from_slice(b".php");

    let Ok(rel) = file.strip_prefix(root) else {
        return false;
    };
    let Some(rel_s) = rel.to_str() else {
        return false;
    };
    rel_s.as_bytes() == expected_rel.as_slice()
}

fn memrchr_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().rposition(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::DeclKind;
    use std::path::PathBuf;

    fn d(fqcn: &str) -> Declaration {
        Declaration {
            fqcn: fqcn.as_bytes().to_vec(),
            kind: DeclKind::Class,
        }
    }

    #[test]
    fn psr4_strict_accepts_matching_layout() {
        let src = PhpSource::Psr4 {
            prefix: "App\\".to_string(),
            root: PathBuf::from("/p/app"),
        };
        let file = PathBuf::from("/p/app/Foo/Bar.php");
        assert!(admit(&d("App\\Foo\\Bar"), &file, &src));
    }

    #[test]
    fn psr4_strict_rejects_extra_class_in_same_file() {
        // Composer drops sibling classes co-located in PSR-4 files.
        let src = PhpSource::Psr4 {
            prefix: "Tests\\".to_string(),
            root: PathBuf::from("/p/tests"),
        };
        let file = PathBuf::from("/p/tests/Feature/QueryTaggingTest.php");
        assert!(admit(&d("Tests\\Feature\\QueryTaggingTest"), &file, &src));
        assert!(!admit(
            &d("Tests\\Feature\\QueryTaggingSampleJob"),
            &file,
            &src
        ));
    }

    #[test]
    fn psr4_strict_rejects_global_class_in_psr4_dir() {
        // openai-php/client/src/OpenAI.php declares global `OpenAI`.
        let src = PhpSource::Psr4 {
            prefix: "OpenAI\\".to_string(),
            root: PathBuf::from("/p/vendor/openai-php/client/src"),
        };
        let file = PathBuf::from("/p/vendor/openai-php/client/src/OpenAI.php");
        assert!(!admit(&d("OpenAI"), &file, &src));
    }

    #[test]
    fn psr0_underscore_form_matches() {
        let src = PhpSource::Psr0 {
            prefix: "HTMLPurifier".to_string(),
            root: PathBuf::from("/p/library"),
        };
        let file = PathBuf::from("/p/library/HTMLPurifier/Lexer/PH5P.php");
        assert!(admit(&d("HTMLPurifier_Lexer_PH5P"), &file, &src));
        assert!(!admit(&d("HTML5"), &file, &src));
        assert!(!admit(&d("HTML5TreeConstructer"), &file, &src));
    }

    #[test]
    fn classmap_admits_everything() {
        let src = PhpSource::Classmap;
        let file = PathBuf::from("/anything");
        assert!(admit(&d("Anything"), &file, &src));
        assert!(admit(&d("Whatever\\Else"), &file, &src));
    }
}
