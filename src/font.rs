//! Terminal font helper.
//!
//! A terminal programme cannot choose its own font — the face belongs to the
//! terminal emulator. On Windows that usually means Windows Terminal, whose
//! setting lives in `settings.json` under `profiles.defaults.font.face`.
//!
//! That file is JSONC: it has comments and trailing commas. Parsing and
//! re-serialising would delete the user's comments, so the file is edited
//! textually: the byte range of the `face` literal is located by a small
//! structural walk, and only that literal is replaced. The result is walked
//! again before it is allowed anywhere near the disk.

use anyhow::{bail, Context, Result};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};

pub fn locate() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA").ok()?;
        let base = PathBuf::from(local);
        let candidates = [
            base.join("Packages")
                .join("Microsoft.WindowsTerminal_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
            base.join("Packages")
                .join("Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe")
                .join("LocalState")
                .join("settings.json"),
            base.join("Microsoft")
                .join("Windows Terminal")
                .join("settings.json"),
            base.join("Microsoft")
                .join("Windows Terminal Preview")
                .join("settings.json"),
        ];
        candidates.into_iter().find(|p| p.exists())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: Vec<String>,
    pub value: String,
    /// Byte range of the quoted literal, quotes included.
    pub span: Range<usize>,
}

fn is_face_path(p: &[String]) -> bool {
    p.len() == 4 && p[0] == "profiles" && p[1] == "defaults" && p[2] == "font" && p[3] == "face"
}

/// Every string value in the document, with its path and byte span.
pub fn entries(src: &str) -> Result<Vec<Entry>> {
    let mut p = Parser {
        bytes: src.as_bytes(),
        src,
        i: 0,
    };
    let mut out = Vec::new();
    let mut path = Vec::new();
    p.value(&mut path, &mut out, 0)?;
    Ok(out)
}

pub fn current_face(src: &str) -> Result<Option<String>> {
    let (_, body) = split_bom(src);
    Ok(entries(body)?
        .into_iter()
        .find(|e| is_face_path(&e.path))
        .map(|e| e.value))
}

/// Replace just the `face` literal, leaving comments and formatting untouched.
pub fn set_face(src: &str, face: &str) -> Result<String> {
    let (bom, body) = split_bom(src);
    let entry = entries(body)?
        .into_iter()
        .find(|e| is_face_path(&e.path))
        .context(
            "settings.json 里没有 profiles.defaults.font.face；请在 Windows Terminal 设置里\
             手动添加一个字体，再回来用它切换",
        )?;

    let quoted = format!("\"{}\"", escape(face));
    let mut out = String::with_capacity(src.len() + quoted.len());
    out.push_str(bom);
    out.push_str(&body[..entry.span.start]);
    out.push_str(&quoted);
    out.push_str(&body[entry.span.end..]);

    // Walk the result again: a wrong splice would show up as a missing or
    // different value, and then nothing is written.
    match current_face(&out)? {
        Some(v) if v == face => Ok(out),
        _ => bail!("改写后回读不一致，已放弃写入"),
    }
}

