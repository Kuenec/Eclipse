use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};

use eclipse::diagnostics::{self, record_head, STATUS_TARGET, SUPERVISOR_TARGET};
use eclipse::webview::redact::{url_scheme_and_host_for_log, url_spans, Schemes};
use tracing::Level;

use crate::desktop_integration::Packaging;
use crate::doctor::Doctor;

const REPORT_BYTES: usize = 60_000;
const RECORD_BYTES: usize = 400;
const HEAD_RECORDS: usize = 40;
const MIDDLE_RECORDS: usize = 60;
const TAIL_RECORDS: usize = 200;
const CUT: &str = "…";
const REDACTED: &str = "<redacted>";
const QUERY: &str = "<query>";
const USER_PLACEHOLDER: &str = "<user>";
const HOME_PLACEHOLDER: &str = "~";
const FENCE: &str = "```";
const QUOTES: [&str; 3] = ["\\\"", "\"", "'"];
const ESCAPED_SEPARATORS: [(&str, &str); 3] =
    [("\\u003d", "="), ("\\u003D", "="), ("\\u0026", "&")];
const ROBLOSECURITY: &str = "_|WARNING:-DO-NOT-SHARE-THIS";
const SECRET_HEADERS: [&str; 2] = ["cookie:", "authorization:"];
const JWT_START: &str = "eyJ";
const GOOGLE_TOKENS: [&str; 3] = ["oauth2_4/", "aas_et/", "ya29."];
const SECRET_KEYS: [&str; 18] = [
    "ticket",
    "token",
    "accesscode",
    "linkcode",
    "password",
    "secret",
    "sessionid",
    "cookie",
    "userid",
    "user_id",
    "playerid",
    "username",
    "displayname",
    "mdid",
    "idfv",
    "idfa",
    "androidid",
    "deviceid",
];

pub(crate) fn report(doctor: &Doctor, run_log: Option<&Path>) -> Result<String, String> {
    let redactor = Redactor::for_this_user();
    let run = match (run_log, doctor.log_dir()) {
        (Some(path), Ok(dir)) => Some(diagnostics::run_head_in(dir, path).ok_or_else(|| {
            format!(
                "{} is not one of Eclipse's run logs in {}",
                path.display(),
                dir.display()
            )
        })?),
        (Some(_), Err(error)) => return Err(error.to_owned()),
        (None, Ok(dir)) => diagnostics::kept_runs(dir)
            .map_err(|error| error.to_string())?
            .into_iter()
            .next()
            .map(|run| run.head),
        (None, Err(_)) => None,
    };
    let mut excerpt = Excerpt::default();
    let mut missing = Some("no launch is logged yet".to_owned());
    let problems = doctor.fixes();
    let intro = match &run {
        Some(head) => {
            let outcome = match diagnostics::run_outcome(head) {
                Ok(Some(outcome)) => outcome,
                Ok(None) => "not recorded; the launch may still be running".to_owned(),
                Err(error) => format!("unknown, because {error}"),
            };
            let parts = diagnostics::run_parts(head);
            match diagnostics::read_records(&parts, |record| excerpt.take(record, &redactor)) {
                Ok(()) => missing = None,
                Err(error) => {
                    excerpt = Excerpt::default();
                    missing = Some(error.to_string());
                }
            }
            intro(&outcome, &problems, &head.display().to_string())
        }
        None => {
            let place = match doctor.log_dir() {
                Ok(dir) => dir.display().to_string(),
                Err(error) => error.to_owned(),
            };
            intro(
                "no launch is logged yet",
                &problems,
                &format!("none in {place}"),
            )
        }
    };
    let heading = |excerpt: &Excerpt| {
        let heading = match &missing {
            Some(reason) => format!("Log excerpt: none, because {reason}"),
            None => excerpt.heading(),
        };
        redactor.text(&heading)
    };
    let mut report = format!(
        "{}\n{FENCE}text\n{}",
        redactor.text(&intro),
        redactor.text(&doctor.to_string())
    );
    let closing = heading(&excerpt).len() + 1 + FENCE.len() + 1;
    excerpt.fit(REPORT_BYTES.saturating_sub(report.len() + closing));
    report.push_str(&heading(&excerpt));
    report.push('\n');
    let _ = excerpt.write_records(&mut report);
    report.push_str(FENCE);
    report.push('\n');
    Ok(report)
}

