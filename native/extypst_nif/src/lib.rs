use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::Hash;
use std::path::{Component, Path, PathBuf};

use comemo::Prehashed;
use elsa::FrozenVec;
use memmap2::Mmap;
use once_cell::unsync::OnceCell;
use same_file::Handle;
use siphasher::sip128::{Hasher128, SipHasher13};
use typst::{
    diag::{FileError, FileResult, StrResult},
    eval::{Bytes, Datetime, Library},
    font::{Font, FontBook, FontInfo},
    syntax::{FileId, Source, VirtualPath},
    World,
};
use walkdir::WalkDir;

/// A world that provides access to the operating system.
pub struct SystemWorld {
    root: PathBuf,
    library: Prehashed<Library>,
    book: Prehashed<FontBook>,
    fonts: Vec<FontSlot>,
    hashes: RefCell<HashMap<PathBuf, FileResult<PathHash>>>,
    paths: RefCell<HashMap<PathHash, PathSlot>>,
    sources: FrozenVec<Box<Source>>,
    main: Source,
}

/// Holds details about the location of a font and lazily the font itself.
#[derive(Debug)]
struct FontSlot {
    path: PathBuf,
    index: u32,
    font: OnceCell<Option<Font>>,
}

/// Holds canonical data for all paths pointing to the same entity.
#[derive(Default)]
struct PathSlot {
    source: OnceCell<FileResult<FileId>>,
    buffer: OnceCell<FileResult<Bytes>>,
}

impl World for SystemWorld {
    fn library(&self) -> &Prehashed<Library> {
        &self.library
    }

    fn book(&self) -> &Prehashed<FontBook> {
        &self.book
    }

    fn main(&self) -> Source {
        self.main.clone()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        self.sources
            .iter()
            .find(|&needle| needle.id().eq(&id))
            .cloned()
            .ok_or_else(|| FileError::NotSource)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        Ok(Bytes::from(self.source(id)?.text().as_bytes()))
    }

    fn font(&self, id: usize) -> Option<Font> {
        let slot = &self.fonts[id];

        slot.font
            .get_or_init(|| {
                let data = read(&slot.path).map(Bytes::from).ok()?;
                Font::new(data, slot.index)
            })
            .clone()
    }

    fn today(&self, _offset: Option<i64>) -> Option<Datetime> {
        unimplemented!()
    }
}

impl SystemWorld {
    pub fn new(root: PathBuf, font_paths: &[PathBuf], font_files: &[PathBuf]) -> Self {
        let mut searcher = FontSearcher::new();
        searcher.search_system();

        for path in font_paths {
            searcher.search_dir(path);
        }
        for path in font_files {
            searcher.search_file(path);
        }

        Self {
            root,
            library: Prehashed::new(typst_library::build()),
            book: Prehashed::new(searcher.book),
            fonts: searcher.fonts,
            hashes: RefCell::default(),
            paths: RefCell::default(),
            sources: FrozenVec::new(),
            main: Source::detached("detached"),
        }
    }

    fn slot(&self, path: &Path) -> FileResult<RefMut<PathSlot>> {
        let mut hashes = self.hashes.borrow_mut();

        let hash = hashes.get(path).cloned().map_or_else(
            || {
                let hash = PathHash::new(path);
                if let Ok(canon) = path.canonicalize() {
                    hashes.insert(normalize(&canon), hash.clone());
                }
                hashes.insert(path.into(), hash.clone());
                hash
            },
            |hash| hash,
        )?;

        Ok(std::cell::RefMut::map(self.paths.borrow_mut(), |paths| {
            paths.entry(hash).or_default()
        }))
    }

    fn insert(&self, path: &Path, text: String) -> FileId {
        let id = FileId::new(None, VirtualPath::new(path));
        let source = Source::new(id, text);
        self.sources.push(Box::new(source));
        id
    }

    fn reset(&mut self) {
        self.sources.as_mut().clear();
        self.hashes.borrow_mut().clear();
        self.paths.borrow_mut().clear();
    }

    pub fn compile(&mut self, markup: String) -> StrResult<Vec<u8>> {
        self.reset();
        self.main = self.source(self.insert(Path::new("MARKUP.tsp"), markup))?;

        let mut tracer = typst::eval::Tracer::new();

        match typst::compile(self, &mut tracer) {
            // Export the PDF.
            Ok(document) => {
                let buffer = typst::export::pdf(&document, None, None);
                Ok(buffer)
            }

            // Format diagnostics.
            Err(errors) => {
                let mut error_msg = "compile error:\n".to_string();

                for error in errors {
                    let range = self
                        .source(error.span.id().expect("the span must reference a fileid"))?
                        .range(error.span)
                        .expect("catches an extra check using `find`");

                    error_msg.push_str(&format!("{}:{} {}", range.start, range.end, error.message));

                    // stacktrace
                    if !error.trace.is_empty() {
                        error_msg.push_str("stacktrace:\n");
                    }
                    for point in error.trace {
                        let range = self
                            .source(
                                point
                                    .span
                                    .id()
                                    .ok_or(FileError::Other(Some("is detached".into())))?,
                            )?
                            .find(point.span)
                            .unwrap()
                            .range();
                        let message = point.v.to_string();
                        error_msg.push_str(&format!("  {}:{} {}", range.start, range.end, message));
                    }
                }
                Err(error_msg.into())
            }
        }
    }
}

