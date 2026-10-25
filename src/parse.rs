use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::{Node, Parser, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclKind {
    Class,
    Interface,
    Trait,
    Enum,
}

/// One class-like declaration. We store `fqcn` as raw bytes rather than a
/// `String` because PHP identifiers may legally contain any byte ≥ 0x80
/// (e.g. Symfony declares `class \xa9` in vendor/symfony/cache; UTF-8 lossy
/// conversion would mangle that into U+FFFD and break byte-identity with
/// Composer's classmap output).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub fqcn: Vec<u8>,
    pub kind: DeclKind,
}

/// Wraps a single tree-sitter parser. Cheap to construct, but reuse across
/// files to avoid the language-handle setup cost per call.
pub struct PhpParser {
    parser: Parser,
}

impl PhpParser {
    pub fn new() -> Self {
        let mut parser = Parser::new();
        let lang: tree_sitter::Language = tree_sitter_php::LANGUAGE_PHP.into();
        parser
            .set_language(&lang)
            .expect("tree-sitter-php language version mismatch");
        PhpParser { parser }
    }

    /// Parse a PHP file's bytes and extract every top-level class-like
    /// declaration along with its fully-qualified name. Top-level here means
    /// "directly under the program or a namespace block" — not inside a
    /// function, method, or anonymous-class body.
    pub fn extract(&mut self, source: &[u8]) -> Vec<Declaration> {
        let tree: Tree = match self.parser.parse(source, None) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let root = tree.root_node();
        let mut out = Vec::new();
        walk_program(root, source, "", &mut out);
        out
    }
}

impl Default for PhpParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk a node's subtree, tracking the active file-level namespace and
/// emitting class-like declarations. The PHP grammar lets a single file have:
///   - file-level `namespace Foo;` (declarations until EOF or next namespace)
///   - braced `namespace Foo { ... }` blocks
///   - no namespace at all (global)
///
/// We descend through control-flow constructs (if / else / for / while /
/// switch / try / etc.) so that stub files declaring `class Foo` inside
/// `if (PHP_VERSION_ID < ...)` blocks are picked up — Composer's tokenizer
/// sees them as ordinary class tokens and so do we.
///
/// We do NOT descend into:
///   - function / method bodies (PHP allows `function f() { class X {} }` —
///     Composer skips it, so do we)
///   - anonymous functions / arrow functions
///   - `new class { ... }` (anonymous class) expressions
fn walk_program(node: Node, src: &[u8], current_ns: &str, out: &mut Vec<Declaration>) {
    let mut cursor = node.walk();
    let mut active_ns: String = current_ns.to_string();

    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_definition" => {
                let ns_name = child
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, src).replace(['\n', '\r'], "").trim().to_string())
                    .unwrap_or_default();
                if let Some(body) = child.child_by_field_name("body") {
                    // Braced form: `namespace Foo { ... }`.
                    walk_program(body, src, &ns_name, out);
                } else {
                    // File-level form: applies to subsequent siblings.
                    active_ns = ns_name;
                }
            }
            "class_declaration" => emit_decl(&child, src, &active_ns, DeclKind::Class, out),
            "interface_declaration" => {
                emit_decl(&child, src, &active_ns, DeclKind::Interface, out)
            }
            "trait_declaration" => emit_decl(&child, src, &active_ns, DeclKind::Trait, out),
            "enum_declaration" => emit_decl(&child, src, &active_ns, DeclKind::Enum, out),

            // Skip these subtrees entirely — anything declared inside is
            // invisible to Composer's classmap.
            "function_definition"
            | "method_declaration"
            | "anonymous_function_creation_expression"
            | "anonymous_function"
            | "arrow_function"
            | "object_creation_expression" => { /* skip */ }

            // Tree-sitter-php (0.22.x) trips on some valid PHP 8.1+ enum
            // constructs (e.g., enums declaring constants) and produces an
            // ERROR node containing the keyword + name tokens. Recover by
            // scanning for `<keyword> <name>` pairs at any depth in the
            // error subtree — we still pick up the declaration even when
            // the body is unparsed. Composer's PHP-tokenizer-based scanner
            // doesn't have this problem; we play catch-up.
            "ERROR" => recover_from_error(child, src, &active_ns, out),

            // Everything else (if / else / for / while / switch / try /
            // compound_statement / declaration_list / etc.): recurse so that
            // class_declaration nested under control-flow is still found.
            _ => walk_program(child, src, &active_ns, out),
        }
    }
}

