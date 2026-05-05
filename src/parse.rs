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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub fqcn: String,
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
        let lang = tree_sitter_php::language_php();
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
            // Find the next `name` token in the flat stream.
            let mut j = i + 1;
            while j < flat.len() && flat[j].kind() != "name" {
                j += 1;
            }
            if j < flat.len() {
                let name = node_text(&flat[j], src).trim().to_string();
                if !name.is_empty() {
                    let fqcn = if ns.is_empty() {
                        name
                    } else {
                        format!("{}\\{}", ns, name)
                    };
                    out.push(Declaration { fqcn, kind });
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

fn emit_decl(node: &Node, src: &[u8], ns: &str, kind: DeclKind, out: &mut Vec<Declaration>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        // Anonymous classes don't have a name field at this position; even
        // if they did, anonymous classes appear inside object_creation_expression,
        // not as siblings of class_declaration.
        return;
    };
    let name = node_text(&name_node, src).trim().to_string();
    if name.is_empty() {
        return;
    }
    let fqcn = if ns.is_empty() {
        name
    } else {
        format!("{}\\{}", ns, name)
    };
    out.push(Declaration { fqcn, kind });
}

fn node_text(node: &Node, src: &[u8]) -> String {
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

    #[test]
    fn file_level_namespace() {
        let r = extract("<?php\nnamespace Foo\\Bar;\nclass Baz {}\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].fqcn, "Foo\\Bar\\Baz");
        assert_eq!(r[0].kind, DeclKind::Class);
    }

    #[test]
    fn braced_namespace() {
        let r = extract("<?php\nnamespace Foo { class A {} }\nnamespace Bar { class B {} }\n");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].fqcn, "Foo\\A");
        assert_eq!(r[1].fqcn, "Bar\\B");
    }

    #[test]
    fn no_namespace() {
        let r = extract("<?php\nclass GlobalCls {}\n");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].fqcn, "GlobalCls");
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
        let names: Vec<_> = r.iter().map(|d| d.fqcn.as_str()).collect();
        assert_eq!(names, vec!["N\\Outside"]);
    }

    #[test]
    fn finds_class_inside_if_block() {
        // Symfony polyfills wrap stub classes in version checks. Composer's
        // classmap includes them; ours must too.
        let r = extract(
            "<?php\nif (PHP_VERSION_ID < 80300) {\n    class DateError extends Error {}\n}\n",
        );
        let names: Vec<_> = r.iter().map(|d| d.fqcn.as_str()).collect();
        assert_eq!(names, vec!["DateError"]);
    }

    #[test]
    fn finds_class_inside_nested_if_else() {
        let r = extract(
            "<?php\nnamespace G;\nif (false) {\n    class A {}\n} else {\n    class B {}\n}\n",
        );
        let mut names: Vec<_> = r.iter().map(|d| d.fqcn.as_str()).collect();
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
        let names: Vec<_> = r.iter().map(|d| d.fqcn.as_str()).collect();
        assert_eq!(names, vec!["S\\Requirement"]);
        assert_eq!(r[0].kind, DeclKind::Enum);
    }

    #[test]
    fn recovers_backed_enum_with_complex_body() {
        let r = extract(
            "<?php\nnamespace U;\nenum Operator: string\n{\n    private const X = 'x';\n    case A = 'a';\n}\n",
        );
        let names: Vec<_> = r.iter().map(|d| d.fqcn.as_str()).collect();
        assert!(names.contains(&"U\\Operator"));
    }
}
