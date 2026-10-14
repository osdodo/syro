//! The file tree's glyphs: which icon a row draws beside a name.

use gpui_kit::component::Icon;

/// One glyph: one SVG in `assets/icons/`, named the way the icon table names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Glyph {
    Archive,
    Axis3d,
    Box,
    Braces,
    Chart,
    Code,
    Cog,
    Database,
    Diff,
    File,
    Image,
    Json,
    Key,
    Lock,
    Music,
    Sheet,
    Sliders,
    Terminal,
    Text,
    Type,
    Video,
}

impl Glyph {
    /// The SVG itself, as the bytes `Icon::data` takes. The bytes *are* the
    /// asset, so a glyph cannot come to mean a different picture than its name
    /// says without this match failing to compile.
    fn svg(self) -> &'static [u8] {
        match self {
            Glyph::Archive => include_bytes!("../assets/icons/file-archive.svg"),
            Glyph::Axis3d => include_bytes!("../assets/icons/file-axis-3d.svg"),
            Glyph::Box => include_bytes!("../assets/icons/file-box.svg"),
            Glyph::Braces => include_bytes!("../assets/icons/file-braces.svg"),
            Glyph::Chart => include_bytes!("../assets/icons/file-chart-column.svg"),
            Glyph::Code => include_bytes!("../assets/icons/file-code.svg"),
            Glyph::Cog => include_bytes!("../assets/icons/file-cog.svg"),
            Glyph::Database => include_bytes!("../assets/icons/database.svg"),
            Glyph::Diff => include_bytes!("../assets/icons/file-diff.svg"),
            Glyph::File => include_bytes!("../assets/icons/file.svg"),
            Glyph::Image => include_bytes!("../assets/icons/file-image.svg"),
            Glyph::Json => include_bytes!("../assets/icons/file-json.svg"),
            Glyph::Key => include_bytes!("../assets/icons/file-key.svg"),
            Glyph::Lock => include_bytes!("../assets/icons/file-lock.svg"),
            Glyph::Music => include_bytes!("../assets/icons/file-music.svg"),
            Glyph::Sheet => include_bytes!("../assets/icons/file-spreadsheet.svg"),
            Glyph::Sliders => include_bytes!("../assets/icons/file-sliders.svg"),
            Glyph::Terminal => include_bytes!("../assets/icons/file-terminal.svg"),
            Glyph::Text => include_bytes!("../assets/icons/file-text.svg"),
            Glyph::Type => include_bytes!("../assets/icons/file-type.svg"),
            Glyph::Video => include_bytes!("../assets/icons/file-video.svg"),
        }
    }
}

/// The icon a file named `name` is drawn with.
pub fn file_icon(name: &str) -> Icon {
    Icon::default().data(glyph(name).svg())
}

/// Which glyph `name` gets: the two tables, and the one rule that comes before
/// them.
fn glyph(name: &str) -> Glyph {
    // Lower-cased once, here: every key in both tables is lower case, and
    // `Makefile` and `makefile` are the same file to a reader.
    let name = name.to_ascii_lowercase();
    // A `.env` is not the environment it configures; it holds secrets, and the
    // lock is what says so. `.env.local` and `.env.production` are the same
    // kind of file as `.env`, which no exact-name table can express.
    if name == ".env" || name.starts_with(".env.") {
        return Glyph::Lock;
    }
    by_name(&name).unwrap_or_else(|| by_extension(&name))
}