/// A hash that is the same for all paths pointing to the same entity.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
struct PathHash(u128);

impl PathHash {
    fn new(path: &Path) -> FileResult<Self> {
        let f = |e| FileError::from_io(e, path);
        let handle = Handle::from_path(path).map_err(f)?;
        let mut state = SipHasher13::new();
        handle.hash(&mut state);
        Ok(Self(state.finish128().as_u128()))
    }
}

/// Read a file.
fn read(path: &Path) -> FileResult<Vec<u8>> {
    let f = |e| FileError::from_io(e, path);
    if fs::metadata(path).map_err(f)?.is_dir() {
        Err(FileError::IsDirectory)
    } else {
        fs::read(path).map_err(f)
    }
}

/// Searches for fonts.
struct FontSearcher {
    book: FontBook,
    fonts: Vec<FontSlot>,
}

impl FontSearcher {
    /// Create a new, empty system searcher.
    fn new() -> Self {
        Self {
            book: FontBook::new(),
            fonts: vec![],
        }
    }

    /// Search for fonts in the linux system font directories.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn search_system(&mut self) {
        self.search_dir("/usr/share/fonts");
        self.search_dir("/usr/local/share/fonts");

        if let Some(dir) = dirs::font_dir() {
            self.search_dir(dir);
        }
    }

    /// Search for fonts in the macOS system font directories.
    #[cfg(target_os = "macos")]
    fn search_system(&mut self) {
        self.search_dir("/Library/Fonts");
        self.search_dir("/System/Library/Fonts");

        // Downloadable fonts, location varies on major macOS releases
        if let Ok(dir) = fs::read_dir("/System/Library/AssetsV2") {
            for entry in dir {
                let Ok(entry) = entry else { continue };
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("com_apple_MobileAsset_Font")
                {
                    self.search_dir(entry.path());
                }
            }
        }

        self.search_dir("/Network/Library/Fonts");

        if let Some(dir) = dirs::font_dir() {
            self.search_dir(dir);
        }
    }

    /// Search for fonts in the Windows system font directories.
    #[cfg(windows)]
    fn search_system(&mut self) {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());

        self.search_dir(Path::new(&windir).join("Fonts"));

        if let Some(roaming) = dirs::config_dir() {
            self.search_dir(roaming.join("Microsoft\\Windows\\Fonts"));
        }

        if let Some(local) = dirs::cache_dir() {
            self.search_dir(local.join("Microsoft\\Windows\\Fonts"));
        }
    }

    /// Search for all fonts in a directory recursively.
    fn search_dir(&mut self, path: impl AsRef<Path>) {
        for entry in WalkDir::new(path)
            .follow_links(true)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("ttf" | "otf" | "TTF" | "OTF" | "ttc" | "otc" | "TTC" | "OTC"),
            ) {
                self.search_file(path);
            }
        }
    }

    /// Index the fonts in the file at the given path.
    fn search_file(&mut self, path: impl AsRef<Path>) {
        let path = path.as_ref();
        if let Ok(file) = File::open(path) {
            if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                for (i, info) in FontInfo::iter(&mmap).enumerate() {
                    self.book.push(info);
                    self.fonts.push(FontSlot {
                        path: path.into(),
                        index: i as u32,
                        font: OnceCell::new(),
                    });
                }
            }
        }
    }
}

#[rustler::nif]
fn compile(markup: String, extra_fonts: Vec<String>) -> Result<String, String> {
    let extra_fonts_paths: Vec<PathBuf> = extra_fonts.iter().map(|f| Path::new(f).into()).collect();

    let mut world = SystemWorld::new(".".into(), extra_fonts_paths.as_slice(), &[]);
    match world.compile(markup) {
        Ok(pdf_bytes) => {
            // the resulting string is not an utf-8 encoded string, but this is exactly what we
            // want as we are passing a binary back to elixir
            unsafe { Ok(String::from_utf8_unchecked(pdf_bytes)) }
        }
        Err(e) => Err(e.into()),
    }
}

rustler::init!("Elixir.ExTypst.NIF", [compile]);

/// Normalizes a path such that that it can be used as a key in a hashmap.
///
/// Note: code carried over from typst v0.4.0
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                _ => out.push(component),
            },
            _ => out.push(component),
        }
    }
    out
}
