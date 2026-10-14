//! "How do I open this?" — the opener registry.

use std::path::Path;

use rust_i18n::t;

/// The editor's size limit: "> 5 MB 或二进制文件不进编辑器".
pub const MAX_TEXT_BYTES: u64 = 5 * 1024 * 1024;

/// The language a unified diff is read as. Not from a path — a diff is
/// synthesised, not opened — so it lives here beside the table that names every
/// other language, and resolves through the same registry.
pub const DIFF_LANGUAGE: &str = "diff";

/// Point the platform's file manager at a path. `open -R` is macOS's "Reveal in
/// Finder", and opening a path *with the desktop* rather than with this app's
/// own editor is the one thing this registry has to say about a path — which is
/// why both the file tree and the sidebar's worktree menu ask it.
pub fn reveal(path: &Path) {
    let mut command = std::process::Command::new("open");
    command.arg("-R").arg(path);
    if let Err(error) = command.spawn() {
        tracing::warn!(%error, path = %path.display(), "reveal failed");
    }
}

/// What a file's extension says it is, as a gpui-kit registry identifier.
pub fn language_for(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    // An extension is case-insensitive: `Makefile` is not the shape this table
    // keys on, but `README.MD` is.
    let extension = extension.to_ascii_lowercase();
    let language = match extension.as_str() {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        // No `jsx` grammar is compiled in; the JavaScript grammar parses JSX.
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "sh" | "bash" | "zsh" => "bash",
        "toml" => "toml",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        _ => return None,
    };
    Some(language)
}

/// Whether `path`'s name says it is an image the preview can draw. The bytes
/// decide the format — gpui guesses it from them — so the name's only job is to
/// say "this is a picture, hand it to the image decoder, do not read it as text".
pub fn is_image(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "tif"
            | "tiff"
            | "svg"
            | "pbm"
            | "pgm"
            | "ppm"
            | "pnm"
    )
}

/// Why a file cannot be opened as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Over [`MAX_TEXT_BYTES`].
    TooLarge { bytes: u64 },
    /// A NUL byte in the first [`PROBE_BYTES`], or bytes that are not UTF-8.
    Binary,
}

/// How far into a file the NUL check looks. A binary file that is all zeros
/// after the first 8 KiB is not a file anyone is editing.
const PROBE_BYTES: usize = 8 * 1024;

impl Refusal {
    /// The sentence the UI shows. `path` is what the reader is looking at.
    pub fn message(&self, path: &Path) -> String {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        match self {
            Refusal::TooLarge { bytes } => t!(
                "opener.too_large",
                name = name,
                mb = format!("{:.1}", *bytes as f64 / (1024. * 1024.)),
                limit = MAX_TEXT_BYTES / (1024 * 1024)
            )
            .to_string(),
            Refusal::Binary => t!("opener.not_text", name = name).to_string(),
        }
    }
}

/// What to do with a file that was clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Its text, ready for an editor.
    Text(String),
    /// A picture, for an image pane to draw.
    Image,
    /// Not this editor's to show.
    Refused(Refusal),
}

