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
use std::ops::Range;
use std::path::PathBuf;

/// The face this machine was asked to use; also the default in `font --face`.
pub const DEFAULT_FACE: &str = "HarmonyOS Sans SC";

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
    installed_faces().map(|list| list.iter().any(|f| f.eq_ignore_ascii_case(face)))
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
        let out = set_face(SAMPLE, "HarmonyOS Sans SC").unwrap();
        assert_eq!(
            current_face(&out).unwrap().as_deref(),
            Some("HarmonyOS Sans SC")
        );
        assert!(out.contains("// Windows Terminal settings"), "注释被删了");
        assert!(out.contains("\"size\": 11"), "相邻字段被破坏");
        assert!(
            out.contains("\"opacity\": 34,\n        },"),
            "尾随逗号被破坏"
        );
        assert_eq!(
            out.len(),
            SAMPLE.len() - "0xProto Nerd Font Mono".len() + "HarmonyOS Sans SC".len()
        );
    }

    #[test]
    fn missing_face_is_an_error_not_a_rewrite() {
        let src = r#"{ "profiles": { "defaults": { "opacity": 34 } } }"#;
        assert!(set_face(src, "HarmonyOS Sans SC").is_err());
    }

    #[test]
    fn crlf_and_unicode_keys_are_preserved() {
        let src = "{\r\n  \"profiles\": {\r\n    \"defaults\": {\r\n      \"font\": {\r\n        \"face\": \"Cascadia Mono\"\r\n      }\r\n    }\r\n  },\r\n  \"名称\": \"命令提示符\"\r\n}\r\n";
        let out = set_face(src, "HarmonyOS Sans SC").unwrap();
        assert!(out.contains("\r\n"), "换行符被改成了 LF");
        assert!(out.contains("命令提示符"), "中文内容受损");
        assert_eq!(
            current_face(&out).unwrap().as_deref(),
            Some("HarmonyOS Sans SC")
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
}