/// The names the table knows outright — the ones whose extension lies
/// (`Makefile` has none) or whose whole point is that they are that file.
fn by_name(name: &str) -> Option<Glyph> {
    Some(match name {
        "bun.lock"
        | "bun.lockb"
        | "cargo.lock"
        | "cargo.toml"
        | "composer.json"
        | "composer.lock"
        | "gemfile"
        | "go.mod"
        | "go.sum"
        | "package-lock.json"
        | "package.json"
        | "pipfile"
        | "pnpm-lock.yaml"
        | "pnpm-workspace.yaml"
        | "poetry.lock"
        | "pom.xml"
        | "pyproject.toml"
        | "requirements-dev.txt"
        | "requirements.txt"
        | "yarn.lock" => Glyph::Box,
        "cmakelists.txt"
        | "dockerfile"
        | "meson.build"
        | "settings.gradle"
        | "settings.gradle.kts" => Glyph::Cog,
        ".eslintrc.json" | ".prettierrc.json" => Glyph::Json,
        "codeowners" | "copying" | "license" | "notice" => Glyph::Key,
        "security" | "security.md" => Glyph::Lock,
        ".babelrc"
        | ".dockerignore"
        | ".editorconfig"
        | ".eslintrc"
        | ".eslintrc.cjs"
        | ".eslintrc.js"
        | ".eslintrc.yaml"
        | ".eslintrc.yml"
        | ".gitattributes"
        | ".gitignore"
        | ".npmrc"
        | ".prettierrc"
        | ".prettierrc.yaml"
        | ".prettierrc.yml"
        | "components.json"
        | "postcss.config.cjs"
        | "postcss.config.js"
        | "postcss.config.mjs"
        | "postcss.config.ts"
        | "tailwind.config.cjs"
        | "tailwind.config.js"
        | "tailwind.config.mjs"
        | "tailwind.config.ts"
        | "tsconfig.json"
        | "vite.config.js"
        | "vite.config.mjs"
        | "vite.config.ts"
        | "vitest.config.js"
        | "vitest.config.mjs"
        | "vitest.config.ts" => Glyph::Sliders,
        "makefile" => Glyph::Terminal,
        "agents.md" | "authors" | "changelog" | "changelog.md" | "contributing"
        | "contributing.md" | "readme" | "readme.md" | "todo" => Glyph::Text,
        _ => return None,
    })
}

/// The extension table, which is where nearly every file lands.
fn by_extension(name: &str) -> Glyph {
    match extension(name) {
        "7z" | "br" | "bz2" | "dmg" | "gz" | "iso" | "rar" | "tar" | "tar.bz2" | "tar.gz"
        | "tar.xz" | "tbz2" | "tgz" | "txz" | "xz" | "zip" => Glyph::Archive,
        "blend" | "fbx" | "glb" | "gltf" | "obj" | "stl" => Glyph::Axis3d,
        "gql" | "graphql" | "proto" => Glyph::Braces,
        "ipynb" | "mmd" | "ppt" | "pptx" => Glyph::Chart,
        "astro" | "c" | "cc" | "cjs" | "clj" | "cpp" | "cs" | "cts" | "cxx" | "dart" | "erl"
        | "ex" | "exs" | "fs" | "fsx" | "go" | "h" | "hpp" | "hrl" | "hs" | "htm" | "html"
        | "java" | "js" | "jsx" | "kt" | "kts" | "lua" | "mjs" | "mts" | "nim" | "php" | "pl"
        | "pm" | "py" | "r" | "rb" | "rs" | "scala" | "sol" | "svelte" | "swift" | "ts" | "tsx"
        | "vb" | "vue" | "xhtml" | "xml" | "zig" => Glyph::Code,
        "gradle" => Glyph::Cog,
        "db" | "duckdb" | "prisma" | "sql" | "sqlite" | "sqlite3" => Glyph::Database,
        "diff" | "patch" => Glyph::Diff,
        "ai" | "avif" | "bmp" | "eps" | "gif" | "heic" | "ico" | "jpeg" | "jpg" | "png" | "psd"
        | "svg" | "tif" | "tiff" | "webp" => Glyph::Image,
        "json" | "json5" | "jsonc" | "jsonl" | "ndjson" => Glyph::Json,
        "asc" | "cer" | "crt" | "gpg" | "key" | "pem" | "pub" => Glyph::Key,
        "lock" | "p12" | "pfx" => Glyph::Lock,
        "aac" | "flac" | "m4a" | "mp3" | "ogg" | "opus" | "wav" => Glyph::Music,
        "csv" | "ods" | "tsv" | "xls" | "xlsx" => Glyph::Sheet,
        "cfg" | "conf" | "hcl" | "ini" | "properties" | "tf" | "tfvars" | "toml" | "yaml"
        | "yml" => Glyph::Sliders,
        "bash" | "bat" | "cmd" | "fish" | "nu" | "ps1" | "sh" | "zsh" => Glyph::Terminal,
        "adoc" | "doc" | "docx" | "log" | "md" | "mdx" | "pdf" | "qmd" | "rmarkdown" | "rmd"
        | "rst" | "rtf" | "tex" | "txt" => Glyph::Text,
        "css" | "eot" | "less" | "otf" | "sass" | "scss" | "ttf" | "woff" | "woff2" => Glyph::Type,
        "avi" | "m4v" | "mkv" | "mov" | "mp4" | "mpeg" | "mpg" | "webm" => Glyph::Video,
        _ => Glyph::File,
    }
}

