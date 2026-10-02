pub(crate) const MESSAGE_HANDLER: &str = "eclipseBridge";

fn js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn interface_script(name: &str, methods: &[String]) -> String {
    let name = js_string(name);
    let methods = methods
        .iter()
        .map(|method| js_string(method))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "(function(){{var h=window.webkit&&window.webkit.messageHandlers&&\
         window.webkit.messageHandlers.{MESSAGE_HANDLER};if(!h){{return;}}var o={{}};\
         [{methods}].forEach(function(m){{o[m]=function(){{return h.postMessage(\
         JSON.stringify({{iface:{name},method:m,args:Array.prototype.slice.call(arguments)}}));}};}});\
         Object.defineProperty(window,{name},{{value:o,configurable:true,writable:true}});}})();"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interface_script_posts_json_calls_to_the_eclipse_handler() {
        let script = interface_script(
            "__globalRobloxAndroidBridge__",
            &["executeRoblox".to_string()],
        );
        assert_eq!(
            script,
            "(function(){var h=window.webkit&&window.webkit.messageHandlers&&\
             window.webkit.messageHandlers.eclipseBridge;if(!h){return;}var o={};\
             [\"executeRoblox\"].forEach(function(m){o[m]=function(){return h.postMessage(\
             JSON.stringify({iface:\"__globalRobloxAndroidBridge__\",method:m,\
             args:Array.prototype.slice.call(arguments)}));};});\
             Object.defineProperty(window,\"__globalRobloxAndroidBridge__\",\
             {value:o,configurable:true,writable:true});})();"
        );
    }

    #[test]
    fn interface_and_method_names_are_quoted_as_javascript_strings() {
        let script = interface_script("a\"b\\c\u{2028}", &["m\n".to_string()]);
        assert!(script.contains("iface:\"a\\\"b\\\\c\\u2028\""), "{script}");
        assert!(script.contains("[\"m\\u000a\"]"), "{script}");
    }
}