pub(crate) fn unfinished(outcome: &str, log: &Path, reason: &str, packaging: &Packaging) -> String {
    let text = format!(
        "{}\n{reason}, so this report holds only how Roblox ended. `{}` prints the whole \
         report.\n",
        intro(outcome, &[], &log.display().to_string()),
        packaging.command("doctor --report")
    );
    Redactor::for_this_user().text(&text)
}

fn intro(outcome: &str, problems: &[String], log: &str) -> String {
    let mut intro = format!("Outcome: {outcome}\n");
    for problem in problems {
        intro.push_str("Problem: ");
        intro.push_str(problem);
        intro.push('\n');
    }
    intro.push_str("Log: ");
    intro.push_str(log);
    intro.push('\n');
    intro
}

struct Redactor {
    homes: Vec<String>,
    user: Option<String>,
}

impl Redactor {
    fn for_this_user() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.parent().is_some());
        let mut homes: Vec<String> = home
            .iter()
            .flat_map(|home| [Some(home.clone()), home.canonicalize().ok()])
            .flatten()
            .filter_map(|home| Some(home.to_str()?.trim_end_matches('/').to_owned()))
            .filter(|home| !home.is_empty())
            .collect();
        homes.sort_by_key(|home| std::cmp::Reverse(home.len()));
        homes.dedup();
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .or_else(|| Some(home?.file_name()?.to_str()?.to_owned()))
            .filter(|user| !user.is_empty() && !user.contains('/'));
        Self { homes, user }
    }

    fn text(&self, text: &str) -> String {
        let mut redacted = String::with_capacity(text.len());
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                redacted.push('\n');
            }
            redacted.push_str(&self.line(line));
        }
        redacted
    }

    fn line(&self, line: &str) -> String {
        let mut line = line.to_owned();
        for (escaped, separator) in ESCAPED_SEPARATORS {
            if line.contains(escaped) {
                line = line.replace(escaped, separator);
            }
        }
        let line = redact_roblosecurity(&line);
        let line = redact_secret_headers(&line);
        let line = redact_jwts(&line);
        let line = redact_google_tokens(&line);
        let line = reduce_urls_and_queries(&line);
        let mut line = redact_key_values(&line).into_owned();
        for home in &self.homes {
            line = replace_path(&line, home, HOME_PLACEHOLDER).into_owned();
        }
        if let Some(user) = &self.user {
            line = replace_path(&line, &format!("/{user}"), &format!("/{USER_PLACEHOLDER}"))
                .into_owned();
        }
        line
    }
}

fn ends_token(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '\'' | '&' | ',' | ';' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

fn token_end(text: &str, start: usize) -> usize {
    text[start..]
        .find(ends_token)
        .map_or(text.len(), |length| start + length)
}

fn redact_roblosecurity(line: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut copied = 0;
    while let Some(offset) = line[copied..].find(ROBLOSECURITY) {
        let start = copied + offset;
        redacted.push_str(&line[copied..start]);
        redacted.push_str(REDACTED);
        copied = token_end(line, start);
    }
    finish(line, redacted, copied)
}

fn finish(line: &str, mut redacted: String, copied: usize) -> Cow<'_, str> {
    if copied == 0 {
        return Cow::Borrowed(line);
    }
    redacted.push_str(&line[copied..]);
    Cow::Owned(redacted)
}

