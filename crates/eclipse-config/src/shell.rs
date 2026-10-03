#[must_use]
pub fn word(text: &str) -> String {
    const PLAIN: &[u8] = b"/._-+,:@%=";
    let plain = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || PLAIN.contains(&byte));
    if plain {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}