/// Read `path` as text, or say why not. The size is checked before the read, so
/// a huge file is refused without being loaded first.
pub fn open(path: &Path) -> anyhow::Result<Opened> {
    // Before the size check: an image is not text at all, and nothing here has
    // to read it to know that.
    if is_image(path) {
        return Ok(Opened::Image);
    }
    let bytes = std::fs::metadata(path)?.len();
    if bytes > MAX_TEXT_BYTES {
        return Ok(Opened::Refused(Refusal::TooLarge { bytes }));
    }
    let content = std::fs::read(path)?;
    let probe = &content[..content.len().min(PROBE_BYTES)];
    if probe.contains(&0) {
        return Ok(Opened::Refused(Refusal::Binary));
    }
    match String::from_utf8(content) {
        Ok(text) => Ok(Opened::Text(text)),
        // Not UTF-8 and no NUL in the probe: still not text this editor can
        // hold, and the same sentence covers both.
        Err(_) => Ok(Opened::Refused(Refusal::Binary)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "syro-opener-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The same, but the name keeps `name` with its extension: `is_image` reads
    /// the name, so the image tests cannot use the extension-less helper above.
    fn temp_file_named(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let stem = format!(
            "syro-opener-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(format!("{stem}-{name}"));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn names_the_languages_it_knows() {
        for (name, language) in [
            ("main.rs", Some("rust")),
            ("app.ts", Some("typescript")),
            ("App.tsx", Some("tsx")),
            ("index.mjs", Some("javascript")),
            ("Component.jsx", Some("javascript")),
            ("script.py", Some("python")),
            ("main.go", Some("go")),
            ("build.sh", Some("bash")),
            ("settings.toml", Some("toml")),
            ("Cargo.json", Some("json")),
            ("syro.yaml", Some("yaml")),
            ("README.md", Some("markdown")),
            // Case-insensitive, because extensions are written both ways.
            ("README.MD", Some("markdown")),
        ] {
            assert_eq!(language_for(Path::new(name)), language, "{name}");
        }
    }

    /// The invariant the editor depends on and nothing else checks: every name this table returns
    /// is one gpui-kit's registry can actually parse.
    #[test]
    fn names_only_languages_the_registry_has() {
        use gpui_kit::component::highlighter::LanguageRegistry;

        let registry = LanguageRegistry::singleton();
        let mut named = 0;
        for name in [
            "a.rs",
            "a.ts",
            "a.tsx",
            "a.js",
            "a.jsx",
            "a.py",
            "a.go",
            "a.rb",
            "a.java",
            "a.c",
            "a.cpp",
            "a.sh",
            "a.toml",
            "a.json",
            "a.yaml",
            "a.md",
            "a.html",
            "a.css",
            DIFF_LANGUAGE,
        ] {
            let language = language_for(Path::new(name)).unwrap_or_else(|| {
                // `DIFF_LANGUAGE` is not a path's answer; the rest are.
                assert_eq!(name, DIFF_LANGUAGE, "{name} should name a language");
                DIFF_LANGUAGE
            });
            let config = registry.language(language).unwrap_or_else(|| {
                panic!("{name} names {language:?}, which the registry does not know")
            });
            assert!(
                config.has_grammar(),
                "{name} names {language:?}, which the registry has no grammar for"
            );
            named += 1;
        }
        // A registry that answered nothing would pass the loop above.
        assert!(named > 15, "only {named} languages were checked");

        // The other half of the claim: a name the registry knows but has no grammar for is a language,
        // not a crash — the editor parses nothing and draws the text.
        assert!(!registry.language("text").unwrap().has_grammar());
        assert!(registry.language("not-a-language").is_none());
        assert_eq!(language_for(Path::new("notes.wat")), None);
    }

    /// The only `Opened` variant that carries no file contents: the name alone
    /// decides, so a `.png` whose bytes decode as nothing is still an image —
    /// the decoder is the one that gets to complain.
    #[test]
    fn an_image_opens_as_an_image() {
        for name in [
            "logo.png",
            "photo.JPG",
            "anim.gif",
            "shot.webp",
            "icon.svg",
            "scan.tiff",
        ] {
            assert!(is_image(Path::new(name)), "{name}");
            let path = temp_file_named(name, &[0u8, 1, 2, 3]);
            assert_eq!(open(&path).unwrap(), Opened::Image, "{name}");
            let _ = std::fs::remove_file(&path);
        }

        // And a name that is not a picture's is not one.
        for name in ["notes.md", "main.rs", "README", "archive.tar.gz"] {
            assert!(!is_image(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn an_unknown_or_missing_extension_is_no_language() {
        assert_eq!(language_for(Path::new("notes.wat")), None);
        assert_eq!(language_for(Path::new("Makefile")), None);
        assert_eq!(language_for(Path::new(".gitignore")), None);
        assert_eq!(language_for(Path::new("archive.tar.gz")), None);
    }

    #[test]
    fn text_is_read_as_it_is() {
        let path = temp_file("text", "hello\nworld\n".as_bytes());
        assert_eq!(
            open(&path).unwrap(),
            Opened::Text("hello\nworld\n".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_nul_byte_is_refused_as_binary() {
        let path = temp_file("binary", &[b'a', 0, b'b']);
        assert_eq!(open(&path).unwrap(), Opened::Refused(Refusal::Binary));
        assert_eq!(
            Refusal::Binary.message(&path),
            format!(
                "{} is not text",
                path.file_name().unwrap().to_string_lossy()
            )
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bytes_that_are_not_utf8_are_refused_too() {
        let path = temp_file("latin1", &[0xff, 0xfe, b'h', b'i']);
        assert_eq!(open(&path).unwrap(), Opened::Refused(Refusal::Binary));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_over_the_limit_is_refused_without_being_read() {
        // Sparse, so the test does not write 5 MB to disk: the size check runs
        // against the metadata, which is the point of checking it first.
        let path = temp_file("large", b"");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_TEXT_BYTES + 1).unwrap();
        drop(file);

        match open(&path).unwrap() {
            Opened::Refused(Refusal::TooLarge { bytes }) => {
                assert_eq!(bytes, MAX_TEXT_BYTES + 1)
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            Refusal::TooLarge {
                bytes: MAX_TEXT_BYTES + 1
            }
            .message(&path)
            .contains("over the 5 MB editor limit")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_limit_is_inclusive() {
        // Exactly at the limit is text: the rule is "over 5 MB", and a file
        // one byte under it is the case an off-by-one would get wrong.
        let path = temp_file("at-limit", &vec![b'a'; MAX_TEXT_BYTES as usize - 1]);
        match open(&path).unwrap() {
            Opened::Text(text) => assert_eq!(text.len() as u64, MAX_TEXT_BYTES - 1),
            other => panic!("expected text at the limit, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