fn redact_secret_headers(line: &str) -> Cow<'_, str> {
    let lowered = line.to_ascii_lowercase();
    let Some(value_start) = SECRET_HEADERS
        .iter()
        .filter_map(|header| Some(lowered.find(header)? + header.len()))
        .min()
    else {
        return Cow::Borrowed(line);
    };
    if line[value_start..].trim().is_empty() {
        return Cow::Borrowed(line);
    }
    Cow::Owned(format!("{} {REDACTED}", &line[..value_start]))
}

fn is_base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn jwt_end(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut end = start;
    for part in 0..3 {
        let length = bytes[end..]
            .iter()
            .take_while(|&&byte| is_base64url(byte))
            .count();
        end += length;
        if part == 2 {
            return Some(end);
        }
        if length == 0 || bytes.get(end) != Some(&b'.') {
            return None;
        }
        end += 1;
    }
    None
}

fn redact_jwts(line: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(offset) = line[cursor..].find(JWT_START) {
        let start = cursor + offset;
        cursor = start + JWT_START.len();
        let Some(end) = jwt_end(line, start) else {
            continue;
        };
        redacted.push_str(&line[copied..start]);
        redacted.push_str(REDACTED);
        copied = end;
        cursor = end;
    }
    finish(line, redacted, copied)
}

fn redact_google_tokens(line: &str) -> Cow<'_, str> {
    let mut line = Cow::Borrowed(line);
    for prefix in GOOGLE_TOKENS {
        let mut redacted = String::new();
        let mut copied = 0;
        let mut cursor = 0;
        while let Some(offset) = line[cursor..].find(prefix) {
            let value_start = cursor + offset + prefix.len();
            let end = token_end(&line, value_start);
            cursor = end.max(value_start);
            if end == value_start || line[value_start..].starts_with(REDACTED) {
                continue;
            }
            redacted.push_str(&line[copied..value_start]);
            redacted.push_str(REDACTED);
            copied = end;
        }
        if copied > 0 {
            redacted.push_str(&line[copied..]);
            line = Cow::Owned(redacted);
        }
    }
    line
}

fn reduce_urls_and_queries(line: &str) -> Cow<'_, str> {
    let mut reduced = String::with_capacity(line.len());
    let mut copied = 0;
    for span in url_spans(line, Schemes::Any) {
        collapse_queries(&line[copied..span.start], &mut reduced);
        collapse_queries(
            &url_scheme_and_host_for_log(&line[span.clone()]),
            &mut reduced,
        );
        copied = span.end;
    }
    collapse_queries(&line[copied..], &mut reduced);
    if reduced == line {
        return Cow::Borrowed(line);
    }
    Cow::Owned(reduced)
}

fn is_query_key(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
}

fn ends_query_value(character: char) -> bool {
    character.is_whitespace() || matches!(character, '&' | '"' | '\'' | '<' | '>' | ']' | ')')
}

fn pair_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let key = bytes[start..]
        .iter()
        .take_while(|&&byte| is_query_key(byte))
        .count();
    if key == 0 || bytes.get(start + key) != Some(&b'=') {
        return None;
    }
    let value_start = start + key + 1;
    Some(
        text[value_start..]
            .find(ends_query_value)
            .map_or(text.len(), |length| value_start + length),
    )
}

fn collapse_queries(text: &str, out: &mut String) {
    let bytes = text.as_bytes();
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        let key_start =
            is_query_key(bytes[index]) && (index == 0 || !is_query_key(bytes[index - 1]));
        let Some(first) = key_start.then(|| pair_end(text, index)).flatten() else {
            index += 1;
            continue;
        };
        let mut end = first;
        let mut pairs = 1;
        while bytes.get(end) == Some(&b'&') {
            let Some(next) = pair_end(text, end + 1) else {
                break;
            };
            end = next;
            pairs += 1;
        }
        if pairs >= 2 {
            out.push_str(&text[copied..index]);
            out.push_str(QUERY);
            copied = end;
        }
        index = end.max(index + 1);
    }
    out.push_str(&text[copied..]);
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|secret| key.ends_with(secret))
}

