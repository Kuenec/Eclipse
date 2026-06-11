//! JNI native-symbol demangling — the deterministic, unit-tested core of the pre-loaded-lib `Java_*`
//! discovery-gap fix (2026-06-11).
//!
//! ## The gap this serves
//! When Eclipse pre-loads a JNI lib through its OWN mmap loader (not ART's `dlopen`), the lib is not in
//! ART's `libraries_`, so ART's lazy native resolution (`dlsym(handle, "Java_…")`) can never find the
//! lib's `Java_*` exports → `UnsatisfiedLinkError` (observed on the real v2.721.1108 boot:
//! `com.roblox.client.JNIAAssetManagerSetup.initNative`, `…universalapp.logging.JNILoggingProtocol.nativeGetTimestamp`,
//! `com.github.luben.zstd.ZstdInputStreamNoFinalizer.recommendedDInSize`). The fix enumerates each
//! pre-loaded lib's exported `Java_*` symbols and `RegisterNatives` them with ART; THIS module is the
//! deterministic half — turning an exported symbol name back into the `(class, method)` it binds.
//!
//! ## The mangling (JNI spec, "Resolving Native Method Names"), reversed
//! Short form `Java_<class>_<method>`, overloaded long form `Java_<class>_<method>__<arg-sig>`. In the
//! class/method part a plain `_` is a separator (package `/` or the class↔method boundary) and the
//! escapes are `_1`→`_`, `_2`→`;`, `_3`→`[`, `_0XXXX`→the UTF-16 code unit `XXXX` (so a nested class
//! `A$B` mangles as `A_00024B`). A `__` after the method begins the (also-mangled, args-only) overload
//! signature. The return type is NEVER encoded — the full JNI signature still needs the declared method
//! (reflection), which the live-wiring half does; this module's job is the unambiguous `(class, method)`.
//!
//! Pure and total: `#![forbid(unsafe_code)]`, every malformed name → `None`, never a panic (it is fed an
//! untrusted symbol table).

#![forbid(unsafe_code)]

/// A demangled `Java_*` native symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemangledNative {
    /// The declaring class's binary name with `/` package separators, e.g.
    /// `com/roblox/client/JNIAAssetManagerSetup`.
    pub class: String,
    /// The native method name, e.g. `initNative`.
    pub method: String,
    /// For the overloaded long form `Java_…__<args>`, the decoded JNI argument descriptors **without**
    /// the enclosing `(`/`)` or the return type, e.g. `Landroid/content/res/AssetManager;`. `None` for
    /// the short (non-overloaded) form. The return type is never in the symbol, so building the full
    /// JNI signature still requires reflecting the declared method.
    pub overload_args: Option<String>,
}

const PREFIX: &str = "Java_";

/// Reverse the JNI name-mangling of an exported `Java_*` native symbol into the `(class, method)` it
/// binds (plus the overload arg-sig fragment for the long form). Returns `None` for any name that is not
/// a well-formed `Java_*` mangling — never panics.
#[must_use]
pub fn demangle(symbol: &str) -> Option<DemangledNative> {
    let body = symbol.strip_prefix(PREFIX)?;
    let (name_part, overload_part) = split_overload(body);

    let components = decode_components(name_part)?;
    // Need at least a class component + a method component, both non-empty.
    if components.len() < 2 {
        return None;
    }
    let method = components.last()?.clone();
    if method.is_empty() {
        return None;
    }
    let class = components[..components.len() - 1].join("/");
    if class.is_empty()
        || components[..components.len() - 1]
            .iter()
            .any(String::is_empty)
    {
        return None;
    }

    let overload_args = match overload_part {
        Some(args) => Some(decode_components(args)?.join("/")),
        None => None,
    };
    Some(DemangledNative {
        class,
        method,
        overload_args,
    })
}

/// Split the body (after `Java_`) at the FIRST overload `__` boundary: a separator `_` immediately
/// followed by another `_` that is not an escape introducer. Escapes (`_1`/`_2`/`_3`/`_0XXXX`) are
/// skipped so an escaped underscore is never mistaken for the boundary. Returns `(name_part, None)` for
/// the short form. Slices are on ASCII byte boundaries (mangled names are ASCII).
fn split_overload(body: &str) -> (&str, Option<&str>) {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' {
            match b.get(i + 1) {
                // `_0XXXX` (6 bytes) / `_1`,`_2`,`_3` (2 bytes): an escape — skip it, not a boundary.
                Some(b'0') => i += 6,
                Some(b'1' | b'2' | b'3') => i += 2,
                // `__`: the overload boundary.
                Some(b'_') => return (&body[..i], Some(&body[i + 2..])),
                // a lone separator `_`, or `_` at end of string.
                _ => i += 1,
            }
        } else {
            i += 1;
        }
    }
    (body, None)
}