/// Tree-sitter ERROR-node recovery. Walks every descendant of the ERROR
/// subtree looking for an adjacent `<class|interface|trait|enum> <name>`
/// keyword/identifier pair, and emits a declaration for each. We scan the
/// whole subtree (not just immediate children) so that nested control-flow
/// or comments between the keyword and its name don't hide the declaration.
fn recover_from_error(node: Node, src: &[u8], ns: &str, out: &mut Vec<Declaration>) {
    // Pre-collect every node in the ERROR subtree in a flat depth-first
    // order. Then a single linear scan catches `(keyword, name)` pairs.
    let mut stack = vec![node];
    let mut flat: Vec<Node> = Vec::new();
    while let Some(n) = stack.pop() {
        flat.push(n);
        let mut c = n.walk();
        // Push children in reverse so flat order is depth-first L→R.
        let mut kids: Vec<Node> = n.children(&mut c).collect();
        kids.reverse();
        for k in kids {
            stack.push(k);
        }
    }

    let mut i = 0;
    while i + 1 < flat.len() {
        let kw = match flat[i].kind() {
            "class" => Some(DeclKind::Class),
            "interface" => Some(DeclKind::Interface),
            "trait" => Some(DeclKind::Trait),
            "enum" => Some(DeclKind::Enum),
            _ => None,
        };
        if let Some(kind) = kw {
            // After a keyword, find the next node usable as a name. We accept
            // either an explicit `name` node or any node whose byte content
            // is a valid PHP identifier — PHP lets identifiers contain
            // \x80-\xff (some Symfony classes use a single Latin-1 byte as
            // their name; tree-sitter-php parses that as ERROR rather than
            // `name`, but Composer's tokenizer accepts it).
            let mut j = i + 1;
            let mut found: Option<&[u8]> = None;
            while j < flat.len() {
                if matches!(flat[j].kind(), "class" | "interface" | "trait" | "enum") {
                    break;
                }
                if flat[j].kind() == "name" {
                    found = Some(trim_ascii(&src[flat[j].byte_range()]));
                    break;
                }
                let raw = &src[flat[j].byte_range()];
                if is_php_identifier(raw) {
                    found = Some(raw);
                    break;
                }
                j += 1;
            }
            if let Some(name_bytes) = found {
                if !name_bytes.is_empty() {
                    let fqcn = compose_fqcn(ns.as_bytes(), name_bytes);
                    out.push(Declaration { fqcn, kind });
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// PHP identifier: `[A-Za-z_\x80-\xff][A-Za-z0-9_\x80-\xff]*`. Used by our
/// ERROR-recovery to accept identifier bytes that tree-sitter-php's grammar
/// rejects (notably single-byte high-bit names like Symfony's `class \xa9`).
fn is_php_identifier(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    let valid_start = first.is_ascii_alphabetic() || first == b'_' || first >= 0x80;
    if !valid_start {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80)
}

fn emit_decl(node: &Node, src: &[u8], ns: &str, kind: DeclKind, out: &mut Vec<Declaration>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        // Anonymous classes don't have a name field at this position; even
        // if they did, anonymous classes appear inside object_creation_expression,
        // not as siblings of class_declaration.
        return;
    };
    let name_bytes = trim_ascii(&src[name_node.byte_range()]);
    if name_bytes.is_empty() {
        return;
    }
    let fqcn = compose_fqcn(ns.as_bytes(), name_bytes);
    out.push(Declaration { fqcn, kind });
}

fn compose_fqcn(ns: &[u8], name: &[u8]) -> Vec<u8> {
    if ns.is_empty() {
        name.to_vec()
    } else {
        let mut v = Vec::with_capacity(ns.len() + 1 + name.len());
        v.extend_from_slice(ns);
        v.push(b'\\');
        v.extend_from_slice(name);
        v
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

fn node_text(node: &Node, src: &[u8]) -> String {
    // Used only for namespace names which we expect to be ASCII / UTF-8.
    String::from_utf8_lossy(&src[node.byte_range()]).into_owned()
}

/// Convenience: read a PHP file from disk and extract declarations.
pub fn extract_from_file(path: &Path, parser: &mut PhpParser) -> std::io::Result<Vec<Declaration>> {
    let bytes = std::fs::read(path)?;
    Ok(parser.extract(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<Declaration> {
        PhpParser::new().extract(src.as_bytes())
    }

    fn fqcns(decls: &[Declaration]) -> Vec<String> {
        decls
            .iter()
            .map(|d| String::from_utf8_lossy(&d.fqcn).into_owned())
            .collect()
    }

    #[test]
    fn file_level_namespace() {
        let r = extract("<?php\nnamespace Foo\\Bar;\nclass Baz {}\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].fqcn, b"Foo\\Bar\\Baz");
        assert_eq!(r[0].kind, DeclKind::Class);
    }

    #[test]
    fn braced_namespace() {
        let r = extract("<?php\nnamespace Foo { class A {} }\nnamespace Bar { class B {} }\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].fqcn, b"Foo\\A");
        assert_eq!(r[1].fqcn, b"Bar\\B");
    }

    #[test]
    fn no_namespace() {
        let r = extract("<?php\nclass GlobalCls {}\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].fqcn, b"GlobalCls");
    }

    #[test]
    fn interfaces_traits_enums() {
        let r = extract(
            "<?php\nnamespace N;\ninterface I {}\ntrait T {}\nenum E {}\nclass C {}\n",
        );
        let kinds: Vec<_> = r.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DeclKind::Interface,
                DeclKind::Trait,
                DeclKind::Enum,
                DeclKind::Class,
            ]
        );
    }

    #[test]
    fn skips_anonymous_class() {
        let r = extract("<?php\nnamespace N;\n$x = new class { public $a; };\n");
        assert!(r.is_empty(), "got {:?}", r);
    }

    #[test]
    fn skips_class_inside_function() {
        let r = extract(
            "<?php\nnamespace N;\nfunction f() { class Inside {} }\nclass Outside {}\n",
        );
        assert_eq!(fqcns(&r), vec!["N\\Outside"]);
    }

    #[test]
    fn finds_class_inside_if_block() {
        // Symfony polyfills wrap stub classes in version checks. Composer's
        // classmap includes them; ours must too.
        let r = extract(
            "<?php\nif (PHP_VERSION_ID < 80300) {\n    class DateError extends Error {}\n}\n",
        );
        assert_eq!(fqcns(&r), vec!["DateError"]);
    }

    #[test]
    fn finds_class_inside_nested_if_else() {
        let r = extract(
            "<?php\nnamespace G;\nif (false) {\n    class A {}\n} else {\n    class B {}\n}\n",
        );
        let mut names = fqcns(&r);
        names.sort();
        assert_eq!(names, vec!["G\\A", "G\\B"]);
    }

    #[test]
    fn recovers_unit_enum_with_constants() {
        // PHP 8.1+ allows enum-level constants; tree-sitter-php 0.22 trips
        // on this and emits an ERROR node. Verify our recovery picks up the
        // enum name regardless.
        let r = extract(
            "<?php\nnamespace S;\nenum Requirement\n{\n    public const X = 'x';\n    case A;\n}\n",
        );
        assert_eq!(fqcns(&r), vec!["S\\Requirement"]);
        assert_eq!(r[0].kind, DeclKind::Enum);
    }

    #[test]
    fn recovers_backed_enum_with_complex_body() {
        let r = extract(
            "<?php\nnamespace U;\nenum Operator: string\n{\n    private const X = 'x';\n    case A = 'a';\n}\n",
        );
        assert!(fqcns(&r).contains(&"U\\Operator".to_string()));
    }

    #[test]
    fn recovers_high_byte_class_name() {
        // Symfony's vendor/symfony/cache/Traits/ValueWrapper.php declares
        // `class <0xa9>` (a single Latin-1 ©). PHP identifiers legally allow
        // \x80-\xff but tree-sitter-php parses such names as ERROR. Recovery
        // must still extract them so we match Composer's classmap byte-for-
        // byte (and so the class loads correctly at runtime).
        let mut src: Vec<u8> = b"<?php\nclass ".to_vec();
        src.push(0xa9);
        src.extend_from_slice(b"\n{\n}\n");
        let decls = PhpParser::new().extract(&src);
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].fqcn, vec![0xa9]);
        assert_eq!(decls[0].kind, DeclKind::Class);
    }

    #[test]
    fn finds_class_containing_nowdoc_argument() {
        // From laravel/framework's DatabaseSqliteSchemaStateTest.php: the
        // tree-sitter-php 0.22 grammar lost the whole file to ERROR on this
        // nowdoc and the class went missing from the classmap.
        let r = extract(concat!(
            "<?php\n",
            "namespace A;\n",
            "class B {\n",
            "function f() {\n",
            "        $files->shouldHaveReceived('put')->with('schema.sql', <<<'SQL'\n",
            "            CREATE VIRTUAL TABLE posts using fts5(body)\n",
            "            /* posts(body) */;\n",
            "            CREATE TABLE IF NOT EXISTS \"logs_data\"(\"id\" integer primary key);\n",
            "\n",
            "            SQL.PHP_EOL);\n",
            "}\n",
            "}\n",
        ));
        assert_eq!(fqcns(&r), vec!["A\\B".to_string()]);
    }
}