fn redact_key_values(line: &str) -> Cow<'_, str> {
    let bytes = line.as_bytes();
    let mut redacted = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(offset) = line[cursor..].find(['=', ':']) {
        let separator = cursor + offset;
        cursor = separator + 1;
        let key_end = separator
            - QUOTES
                .into_iter()
                .find(|quote| line[..separator].ends_with(quote))
                .map_or(0, str::len);
        let key_start = key_end
            - bytes[..key_end]
                .iter()
                .rev()
                .take_while(|&&byte| byte.is_ascii_alphanumeric() || byte == b'_')
                .count();
        if key_start < copied || !is_secret_key(&line[key_start..key_end]) {
            continue;
        }
        let mut value_start = separator + 1;
        value_start += bytes[value_start..]
            .iter()
            .take_while(|&&byte| byte == b' ')
            .count();
        let quote = QUOTES
            .into_iter()
            .find(|quote| line[value_start..].starts_with(quote));
        let end = match quote {
            Some(quote) => {
                value_start += quote.len();
                closing_quote(line, value_start, quote)
            }
            None => token_end(line, value_start),
        };
        if end == value_start || line[value_start..].starts_with(REDACTED) {
            continue;
        }
        redacted.push_str(&line[copied..value_start]);
        redacted.push_str(REDACTED);
        copied = end;
        cursor = end;
    }
    finish(line, redacted, copied)
}

fn closing_quote(text: &str, start: usize, quote: &str) -> usize {
    let bytes = text.as_bytes();
    let mut index = start;
    while index < bytes.len() {
        if bytes[index..].starts_with(quote.as_bytes()) {
            return index;
        }
        index += if bytes[index] == b'\\' { 2 } else { 1 };
    }
    bytes.len()
}

fn is_path_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn replace_path<'a>(line: &'a str, path: &str, replacement: &str) -> Cow<'a, str> {
    let mut replaced = String::new();
    let mut copied = 0;
    for (start, _) in line.match_indices(path) {
        let end = start + path.len();
        if start < copied
            || line
                .as_bytes()
                .get(end)
                .is_some_and(|&byte| is_path_name(byte))
        {
            continue;
        }
        replaced.push_str(&line[copied..start]);
        replaced.push_str(replacement);
        copied = end;
    }
    finish(line, replaced, copied)
}

struct Kept {
    index: usize,
    notable: bool,
    text: String,
}

#[derive(Default)]
struct Excerpt {
    head: Vec<Kept>,
    middle: VecDeque<Kept>,
    tail: VecDeque<Kept>,
    records: usize,
}

impl Excerpt {
    fn take(&mut self, record: &str, redactor: &Redactor) {
        let first_line = record.split('\n').next().unwrap_or(record);
        let notable = record_head(first_line).is_some_and(|head| {
            head.level == Level::ERROR
                || head.target == STATUS_TARGET
                || head.target == SUPERVISOR_TARGET
        });
        let mut text = redactor.text(record);
        if text.len() > RECORD_BYTES {
            text.truncate(text.floor_char_boundary(RECORD_BYTES - CUT.len()));
            text.push_str(CUT);
        }
        let kept = Kept {
            index: self.records,
            notable,
            text,
        };
        self.records += 1;
        if self.head.len() < HEAD_RECORDS {
            self.head.push(kept);
            return;
        }
        self.tail.push_back(kept);
        if self.tail.len() <= TAIL_RECORDS {
            return;
        }
        let Some(left) = self.tail.pop_front().filter(|left| left.notable) else {
            return;
        };
        self.middle.push_back(left);
        if self.middle.len() > MIDDLE_RECORDS {
            self.middle.pop_front();
        }
    }