/// The extension of `name`, lower-cased: `""` when it has none that counts.
fn extension(name: &str) -> &str {
    for compound in ["tar.bz2", "tar.gz", "tar.xz"] {
        if name.ends_with(compound) {
            return compound;
        }
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => &name[dot + 1..],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::{Glyph, glyph};

    /// The tables, and the order the three ways of picking between them run in.
    #[test]
    fn a_name_gets_the_glyph_the_table_gives_it() {
        // The name table, and that it is read without regard to case.
        assert_eq!(glyph("package.json"), Glyph::Box);
        assert_eq!(glyph("Makefile"), Glyph::Terminal);
        assert_eq!(glyph(".gitignore"), Glyph::Sliders);
        assert_eq!(glyph("README"), Glyph::Text);
        // The extension table.
        assert_eq!(glyph("main.rs"), Glyph::Code);
        assert_eq!(glyph("icon.PNG"), Glyph::Image);
        assert_eq!(glyph("notes.json"), Glyph::Json);
        assert_eq!(glyph("bundle.tar.gz"), Glyph::Archive);
        // A name beats its extension: `cargo.toml` is a manifest, not a config
        // file, and `package.json` is a manifest, not JSON.
        assert_eq!(glyph("cargo.toml"), Glyph::Box);
        assert_ne!(glyph("cargo.toml"), glyph("other.toml"));
        // Nothing knows it: the plain file, which is what a reader expects of a
        // name this panel has never seen.
        assert_eq!(glyph("no-extension"), Glyph::File);
        assert_eq!(glyph("trailing."), Glyph::File);
        assert_eq!(glyph(""), Glyph::File);
    }

    /// Every glyph is a whole SVG. An `include_bytes!` pointing at the wrong
    /// file — or at a truncated one — is invisible in the app, where a bad
    /// glyph draws nothing and logs nothing a test can see.
    #[test]
    fn every_glyph_is_a_whole_svg() {
        let all = [
            Glyph::Archive,
            Glyph::Axis3d,
            Glyph::Box,
            Glyph::Braces,
            Glyph::Chart,
            Glyph::Code,
            Glyph::Cog,
            Glyph::Database,
            Glyph::Diff,
            Glyph::File,
            Glyph::Image,
            Glyph::Json,
            Glyph::Key,
            Glyph::Lock,
            Glyph::Music,
            Glyph::Sheet,
            Glyph::Sliders,
            Glyph::Terminal,
            Glyph::Text,
            Glyph::Type,
            Glyph::Video,
        ];
        for glyph in all {
            let svg = String::from_utf8_lossy(glyph.svg());
            assert!(svg.starts_with("<svg"), "{glyph:?} does not start an SVG");
            assert!(svg.contains("</svg>"), "{glyph:?} is not a whole SVG");
            // The colour comes from the row, so the glyph must not carry one:
            // a hard-coded fill would paint over the row's own text colour.
            assert!(
                svg.contains("currentColor"),
                "{glyph:?} does not use currentColor"
            );
        }
    }
}
