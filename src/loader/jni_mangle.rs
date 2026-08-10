#![forbid(unsafe_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemangledNative {
    pub class: String,

    pub method: String,

    pub overload_args: Option<String>,
}

const PREFIX: &str = "Java_";

#[must_use]
pub fn demangle(symbol: &str) -> Option<DemangledNative> {
    let body = symbol.strip_prefix(PREFIX)?;
    let (name_part, overload_part) = split_overload(body);

    let components = decode_components(name_part)?;

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

fn split_overload(body: &str) -> (&str, Option<&str>) {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' {
            match b.get(i + 1) {
                Some(b'0') => i += 6,
                Some(b'1' | b'2' | b'3') => i += 2,

                Some(b'_') => return (&body[..i], Some(&body[i + 2..])),

                _ => i += 1,
            }
        } else {
            i += 1;
        }
    }
    (body, None)
}

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
                    let hex = s.get(i + 2..i + 6)?;
                    let cp = u32::from_str_radix(hex, 16).ok()?;
                    cur.push(char::from_u32(cp)?);
                    i += 6;
                }

                _ => {
                    comps.push(std::mem::take(&mut cur));
                    i += 1;
                }
            }
        } else {
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
        assert_eq!(demangle("Java_a_b_C_do_1thing"), dm("a/b/C", "do_thing"));
    }

    #[test]
    fn nested_class_dollar_escape() {
        assert_eq!(
            demangle("Java_a_b_Outer_00024Inner_m"),
            dm("a/b/Outer$Inner", "m"),
        );
    }

    #[test]
    fn overloaded_long_form_decodes_arg_sig() {
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
        assert_eq!(demangle("JNI_OnLoad"), None);
        assert_eq!(demangle("Java_"), None);
        assert_eq!(demangle("Java_singletoken"), None);
        assert_eq!(demangle("Java_a__b"), None);
        assert_eq!(demangle("Java_a_b_"), None);
    }

    #[test]
    fn malformed_unicode_escape_is_none_not_panic() {
        assert_eq!(demangle("Java_a_b_0zz"), None);
        assert_eq!(demangle("Java_a_C_m_0"), None);
    }

    #[test]
    fn total_over_arbitrary_input_never_panics() {
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