/// Windows editors sometimes leave a BOM; the parser starts at `{`.
fn split_bom(src: &str) -> (&str, &str) {
    match src.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", src),
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    src: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.i).copied()
    }

    fn skip_trivia(&mut self) {
        loop {
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.i += 1;
            }
            match (self.peek(), self.bytes.get(self.i + 1).copied()) {
                (Some(b'/'), Some(b'/')) => {
                    while self.peek().is_some() && self.peek() != Some(b'\n') {
                        self.i += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.i += 2;
                    while self.i + 1 < self.bytes.len()
                        && !(self.bytes[self.i] == b'*' && self.bytes[self.i + 1] == b'/')
                    {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.bytes.len());
                }
                _ => return,
            }
        }
    }

    fn value(&mut self, path: &mut Vec<String>, out: &mut Vec<Entry>, depth: usize) -> Result<()> {
        if depth > 64 {
            bail!("settings.json 嵌套过深，放弃解析");
        }
        self.skip_trivia();
        match self.peek() {
            Some(b'{') => {
                self.i += 1;
                loop {
                    self.skip_trivia();
                    match self.peek() {
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(());
                        }
                        Some(b',') => {
                            self.i += 1;
                        }
                        Some(b'"') => {
                            let (key, _) = self.string()?;
                            self.skip_trivia();
                            if self.peek() != Some(b':') {
                                bail!("`{key}` 后面缺少冒号");
                            }
                            self.i += 1;
                            path.push(key);
                            self.value(path, out, depth + 1)?;
                            path.pop();
                        }
                        _ => bail!("对象里出现意外字符（偏移 {}）", self.i),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut idx = 0;
                loop {
                    self.skip_trivia();
                    match self.peek() {
                        Some(b']') => {
                            self.i += 1;
                            return Ok(());
                        }
                        Some(b',') => {
                            self.i += 1;
                        }
                        Some(_) => {
                            path.push(format!("[{idx}]"));
                            self.value(path, out, depth + 1)?;
                            path.pop();
                            idx += 1;
                        }
                        None => bail!("数组没有闭合"),
                    }
                }
            }
            Some(b'"') => {
                let (value, span) = self.string()?;
                out.push(Entry {
                    path: path.clone(),
                    value,
                    span,
                });
                Ok(())
            }
            // Numbers, booleans and nulls are irrelevant to us.
            Some(_) => {
                while let Some(b) = self.peek() {
                    if matches!(b, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' | b'/') {
                        break;
                    }
                    self.i += 1;
                }
                Ok(())
            }
            None => bail!("文件意外结束"),
        }
    }

    fn string(&mut self) -> Result<(String, Range<usize>)> {
        let start = self.i;
        if self.peek() != Some(b'"') {
            bail!("期望字符串（偏移 {}）", self.i);
        }
        self.i += 1;
        let mut escaped = false;
        while let Some(b) = self.peek() {
            self.i += 1;
            if escaped {
                escaped = false;
                continue;
            }
            match b {
                b'\\' => escaped = true,
                b'"' => {
                    let raw = &self.src[start + 1..self.i - 1];
                    return Ok((unescape(raw), start..self.i));
                }
                _ => {}
            }
        }
        bail!("字符串没有闭合")
    }
}

