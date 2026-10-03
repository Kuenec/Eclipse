use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::error::Category;
use serde_json::value::RawValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    fn of_offset(text: &[u8], offset: usize) -> Self {
        let before = &text[..offset];
        let line_start = before
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |newline| newline + 1);
        Self {
            line: 1 + before.iter().filter(|byte| **byte == b'\n').count(),
            column: offset - line_start + 1,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

pub(crate) struct Document<'a> {
    text: &'a str,
    pub(crate) entries: Vec<(String, &'a RawValue)>,
}

pub(crate) enum Malformed {
    NotUtf8(Position),
    Syntax { at: Position, reason: String },
    NotAnObject,
}

impl Malformed {
    fn from_parser(error: serde_json::Error) -> Self {
        match error.classify() {
            Category::Data => Self::NotAnObject,
            Category::Syntax | Category::Eof | Category::Io => Self::Syntax {
                at: Position {
                    line: error.line(),
                    column: error.column().max(1),
                },
                reason: reason(&error),
            },
        }
    }
}

impl<'a> Document<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, Malformed> {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| Malformed::NotUtf8(Position::of_offset(bytes, error.valid_up_to())))?;
        let Entries(entries) = serde_json::from_str(text).map_err(Malformed::from_parser)?;
        Ok(Self { text, entries })
    }

    pub(crate) fn position(&self, value: &RawValue) -> Position {
        let offset = value.get().as_ptr().addr() - self.text.as_ptr().addr();
        Position::of_offset(self.text.as_bytes(), offset)
    }
}

pub(crate) fn reason(error: &serde_json::Error) -> String {
    let text = error.to_string();
    let position = format!(" at line {} column {}", error.line(), error.column());
    match text.strip_suffix(&position) {
        Some(reason) => reason.to_owned(),
        None => text,
    }
}

struct Entries<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for Entries<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(EntriesVisitor)
    }
}

struct EntriesVisitor;

impl<'de> Visitor<'de> for EntriesVisitor {
    type Value = Entries<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries = Vec::new();
        while let Some(entry) = map.next_entry()? {
            entries.push(entry);
        }
        Ok(Entries(entries))
    }
}
