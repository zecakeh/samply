use debugid::DebugId;
use samply_api::samply_symbols;
use samply_symbols::{
    CandidatePathInfo, FileAndPathHelper, FileAndPathHelperResult, FileLocation,
    OptionallySendFuture,
};
use symsrv::{memmap2, FileContents, SymbolCache};

use std::{
    fs::File,
    path::{Path, PathBuf},
    pin::Pin,
};

use crate::config::SymbolManagerConfig;

pub struct Helper {
    symbol_cache: Option<SymbolCache>,
    config: SymbolManagerConfig,
}

impl Helper {
    pub fn with_config(config: SymbolManagerConfig) -> Self {
        let symbol_cache = match config.nt_symbol_path.clone() {
            Some(nt_symbol_path) => Some(SymbolCache::new(nt_symbol_path, config.verbose)),
            None => None,
        };
        Self {
            symbol_cache,
            config,
        }
    }

    async fn open_file_impl(
        &self,
        location: FileLocation,
    ) -> FileAndPathHelperResult<FileContents> {
        match location {
            FileLocation::Path(path) => {
                if self.config.verbose {
                    eprintln!("Opening file {:?}", path.to_string_lossy());
                }
                let file = File::open(&path)?;
                Ok(FileContents::Mmap(unsafe {
                    memmap2::MmapOptions::new().map(&file)?
                }))
            }
            FileLocation::Custom(custom) => {
                assert!(custom.starts_with("symbolserver:"));
                let path = custom.trim_start_matches("symbolserver:");
                if self.config.verbose {
                    eprintln!("Trying to get file {:?} from symbol cache", path);
                }
                Ok(self
                    .symbol_cache
                    .as_ref()
                    .unwrap()
                    .get_file(Path::new(path))
                    .await?)
            }
        }
    }
}

impl<'h> FileAndPathHelper<'h> for Helper {
    type F = FileContents;
    type OpenFileFuture =
        Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>>;

    fn get_candidate_paths_for_binary_or_pdb(
        &self,
        debug_name: &str,
        debug_id: &DebugId,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        let mut paths = vec![];

        // Look up (debugName, breakpadId) in the path map.
        let libinfo = self
            .config
            .known_libs
            .get(&(debug_name.to_string(), *debug_id))
            .cloned()
            .unwrap_or_default();

        let mut got_dsym = false;

        if let Some(debug_path) = &libinfo.debug_path {
            // First, see if we can find a dSYM file for the binary.
            if let Some(dsym_path) =
                crate::moria_mac::locate_dsym_fastpath(Path::new(debug_path), debug_id.uuid())
            {
                got_dsym = true;
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    dsym_path.clone(),
                )));
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    dsym_path
                        .join("Contents")
                        .join("Resources")
                        .join("DWARF")
                        .join(debug_name),
                )));
            }

            // Also consider .so.dbg files in the same directory.
            if debug_name.ends_with(".so") {
                let dbg_name = format!("{}.dbg", debug_name);
                let debug_path = PathBuf::from(debug_path);
                if let Some(dir) = debug_path.parent() {
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                        dir.join(dbg_name),
                    )));
                }
            }
        }

        if libinfo.debug_path != libinfo.path {
            if let Some(debug_path) = &libinfo.debug_path {
                // Get symbols from the debug file.
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    debug_path.into(),
                )));
            }
        }

        if !got_dsym {
            // Try a little harder to find a dSYM, just from the UUID. We can do this
            // even if we don't have an entry for this library in the libinfo map.
            if let Ok(dsym_path) = crate::moria_mac::locate_dsym_using_spotlight(debug_id.uuid()) {
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    dsym_path.clone(),
                )));
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    dsym_path
                        .join("Contents")
                        .join("Resources")
                        .join("DWARF")
                        .join(debug_name),
                )));
            }
        }

        // Find debuginfo in /usr/lib/debug/.build-id/ etc.
        // <https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html>
        if let Some(code_id) = &libinfo.code_id {
            let code_id = code_id.as_str();
            if code_id.len() > 2 {
                let (two_chars, rest) = code_id.split_at(2);
                let path = format!("/usr/lib/debug/.build-id/{}/{}.debug", two_chars, rest);
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    PathBuf::from(path),
                )));
            }
        }

        // Fake "debug link" support. We hardcode a "debug link name" of
        // `{debug_name}.debug`.
        // It would be better to get the actual debug link name from the binary.
        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
            PathBuf::from(format!("/usr/bin/{}.debug", &debug_name)),
        )));
        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
            PathBuf::from(format!("/usr/bin/.debug/{}.debug", &debug_name)),
        )));
        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
            PathBuf::from(format!("/usr/lib/debug/usr/bin/{}.debug", &debug_name)),
        )));

        if debug_name.ends_with(".pdb") && self.symbol_cache.is_some() {
            // We might find this pdb file with the help of a symbol server.
            // Construct a custom string to identify this pdb.
            let custom = format!(
                "symbolserver:{}/{}/{}",
                debug_name,
                debug_id.breakpad(),
                debug_name
            );
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Custom(custom)));
        }

        if let Some(path) = &libinfo.path {
            // Fall back to getting symbols from the binary itself.
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                path.into(),
            )));

            // For macOS system libraries, also consult the dyld shared cache.
            if path.starts_with("/usr/") || path.starts_with("/System/") {
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64h")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
            }
        }

        Ok(paths)
    }

    fn open_file(
        &'h self,
        location: &FileLocation,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>> {
        Box::pin(self.open_file_impl(location.clone()))
    }
}