fn unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn escape(face: &str) -> String {
    face.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Face names installed on this machine, or `None` when we cannot tell.
pub fn installed_faces() -> Option<Vec<String>> {
    #[cfg(windows)]
    {
        let mut out: Vec<String> = Vec::new();
        let mut any_ok = false;
        for hive in ["HKCU", "HKLM"] {
            let key = format!(
                r"{}\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
                hive
            );
            // A blocked or missing `reg` must read as "unknown", never as
            // "the font is not installed".
            let Ok(res) = std::process::Command::new("reg")
                .args(["query", &key])
                .output()
            else {
                continue;
            };
            if !res.status.success() {
                continue;
            }
            any_ok = true;
            let text = String::from_utf8_lossy(&res.stdout);
            for line in text.lines() {
                let Some(pos) = line.find("REG_SZ") else {
                    continue;
                };
                let name = line[..pos].trim();
                let name = name
                    .trim_end_matches("(TrueType)")
                    .trim_end_matches("(OpenType)")
                    .trim();
                if !name.is_empty() {
                    out.push(name.to_string());
                }
            }
        }
        if !any_ok {
            return None;
        }
        out.sort();
        out.dedup();
        Some(out)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn face_installed(face: &str) -> Option<bool> {
    match installed_faces() {
        // A `.ttc` is registered once, under the localized names of every
        // face it holds, joined by " & "; the English family name the
        // terminal wants may be absent from the registry entirely. So a
        // registry miss only sends us to the files, it does not decide.
        Some(list) if !list.iter().any(|e| entry_matches(e, face)) => {}
        Some(_) => return Some(true),
        None => return None,
    }
    let files = installed_font_files();
    if files.is_empty() {
        return Some(false);
    }
    Some(
        files
            .iter()
            .any(|p| families_in_file(p).iter().any(|f| face_matches(f, face))),
    )
}

/// One registry entry can describe several faces: `A Bold & A Italic`.
fn entry_matches(entry: &str, face: &str) -> bool {
    entry.split(" & ").any(|part| face_matches(part, face))
}

/// Whether a registry entry names the same family as `face`.
///
/// Windows appends the style to single-face families, so the entry is
/// `JetBrainsMono NFM Regular` while Windows Terminal wants
/// `JetBrainsMono NFM`. Comparing the raw strings would report every such
/// font as missing and print a bogus warning.
fn face_matches(entry: &str, face: &str) -> bool {
    if entry.eq_ignore_ascii_case(face) {
        return true;
    }
    family(entry).eq_ignore_ascii_case(face)
}

const STYLES: [&str; 9] = [
    "regular", "bold", "italic", "light", "medium", "thin", "semibold", "black", "oblique",
];

/// Drop trailing style words: `Foo SemiBold Italic` -> `Foo`.
fn family(entry: &str) -> &str {
    let mut name = entry.trim();
    while let Some((head, tail)) = name.rsplit_once(' ') {
        if STYLES.iter().any(|s| s.eq_ignore_ascii_case(tail)) {
            name = head.trim_end();
        } else {
            break;
        }
    }
    name
}

/// Font files installed for this user and for all users.
fn installed_font_files() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    if let Ok(win) = std::env::var("WINDIR") {
        dirs.push(PathBuf::from(win).join("Fonts"));
    }

    let mut out = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "ttf" | "otf" | "ttc" | "otc") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn families_in_file(path: &Path) -> Vec<String> {
    match std::fs::File::open(path) {
        Ok(mut f) => families_in(&mut f),
        Err(_) => Vec::new(),
    }
}

/// Family names declared inside a font file. Only the table directory and
/// the `name` table are read, so a 280 MB `.ttc` costs two small seeks.
fn families_in<R: Read + Seek>(r: &mut R) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(head) = read_at(r, 0, 12) else {
        return out;
    };
    let starts: Vec<u64> = if &head[0..4] == b"ttcf" {
        let count = be32(&head, 8) as usize;
        if count == 0 || count > 64 {
            return out;
        }
        let Ok(raw) = read_at(r, 12, count * 4) else {
            return out;
        };
        (0..count).map(|i| be32(&raw, i * 4) as u64).collect()
    } else {
        vec![0]
    };

    for start in starts {
        if let Some(mut names) = families_at(r, start) {
            out.append(&mut names);
        }
    }
    out
}

fn families_at<R: Read + Seek>(r: &mut R, start: u64) -> Option<Vec<String>> {
    let head = read_at(r, start, 12).ok()?;
    let tables = be16(&head, 4) as usize;
    if tables == 0 || tables > 512 {
        return None;
    }
    let dir = read_at(r, start + 12, tables * 16).ok()?;

    for i in 0..tables {
        let rec = &dir[i * 16..i * 16 + 16];
        if &rec[0..4] != b"name" {
            continue;
        }
        // In a `.ttc` these offsets are absolute, in a plain `.ttf` they are
        // relative to the file start — either way `start` is not the base, so
        // the value is used as-is.
        let offset = be32(rec, 8) as u64;
        let len = be32(rec, 12) as usize;
        if !(6..=4 * 1024 * 1024).contains(&len) {
            return None;
        }
        let table = read_at(r, offset, len).ok()?;
        return Some(parse_name_table(&table));
    }
    None
}

/// Family names (id 1 and 16) found in a `name` table.
fn parse_name_table(t: &[u8]) -> Vec<String> {
    let count = be16(t, 2) as usize;
    let storage = be16(t, 4) as usize;
    let mut out = Vec::new();

    for i in 0..count.min(1024) {
        let rec = 6 + i * 12;
        let Some(r) = t.get(rec..rec + 12) else {
            break;
        };
        let id = be16(r, 6);
        if id != 1 && id != 16 {
            continue;
        }
        let platform = be16(r, 0);
        let len = be16(r, 8) as usize;
        let offset = be16(r, 10) as usize;
        let Some(raw) = t.get(storage + offset..storage + offset + len) else {
            continue;
        };
        let text = decode_name(platform, raw);
        if !text.is_empty() {
            out.push(text);
        }
    }
    out
}

fn decode_name(platform: u16, raw: &[u8]) -> String {
    if platform == 0 || platform == 3 {
        let mut units = Vec::with_capacity(raw.len() / 2);
        let mut i = 0;
        while i + 1 < raw.len() {
            units.push(u16::from_be_bytes([raw[i], raw[i + 1]]));
            i += 2;
        }
        String::from_utf16_lossy(&units)
    } else {
        // Mac Roman in practice; close enough for a family name.
        raw.iter().map(|b| *b as char).collect()
    }
}

fn read_at<R: Read + Seek>(r: &mut R, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
    // Windows Terminal settings, comments must survive
    "profiles":
    {
        "defaults":
        {
            "font":
            {
                "face": "0xProto Nerd Font Mono",
                "size": 11
            },
            "opacity": 34,
        },
        "list": [ { "name": "PowerShell", "guid": "{abc}" } ]
    },
    "schemes": [],
}"#;

    #[test]
    fn finds_face_through_comments_and_trailing_commas() {
        assert_eq!(
            current_face(SAMPLE).unwrap().as_deref(),
            Some("0xProto Nerd Font Mono")
        );
    }

    #[test]
    fn replaces_only_the_face_literal() {
        let out = set_face(SAMPLE, "Sarasa Mono SC").unwrap();
        assert_eq!(
            current_face(&out).unwrap().as_deref(),
            Some("Sarasa Mono SC")
        );
        assert!(out.contains("// Windows Terminal settings"), "注释被删了");
        assert!(out.contains("\"size\": 11"), "相邻字段被破坏");
        assert!(
            out.contains("\"opacity\": 34,\n        },"),
            "尾随逗号被破坏"
        );
        assert_eq!(
            out.len(),
            SAMPLE.len() - "0xProto Nerd Font Mono".len() + "Sarasa Mono SC".len()
        );
    }

    #[test]
    fn missing_face_is_an_error_not_a_rewrite() {
        let src = r#"{ "profiles": { "defaults": { "opacity": 34 } } }"#;
        assert!(set_face(src, "Sarasa Mono SC").is_err());
    }

    #[test]
    fn crlf_and_unicode_keys_are_preserved() {
        let src = "{\r\n  \"profiles\": {\r\n    \"defaults\": {\r\n      \"font\": {\r\n        \"face\": \"Cascadia Mono\"\r\n      }\r\n    }\r\n  },\r\n  \"名称\": \"命令提示符\"\r\n}\r\n";
        // A non-ASCII face name: the splice works on byte spans, so this
        // guards the arithmetic against multi-byte characters.
        let out = set_face(src, "等距更纱黑体 SC").unwrap();
        assert!(out.contains("\r\n"), "换行符被改成了 LF");
        assert!(out.contains("命令提示符"), "中文内容受损");
        assert_eq!(
            current_face(&out).unwrap().as_deref(),
            Some("等距更纱黑体 SC")
        );
    }

    #[test]
    fn lone_strings_and_arrays_do_not_confuse_the_walker() {
        let src =
            r#"{ "a": ["x", {"b": "y"}], "profiles": {"defaults": {"font": {"face": "F"}}} }"#;
        let found = entries(src).unwrap();
        assert!(found.iter().any(|e| e.value == "y"));
        assert_eq!(current_face(src).unwrap().as_deref(), Some("F"));
    }

    #[test]
    fn leading_bom_is_kept() {
        let src = "\u{feff}{\"profiles\":{\"defaults\":{\"font\":{\"face\":\"F\"}}}}";
        let out = set_face(src, "G").unwrap();
        assert!(out.starts_with('\u{feff}'), "BOM 被吃掉了");
        assert_eq!(current_face(&out).unwrap().as_deref(), Some("G"));
    }

    #[test]
    fn registry_style_suffix_still_counts_as_installed() {
        // The registry names single-face families "<family> <style>"; the
        // terminal wants the bare family. This used to warn "not installed"
        // for perfectly good fonts.
        assert!(face_matches(
            "JetBrainsMono NFM Regular",
            "JetBrainsMono NFM"
        ));
        assert!(face_matches(
            "JetBrainsMono NFM SemiBold Italic",
            "JetBrainsMono NFM"
        ));
        assert!(face_matches("Cascadia Mono", "Cascadia Mono"));
        assert!(!face_matches("JetBrainsMono NFM", "JetBrainsMono NF"));
        assert!(!face_matches("JetBrainsMono NF", "JetBrainsMono NFM"));
        assert_eq!(
            family("0xProto Nerd Font Mono Light"),
            "0xProto Nerd Font Mono"
        );
        assert!(entry_matches(
            "更纱终端书呆黑体-简 Bold & 更纱终端书呆黑体-简 Italic",
            "更纱终端书呆黑体-简"
        ));
        assert!(!entry_matches("A Bold & B Italic", "C"));
    }

    /// Minimal font file: header, one `name` table, no glyphs. Enough for the
    /// reader, which must not care about anything else.
    fn fixture_body(family: &str, table_offset: usize) -> Vec<u8> {
        let mut store: Vec<u8> = Vec::new();
        let mut records: Vec<(u16, u16, u16)> = Vec::new();
        for (id, text) in [(1u16, family), (2u16, "Regular"), (16u16, family)] {
            let offset = store.len() as u16;
            let bytes: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
            let len = bytes.len() as u16;
            store.extend_from_slice(&bytes);
            records.push((id, offset, len));
        }

        let storage = 6 + records.len() as u16 * 12;
        let mut name = Vec::new();
        name.extend_from_slice(&0u16.to_be_bytes());
        name.extend_from_slice(&(records.len() as u16).to_be_bytes());
        name.extend_from_slice(&storage.to_be_bytes());
        for (id, offset, len) in &records {
            name.extend_from_slice(&3u16.to_be_bytes());
            name.extend_from_slice(&1u16.to_be_bytes());
            name.extend_from_slice(&0x0409u16.to_be_bytes());
            name.extend_from_slice(&id.to_be_bytes());
            name.extend_from_slice(&len.to_be_bytes());
            name.extend_from_slice(&offset.to_be_bytes());
        }
        name.extend_from_slice(&store);

        let mut body = Vec::new();
        body.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&[0u8; 6]);
        body.extend_from_slice(b"name");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(table_offset as u32).to_be_bytes());
        body.extend_from_slice(&(name.len() as u32).to_be_bytes());
        body.extend_from_slice(&name);
        body
    }

    fn fixture_ttf(family: &str) -> Vec<u8> {
        fixture_body(family, 28)
    }

    fn fixture_ttc(families: &[&str]) -> Vec<u8> {
        let mut out = vec![0u8; 12 + 4 * families.len()];
        out[0..4].copy_from_slice(b"ttcf");
        out[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        out[8..12].copy_from_slice(&(families.len() as u32).to_be_bytes());
        for (i, fam) in families.iter().enumerate() {
            let pos = out.len() as u32;
            out[12 + i * 4..16 + i * 4].copy_from_slice(&pos.to_be_bytes());
            // Inside a `.ttc` the table offsets are absolute, so a face that
            // starts at `pos` keeps its table at `pos + 28`.
            out.extend_from_slice(&fixture_body(fam, pos as usize + 28));
        }
        out
    }

    #[test]
    fn family_names_come_out_of_the_name_table() {
        let mut cur = std::io::Cursor::new(fixture_ttf("Sarasa Term SC Nerd"));
        let names = families_in(&mut cur);
        assert!(
            names.iter().any(|n| n == "Sarasa Term SC Nerd"),
            "英语族名没读出来: {names:?}"
        );
        assert!(!names.iter().any(|n| n == "Regular"), "把子族名当族名了");
    }

    #[test]
    fn every_face_of_a_collection_is_read() {
        let ttc = fixture_ttc(&["Sarasa Term SC Nerd", "更纱终端书呆黑体-简"]);
        let mut cur = std::io::Cursor::new(ttc);
        let names = families_in(&mut cur);
        assert!(names.iter().any(|n| n == "Sarasa Term SC Nerd"));
        assert!(
            names.iter().any(|n| n == "更纱终端书呆黑体-简"),
            "本地化族名没读出来: {names:?}"
        );
    }

    #[test]
    fn junk_does_not_parse_as_a_font() {
        let mut cur = std::io::Cursor::new(b"not a font at all".to_vec());
        assert!(families_in(&mut cur).is_empty());
        let mut empty = std::io::Cursor::new(Vec::new());
        assert!(families_in(&mut empty).is_empty());
    }
}
