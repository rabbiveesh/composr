//! PHP-compatible `json_encode` for the byte shapes Composer produces.
//!
//! Differences from `serde_json`'s defaults that matter:
//!   - `/` is escaped as `\/` (no JSON_UNESCAPED_SLASHES)
//!   - non-ASCII becomes `\uXXXX`, with surrogate pairs for supplementary
//!     plane (no JSON_UNESCAPED_UNICODE)
//!   - empty PHP-objects (which `json_decode('{}', true)` turns into an
//!     empty associative array) re-encode as `[]`
//!   - JSON_PRETTY_PRINT: 4-space indent per depth, `: ` after object keys,
//!     no whitespace inside empty containers, no trailing newline
//!
//! Two encoder shapes are exposed:
//!   - default (`encode` / `encode_into`) — compact, used for the
//!     composer.json content-hash
//!   - JSON_PRETTY_PRINT (`encode_pretty`) — used for vendor/pest-plugins.json

use serde_json::Value;
use std::fmt::Write;

#[derive(Debug, Default, Clone, Copy)]
pub struct EncodeOptions {
    pub pretty: bool,
}

/// Encode `v` with PHP's default `json_encode($x, JSON_THROW_ON_ERROR)` shape.
pub fn encode(v: &Value) -> String {
    let mut out = String::new();
    encode_into(v, &mut out, EncodeOptions::default());
    out
}

/// Encode `v` matching PHP's `json_encode($x, JSON_PRETTY_PRINT)` byte shape.
pub fn encode_pretty(v: &Value) -> String {
    let mut out = String::new();
    encode_into(v, &mut out, EncodeOptions { pretty: true });
    out
}

/// Append-encode `v` into `out` using `opts`. Public so callers that build
/// content incrementally (e.g. content-hash buffers sized up-front) can avoid
/// the intermediate `String` from [`encode`].
pub fn encode_into(v: &Value, out: &mut String, opts: EncodeOptions) {
    write_value(v, out, opts, 0);
}

/// Encode a string the way PHP's `json_encode` does with options=0:
/// double-quoted, with `\/`, `\u00XX` for control chars, named escapes
/// for `\b\t\n\f\r"`, and `\uXXXX` for any code point above 0x7F
/// (surrogate pair for supplementary-plane).
pub fn encode_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\x08' => out.push_str("\\b"),
            '\x09' => out.push_str("\\t"),
            '\x0a' => out.push_str("\\n"),
            '\x0c' => out.push_str("\\f"),
            '\x0d' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x80 => {
                out.push(c);
            }
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    let _ = write!(out, "\\u{:04x}", cp);
                } else {
                    // UTF-16 surrogate pair (PHP json_encode does this).
                    let v = cp - 0x10000;
                    let high = 0xD800 + (v >> 10);
                    let low = 0xDC00 + (v & 0x3FF);
                    let _ = write!(out, "\\u{:04x}\\u{:04x}", high, low);
                }
            }
        }
    }
    out.push('"');
}

fn write_value(v: &Value, out: &mut String, opts: EncodeOptions, depth: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            // serde_json's Display matches PHP's `json_encode` for the
            // representations we encounter (small integers, rare floats).
            out.push_str(&n.to_string());
        }
        Value::String(s) => encode_string(s, out),
        Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if opts.pretty {
                    out.push('\n');
                    push_indent(out, depth + 1);
                }
                write_value(item, out, opts, depth + 1);
            }
            if opts.pretty {
                out.push('\n');
                push_indent(out, depth);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // PHP gotcha: `json_decode('{}', true)` produces an empty
            // associative array `[]`, and `json_encode([])` emits `[]`,
            // not `{}`. So the canonical PHP round-trip turns every
            // empty object into an empty array. composer.json hits this
            // on shapes like `extra: { "laravel": {} }`.
            if map.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if opts.pretty {
                    out.push('\n');
                    push_indent(out, depth + 1);
                }
                encode_string(k, out);
                out.push(':');
                if opts.pretty {
                    out.push(' ');
                }
                write_value(val, out, opts, depth + 1);
            }
            if opts.pretty {
                out.push('\n');
                push_indent(out, depth);
            }
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("    ");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_string_like_php_json_encode() {
        let mut out = String::new();
        encode_string("hello", &mut out);
        assert_eq!(out, "\"hello\"");

        let mut out = String::new();
        encode_string("https://example.com/x", &mut out);
        // `/` escaped as `\/`, matches PHP default.
        assert_eq!(out, "\"https:\\/\\/example.com\\/x\"");

        let mut out = String::new();
        encode_string("ñ", &mut out);
        // U+00F1 → ñ (lowercase hex).
        assert_eq!(out, "\"\\u00f1\"");

        let mut out = String::new();
        encode_string("\"\\\n\t", &mut out);
        assert_eq!(out, "\"\\\"\\\\\\n\\t\"");

        // Supplementary plane → surrogate pair. U+1F600 = 😀.
        let mut out = String::new();
        encode_string("😀", &mut out);
        assert_eq!(out, "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn compact_object_matches_php_default() {
        let v = serde_json::json!({ "a": 1, "b": [true, false, null] });
        // Compact, no whitespace, no trailing newline. Mirrors
        // `json_encode($x, JSON_THROW_ON_ERROR)`.
        assert_eq!(encode(&v), r#"{"a":1,"b":[true,false,null]}"#);
    }

    #[test]
    fn compact_empty_object_becomes_empty_array() {
        let v = serde_json::json!({ "k": {} });
        assert_eq!(encode(&v), r#"{"k":[]}"#);
    }

    #[test]
    fn pretty_empty_array() {
        // PHP: `json_encode([], JSON_PRETTY_PRINT)` => `[]`
        assert_eq!(encode_pretty(&serde_json::json!([])), "[]");
    }

    #[test]
    fn pretty_string_array_matches_php() {
        // Verified against PHP 8.5:
        //   php -r 'echo json_encode(["Pest\\Plugins\\Cache","Pest\\Arch\\Plugin"], JSON_PRETTY_PRINT);'
        let v = serde_json::json!(["Pest\\Plugins\\Cache", "Pest\\Arch\\Plugin"]);
        assert_eq!(
            encode_pretty(&v),
            "[\n    \"Pest\\\\Plugins\\\\Cache\",\n    \"Pest\\\\Arch\\\\Plugin\"\n]"
        );
    }

    #[test]
    fn pretty_slash_and_backslash_escape() {
        // Verified against PHP 8.5:
        //   php -r 'echo json_encode(["a/b","x\\y"], JSON_PRETTY_PRINT);'
        let v = serde_json::json!(["a/b", "x\\y"]);
        assert_eq!(encode_pretty(&v), "[\n    \"a\\/b\",\n    \"x\\\\y\"\n]");
    }

    #[test]
    fn pretty_object_with_nested_array_matches_php() {
        // Verified against PHP 8.5:
        //   php -r 'echo json_encode(["a"=>1,"b"=>["x","y"]], JSON_PRETTY_PRINT);'
        let v = serde_json::json!({ "a": 1, "b": ["x", "y"] });
        assert_eq!(
            encode_pretty(&v),
            "{\n    \"a\": 1,\n    \"b\": [\n        \"x\",\n        \"y\"\n    ]\n}"
        );
    }

    #[test]
    fn pretty_empty_object_becomes_empty_array() {
        // Same PHP-ism as compact: empty object encodes as `[]`.
        let v = serde_json::json!({ "k": {} });
        assert_eq!(encode_pretty(&v), "{\n    \"k\": []\n}");
    }
}