    fn kept(&self) -> impl Iterator<Item = &Kept> {
        self.head.iter().chain(&self.middle).chain(&self.tail)
    }

    fn heading(&self) -> String {
        format!(
            "Log excerpt: {} of {} records",
            self.kept().count(),
            self.records
        )
    }

    fn write_records(&self, out: &mut impl fmt::Write) -> fmt::Result {
        let mut next = 0;
        for kept in self.kept() {
            if kept.index > next {
                writeln!(out, "[… {} records left out]", kept.index - next)?;
            }
            writeln!(out, "{}", kept.text)?;
            next = kept.index + 1;
        }
        if self.records > next {
            writeln!(out, "[… {} records left out]", self.records - next)?;
        }
        Ok(())
    }

    fn written_bytes(&self) -> usize {
        let mut counter = ByteCount(0);
        let _ = self.write_records(&mut counter);
        counter.0
    }

    fn fit(&mut self, budget: usize) {
        while self.written_bytes() > budget {
            if self.middle.pop_front().is_some() || self.head.pop().is_some() {
                continue;
            }
            if self.tail.pop_front().is_none() {
                return;
            }
        }
    }
}

struct ByteCount(usize);

impl fmt::Write for ByteCount {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor {
            homes: vec!["/var/home/ada".to_owned(), "/home/ada".to_owned()],
            user: Some("ada".to_owned()),
        }
    }

    fn redacted(line: &str) -> String {
        redactor().line(line)
    }

    #[test]
    fn escaped_upload_lines_lose_their_device_and_user_ids() {
        let line =
            "Making upload request [\"evt\\u003dandroidNotificationsSettings\\u0026ctx\\u003d\
                    backgroundTask\\u0026mdid\\u003d6f1c2e9a\\u0026recipient_user_id\\u003d\
                    1505371750\"] tag=\"EventUploadJob\" for placeId 1818";
        assert_eq!(
            redacted(line),
            "Making upload request [\"<query>\"] tag=\"EventUploadJob\" for placeId 1818"
        );
        let plain = redacted(
            "Added event evt=join&mdid=6f1c2e9a&idfa=&recipient_user_id=1505371750 to database",
        );
        assert_eq!(plain, "Added event <query> to database");
    }

    #[test]
    fn the_roblosecurity_cookie_is_removed() {
        let out = redacted(
            "cookie value _|WARNING:-DO-NOT-SHARE-THIS.--Sharing-this-will-allow-someone-to-log-\
             in-as-you-and-to-steal-your-ROBUX-and-items.|_9F1C2B3A; placeId 1818 kept",
        );
        assert_eq!(out, "cookie value <redacted>; placeId 1818 kept");
    }

    #[test]
    fn cookie_and_authorization_headers_are_removed() {
        for (line, expected) in [
            (
                "request Cookie: GuestData=UserID=-1; RBXEventTrackerV2=browserid=99",
                "request Cookie: <redacted>",
            ),
            (
                "response set-cookie: .ROBLOSECURITY=abc; path=/",
                "response set-cookie: <redacted>",
            ),
            ("Authorization: Bearer abc.def", "Authorization: <redacted>"),
            ("Cookie:", "Cookie:"),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    #[test]
    fn jwts_are_removed() {
        let out = redacted(
            "token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxNTA1MzcxNzUwIn0.c2lnbmF0dXJl for place 1818",
        );
        assert_eq!(out, "token <redacted> for place 1818");
        assert_eq!(
            redacted("keyJson eyJ.only one part"),
            "keyJson eyJ.only one part"
        );
    }

    #[test]
    fn google_tokens_are_removed() {
        for (line, expected) in [
            (
                "got oauth2_4/0AVGzR1A-secret in reply",
                "got oauth2_4/<redacted> in reply",
            ),
            (
                "reply aas_et/AKppINb-secret== for Play",
                "reply aas_et/<redacted> for Play",
            ),
            (
                "bearer ya29.a0AfB_byC-secret, then 1818",
                "bearer ya29.<redacted>, then 1818",
            ),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    #[test]
    fn an_unfinished_report_redacts_the_outcome_and_says_why_it_is_short() {
        let report = unfinished(
            "Roblox crashed (signal 11, SIGSEGV: invalid memory access)\nLast error: cannot \
             reach https://apkcombo.com/r?ticket=9F1C",
            Path::new("/srv/eclipse/logs/eclipse-20261003T091434.289Z.log"),
            "Eclipse's setup check did not finish within 10s",
            &Packaging::Flatpak {
                app_id: "io.github.kuenec.Eclipse".to_owned(),
            },
        );

        assert_eq!(
            report,
            "Outcome: Roblox crashed (signal 11, SIGSEGV: invalid memory access)\n\
             Last error: cannot reach https://apkcombo.com\n\
             Log: /srv/eclipse/logs/eclipse-20261003T091434.289Z.log\n\
             \n\
             Eclipse's setup check did not finish within 10s, so this report holds only how \
             Roblox ended. `flatpak run io.github.kuenec.Eclipse doctor --report` prints the \
             whole report.\n"
        );
        let on_host = unfinished(
            "Roblox crashed",
            Path::new("/srv/eclipse/logs/eclipse-20261003T091434.289Z.log"),
            "Eclipse's setup check failed (exit status: 3)",
            &Packaging::Host,
        );
        assert!(
            on_host.ends_with("`eclipse doctor --report` prints the whole report.\n"),
            "{on_host}"
        );
    }

    #[test]
    fn the_summary_names_each_problem_doctor_found_between_the_outcome_and_the_log() {
        let problems = [
            "Roblox is not installed. Start Eclipse to download it, or run `eclipse update`."
                .to_owned(),
            "Only 512.0 MiB is free where Roblox is stored (/data/roblox).".to_owned(),
        ];

        let summary = intro(
            "cannot download Roblox: APKCombo did not answer",
            &problems,
            "/data/logs/eclipse-20261003T091434.289Z.log",
        );

        assert_eq!(
            summary,
            "Outcome: cannot download Roblox: APKCombo did not answer\n\
             Problem: Roblox is not installed. Start Eclipse to download it, or run `eclipse \
             update`.\n\
             Problem: Only 512.0 MiB is free where Roblox is stored (/data/roblox).\n\
             Log: /data/logs/eclipse-20261003T091434.289Z.log\n"
        );
        assert_eq!(
            intro("Roblox crashed", &[], "/data/logs/x.log"),
            "Outcome: Roblox crashed\nLog: /data/logs/x.log\n"
        );
    }

    #[test]
    fn urls_keep_only_their_scheme_and_host() {
        let out = redacted(
            "open https://www.roblox.com/games/1818/x?privateServerLinkCode=42&ref=a and \
             content://com.roblox.client.provider/files/ada/pic.png in place 1818",
        );
        assert_eq!(
            out,
            "open https://www.roblox.com and content://com.roblox.client.provider in place 1818"
        );
    }

    #[test]
    fn links_whose_host_is_a_query_lose_the_query() {
        for (line, expected) in [
            (
                "launch roblox://placeId=1818&accessCode=8f3c2a10-5b6d now",
                "launch roblox://<query> now",
            ),
            (
                "[FLog::Error] Asset (Image) \"rbxthumb://type=AvatarHeadShot&id=1505371750&w=48\
                 &h=48&filters=circular\" load failed: Temp read failed.",
                "[FLog::Error] Asset (Image) \"rbxthumb://<query>\" load failed: Temp read \
                 failed.",
            ),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    #[test]
    fn secret_values_are_removed_after_equals_colon_or_json_colon() {
        for (line, expected) in [
            (
                "joined userid:1505371750 place 1818",
                "joined userid:<redacted> place 1818",
            ),
            (
                "{\"UserId\":\"1505371750\",\"placeId\":1818,\"DisplayName\":\"Ada\"}",
                "{\"UserId\":\"<redacted>\",\"placeId\":1818,\"DisplayName\":\"<redacted>\"}",
            ),
            (
                "refresh access_token=abc123 at 09:14:45",
                "refresh access_token=<redacted> at 09:14:45",
            ),
            (
                "Username: ada_plays in universe 42",
                "Username: <redacted> in universe 42",
            ),
            ("sessionId=; placeId=1818", "sessionId=; placeId=1818"),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    #[test]
    fn quoted_secret_values_are_removed_up_to_their_closing_quote() {
        for (line, expected) in [
            (
                "{\"DisplayName\":\"Ada Lovelace\",\"placeId\":1818}",
                "{\"DisplayName\":\"<redacted>\",\"placeId\":1818}",
            ),
            (
                "auth \"password\": \"correct horse battery\" for place 1818",
                "auth \"password\": \"<redacted>\" for place 1818",
            ),
            (
                "Username: \"Ada \\\"the\\\" first\" joined 1818",
                "Username: \"<redacted>\" joined 1818",
            ),
            (
                "body {\\\"DisplayName\\\":\\\"Ada Lovelace\\\",\\\"UserId\\\":\\\"1505371750\\\"}",
                "body {\\\"DisplayName\\\":\\\"<redacted>\\\",\\\"UserId\\\":\\\"<redacted>\\\"}",
            ),
            ("token='abc def' then 1818", "token='<redacted>' then 1818"),
            (
                "{'username': 'Ada Lovelace', 'placeId': 1818}",
                "{'username': '<redacted>', 'placeId': 1818}",
            ),
            ("DisplayName: \"Ada Love", "DisplayName: \"<redacted>"),
            ("{\"DisplayName\":\"\"}", "{\"DisplayName\":\"\"}"),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    #[test]
    fn home_paths_and_the_login_name_are_hidden() {
        for (line, expected) in [
            (
                "cannot read /home/ada/.var/app/io.github.kuenec.Eclipse/data/x.json",
                "cannot read ~/.var/app/io.github.kuenec.Eclipse/data/x.json",
            ),
            ("canonical /var/home/ada/games", "canonical ~/games"),
            (
                "other /run/media/ada/disk and /home/adams/x",
                "other /run/media/<user>/disk and /home/adams/x",
            ),
            ("home /home/ada", "home ~"),
        ] {
            assert_eq!(redacted(line), expected, "{line}");
        }
    }

    fn record(index: usize, level: &str, target: &str) -> String {
        format!("2026-10-03T09:14:46.{index:06}Z {level:>5} {target}: record {index}")
    }

    fn excerpt_of(records: &[String]) -> Excerpt {
        let redactor = redactor();
        let mut excerpt = Excerpt::default();
        for record in records {
            excerpt.take(record, &redactor);
        }
        excerpt
    }

    fn kept_indices(excerpt: &Excerpt) -> Vec<usize> {
        excerpt.kept().map(|kept| kept.index).collect()
    }

    #[test]
    fn the_excerpt_keeps_the_head_notable_middle_records_and_the_tail() {
        let records: Vec<String> = (0..1_000)
            .map(|index| match index {
                100 | 200 => record(index, "ERROR", "liblog"),
                300 => record(index, "WARN", STATUS_TARGET),
                400 => record(index, "INFO", SUPERVISOR_TARGET),
                450 => record(index, "WARN", "liblog"),
                _ => record(index, "INFO", "liblog"),
            })
            .collect();
        let excerpt = excerpt_of(&records);

        let expected: Vec<usize> = (0..40)
            .chain([100, 200, 300, 400])
            .chain(800..1_000)
            .collect();
        assert_eq!(kept_indices(&excerpt), expected);
        assert_eq!(excerpt.heading(), "Log excerpt: 244 of 1000 records");
        let mut text = String::new();
        excerpt.write_records(&mut text).unwrap();
        assert!(
            text.contains("record 39\n[… 60 records left out]\n"),
            "{text}"
        );
        assert!(
            text.contains("record 400\n[… 399 records left out]\n"),
            "{text}"
        );
        assert!(text.ends_with("record 999\n"), "{text}");
    }

    #[test]
    fn the_middle_keeps_only_the_newest_notable_records() {
        let records: Vec<String> = (0..500)
            .map(|index| record(index, "ERROR", "liblog"))
            .collect();
        let excerpt = excerpt_of(&records);
        let expected: Vec<usize> = (0..40).chain(240..300).chain(300..500).collect();
        assert_eq!(kept_indices(&excerpt), expected);
    }

    #[test]
    fn records_spanning_lines_stay_whole_and_long_records_are_cut() {
        let trace = format!(
            "{}\njava.lang.RuntimeException: boom\n    at com.roblox.client.Main.run",
            record(0, "ERROR", "liblog")
        );
        let long = format!("{} {}", record(1, "INFO", "stdout"), "é".repeat(400));
        let excerpt = excerpt_of(&[trace.clone(), long]);
        let texts: Vec<&str> = excerpt.kept().map(|kept| kept.text.as_str()).collect();
        assert_eq!(texts[0], trace);
        assert!(texts[1].len() <= RECORD_BYTES, "{}", texts[1].len());
        assert!(texts[1].ends_with("é…"), "{}", texts[1]);
    }

    #[test]
    fn the_excerpt_fits_the_budget_trimming_the_middle_then_the_head_then_the_oldest_tail() {
        let records: Vec<String> = (0..1_000)
            .map(|index| format!("{} {}", record(index, "ERROR", "liblog"), "x".repeat(300)))
            .collect();
        let mut excerpt = excerpt_of(&records);
        let full = excerpt.written_bytes();

        excerpt.fit(full - 1);
        assert_eq!(excerpt.middle.len(), MIDDLE_RECORDS - 1);
        assert_eq!(excerpt.head.len(), HEAD_RECORDS);

        excerpt.fit(REPORT_BYTES / 2);
        assert!(excerpt.written_bytes() <= REPORT_BYTES / 2);
        assert!(excerpt.middle.is_empty() && excerpt.head.is_empty());
        assert_eq!(excerpt.tail.back().map(|kept| kept.index), Some(999));
        assert!(excerpt.tail.len() < TAIL_RECORDS);
    }

    #[test]
    fn records_are_read_across_the_head_and_tail_files_of_a_run() {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-bug-report-parts-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let head = dir.join("eclipse-20261003T091434.289Z.log");
        let lines = |range: std::ops::Range<usize>| -> String {
            range
                .map(|index| record(index, "INFO", "liblog") + "\n")
                .collect()
        };
        std::fs::write(&head, lines(0..50)).unwrap();
        std::fs::write(
            dir.join("eclipse-20261003T091434.289Z.tail.log.1"),
            lines(50..300),
        )
        .unwrap();
        std::fs::write(
            dir.join("eclipse-20261003T091434.289Z.tail.log"),
            format!("{}    at a continuation line\n", lines(300..400)),
        )
        .unwrap();

        let redactor = redactor();
        let mut excerpt = Excerpt::default();
        let read = diagnostics::read_records(&diagnostics::run_parts(&head), |record| {
            excerpt.take(record, &redactor)
        });
        std::fs::remove_dir_all(&dir).ok();
        read.unwrap();

        let expected: Vec<usize> = (0..40).chain(200..400).collect();
        assert_eq!(kept_indices(&excerpt), expected);
        assert_eq!(
            excerpt.tail.back().map(|kept| kept.text.as_str()),
            Some(
                format!(
                    "{}\n    at a continuation line",
                    record(399, "INFO", "liblog")
                )
                .as_str()
            )
        );
    }
}