/// Decode a mangled class/method (or arg-sig) part into its `_`-separated components, each with its
/// `_1`/`_2`/`_3`/`_0XXXX` escapes resolved. `None` on a malformed `_0` escape (short/non-hex).
fn decode_components(s: &str) -> Option<Vec<String>> {
    let b = s.as_bytes();
    let mut comps: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' {
            match b.get(i + 1) {
                Some(b'1') => {
                    cur.push('_');
                    i += 2;
                }
                Some(b'2') => {
                    cur.push(';');
                    i += 2;
                }
                Some(b'3') => {
                    cur.push('[');
                    i += 2;
                }
                Some(b'0') => {
                    // `_0XXXX` → the UTF-16 code unit XXXX (4 hex digits).
                    let hex = s.get(i + 2..i + 6)?;
                    let cp = u32::from_str_radix(hex, 16).ok()?;
                    cur.push(char::from_u32(cp)?);
                    i += 6;
                }
                // separator (a lone `_`, or `_` at the very end → a trailing empty component).
                _ => {
                    comps.push(std::mem::take(&mut cur));
                    i += 1;
                }
            }
        } else {
            // Mangled identifiers are ASCII (non-ASCII is `_0XXXX`-escaped); push the literal byte.
            cur.push(b[i] as char);
            i += 1;
        }
    }
    comps.push(cur);
    Some(comps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dm(class: &str, method: &str) -> Option<DemangledNative> {
        Some(DemangledNative {
            class: class.to_string(),
            method: method.to_string(),
            overload_args: None,
        })
    }

    #[test]
    fn short_form_real_blocking_natives() {
        // The exact symbols ART reported missing on the real v2.721.1108 boot.
        assert_eq!(
            demangle("Java_com_roblox_client_JNIAAssetManagerSetup_initNative"),
            dm("com/roblox/client/JNIAAssetManagerSetup", "initNative"),
        );
        assert_eq!(
            demangle("Java_com_roblox_universalapp_logging_JNILoggingProtocol_nativeGetTimestamp"),
            dm(
                "com/roblox/universalapp/logging/JNILoggingProtocol",
                "nativeGetTimestamp",
            ),
        );
        assert_eq!(
            demangle("Java_com_github_luben_zstd_ZstdInputStreamNoFinalizer_recommendedDInSize"),
            dm(
                "com/github/luben/zstd/ZstdInputStreamNoFinalizer",
                "recommendedDInSize",
            ),
        );
    }

    #[test]
    fn escaped_underscore_in_method_name() {
        // method `do_thing` → `do_1thing`; the `_1` is an escaped `_`, not a separator.
        assert_eq!(demangle("Java_a_b_C_do_1thing"), dm("a/b/C", "do_thing"));
    }

    #[test]
    fn nested_class_dollar_escape() {
        // nested class `Outer$Inner` → `Outer_00024Inner` ($ = U+0024).
        assert_eq!(
            demangle("Java_a_b_Outer_00024Inner_m"),
            dm("a/b/Outer$Inner", "m"),
        );
    }

    #[test]
    fn overloaded_long_form_decodes_arg_sig() {
        // `m(android.content.res.AssetManager)` overloaded form: args after `__`.
        let d = demangle("Java_a_b_C_m__Landroid_content_res_AssetManager_2").expect("demangles");
        assert_eq!(d.class, "a/b/C");
        assert_eq!(d.method, "m");
        assert_eq!(
            d.overload_args.as_deref(),
            Some("Landroid/content/res/AssetManager;")
        );
    }

    #[test]
    fn overloaded_array_and_primitive_args() {
        // `m(String, long, Object[])` → args `Ljava/lang/String;J[Ljava/lang/Object;`
        // mangled: L java_lang_String _2 J _3 L java_lang_Object _2
        let d =
            demangle("Java_a_C_m__Ljava_lang_String_2J_3Ljava_lang_Object_2").expect("demangles");
        assert_eq!(d.class, "a/C");
        assert_eq!(d.method, "m");
        assert_eq!(
            d.overload_args.as_deref(),
            Some("Ljava/lang/String;J[Ljava/lang/Object;")
        );
    }

    #[test]
    fn rejects_non_java_and_degenerate() {
        assert_eq!(demangle("JNI_OnLoad"), None); // not a Java_* native
        assert_eq!(demangle("Java_"), None); // no components
        assert_eq!(demangle("Java_singletoken"), None); // only a class, no method
        assert_eq!(demangle("Java_a__b"), None); // empty class component before `__` overload
        assert_eq!(demangle("Java_a_b_"), None); // trailing separator → empty method
    }

    #[test]
    fn malformed_unicode_escape_is_none_not_panic() {
        assert_eq!(demangle("Java_a_b_0zz"), None); // `_0zz` short/non-hex → total, no panic
        assert_eq!(demangle("Java_a_C_m_0"), None); // truncated `_0` escape
    }

    #[test]
    fn total_over_arbitrary_input_never_panics() {
        // Feeding hostile/garbage names must always return a Result-shaped Option, never panic.
        for s in [
            "",
            "_",
            "__",
            "Java__",
            "Java_a_b_00",
            "Java_\u{00e9}_m",
            "Java_a_b_c_d_e_f_g",
        ] {
            let _ = demangle(s);
        }
    }
}
