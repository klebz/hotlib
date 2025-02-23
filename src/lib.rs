//! For watching and loading a Rust library in
//! Rust.
//!
//! You are likely looking for the [watch function
//! docs](./fn.watch.html).

use notify::Watcher as NotifyWatcher;
use notify::EventHandler;
use slug::slugify;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use thiserror::Error;

#[doc(inline)]
pub use libloading::{self, Library, Symbol};

/// Watches and re-builds the library upon changes
/// to its source code.
pub struct Watch {
    package_info: PackageInfo,
    _watcher:     notify::RecommendedWatcher,
    event_rx:     crossbeam_channel::Receiver<Result<notify::Event,notify::Error>>,
}

struct PackageInfo {
    manifest_path:   PathBuf,
    src_path:        PathBuf,
    lib_name:        String,
    target_dir_path: PathBuf,
}

/// The information required to build the
/// package's dylib target.
pub struct Package<'a> {
    info: &'a PackageInfo,
}

/// The result of building a package's dynamic
/// library.
///
/// This can be used to load the dynamic library
/// either in place or via a temporary file as to
/// allow for re-building the package while using
/// the library.
#[derive(Clone)]
pub struct Build {
    lib_name:        String,
    target_dir_path: PathBuf,
    timestamp:       SystemTime,
    output:          std::process::Output,
}

/// A wrapper around a `libloading::Library` that
/// cleans up the library on `Drop`.
#[derive(Debug)]
pub struct TempLibrary {
    build_timestamp: SystemTime,
    path:            PathBuf,

    // This is always `Some`. An `Option` is only
    // used so that the library may be `Drop`ped
    // during the `TempLibrary`'s `drop`
    // implementation before the temporary library
    // file at `path` is removed.
    lib:             Option<libloading::Library>,
}

#[derive(Debug)]
pub enum CreateTempLibraryError {
    CouldNotLoadDirectlyFromDylib {
        path:  PathBuf,
        error: LoadError
    },
    CannotGetMetadata {
        path:  PathBuf,
    },
    CannotGetFileCreationTime {
        path:     PathBuf,
        metadata: std::fs::Metadata,
    },
    LoadError {
        error: LoadError,
    }
}

impl TempLibrary {

    //#[tracing::instrument]
    pub fn new(dylib_path: &PathBuf, lib_name: &str) -> Result<Self,CreateTempLibraryError> {

        let metadata = dylib_path.metadata().map_err(|_err| {
            CreateTempLibraryError::CannotGetMetadata {
                path:  dylib_path.to_path_buf(),
            }
        })?;

        let build_timestamp = metadata.created().map_err(|_err| {
            CreateTempLibraryError::CannotGetFileCreationTime {
                path: dylib_path.to_path_buf(),
                metadata
            }
        })?;

        let tmp_path = Self::tmp_dylib_path(lib_name, &build_timestamp);
        let tmp_dir  = tmp_path.parent().expect("temp dylib path has no parent");

        std::fs::write("/tmp/fuckafucka", format!{"dmt at {:?}", tmp_path}).unwrap();

        // If the library already exists, load it.
        loop {

            if tmp_path.exists() {

                tracing::info!("creating Library from {:?}", tmp_path);

                // This is some voodoo to enable
                // reloading of dylib on mac os
                if cfg!(target_os = "macos") {

                    tracing::info!("running install_name_tool");

                    let output = std::process::Command::new("install_name_tool")
                        .current_dir(tmp_dir)
                        .arg("-id")
                        .arg("''")
                        .arg(
                            tmp_path
                                .file_name()
                                .expect("temp dylib path has no file name"),
                        )
                        .output()
                        .expect("ls command failed to start");

                        tracing::info!("install_name_tool output status: {}", output.status);
                        tracing::info!("install_name_tool output stdout: {}", String::from_utf8(output.stdout).unwrap());
                        tracing::info!("install_name_tool output stdout: {}", String::from_utf8(output.stderr).unwrap());

                        if !output.status.success() {
                            tracing::info!("ERROR: install_name_tool failed!");
                        }
                }

                let lib = libloading::Library::new(dylib_path)
                    .map(Some)
                    .map_err(
                        |err| CreateTempLibraryError::CouldNotLoadDirectlyFromDylib {
                            path:  dylib_path.to_path_buf(),
                            error: LoadError::Library { err }
                        }
                    )?;

                return Ok(
                    TempLibrary {
                        build_timestamp,
                        path: tmp_path.clone(),
                        lib,
                    }
                );
            }

            // Copy the dylib to the tmp location.
            std::fs::create_dir_all(tmp_dir)
                .map_err(|err| 
                    CreateTempLibraryError::LoadError {
                        error: LoadError::Io { err }
                    }
                )?;

            std::fs::copy(&dylib_path, &tmp_path)
                .map_err(|err| 
                    CreateTempLibraryError::LoadError {
                        error: LoadError::Io { err }
                    }
                )?;
        }
    }

    /// The path to the temporary dynamic library
    /// clone that will be created upon `load`.
    fn tmp_dylib_path(lib_name: &str, build_timestamp: &SystemTime) -> PathBuf {
        tmp_dir()
            .join(Self::tmp_file_stem(lib_name, build_timestamp))
            .with_extension(dylib_ext())
    }

    fn tmp_file_stem(lib_name: &str, build_timestamp: &SystemTime) -> String {
        let timestamp_slug = slugify(format!("{}", humantime::format_rfc3339(*build_timestamp)));
        format!("{}-{}", Self::file_stem(lib_name), timestamp_slug)
    }

    fn file_stem(lib_name: &str) -> String {

        // TODO: On windows, the generated lib
        // does not contain the "lib" prefix.
        //
        // A proper solution would likely involve
        // retrieving the file stem from cargo
        // itself.
        #[cfg(target_os = "windows")]
        {
            format!("{}", lib_name)
        }

        #[cfg(not(target_os = "windows"))]
        {
            format!("lib{}", lib_name)
        }
    }
}

/// Errors that might occur within the `watch` function.
#[derive(Debug, Error)]
pub enum WatchError {

    #[error("invalid path: expected path to end with `Cargo.toml`")]
    InvalidPath,

    #[error("an IO error occurred while attempting to invoke `cargo metadata`: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },

    #[error("{err}")]
    ExitStatusUnsuccessful {
        #[from]
        err: ExitStatusUnsuccessfulError,
    },

    #[error("an error occurred when attempting to read cargo stdout as json: {err}")]
    Json {
        #[from]
        err: serde_json::Error,
    },

    #[error("no dylib targets were found within the given cargo package")]
    NoDylibTarget,

    #[error("failed to construct `notify::RecommendedWatcher`: {err}")]
    Notify {
        #[from]
        err: notify::Error,
    },
}

/// Errors that might occur while building
/// a library instance.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("an IO error occurred while attempting to invoke cargo: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },
    #[error("{err}")]
    ExitStatusUnsuccessful {
        #[from]
        err: ExitStatusUnsuccessfulError,
    },
}

/// A process' output indicates unsuccessful
/// completion.
#[derive(Debug, Error)]
#[error("cargo process exited unsuccessfully with status code: {code:?}: {stderr}")]
pub struct ExitStatusUnsuccessfulError {
    pub code: Option<i32>,
    pub stderr: String,
}

/// Errors that might occur while waiting for the
/// next library instance.
#[derive(Debug, Error)]
pub enum NextError {
    #[error("the channel used to receive file system events was closed")]
    ChannelClosed,
    #[error("a notify event signalled an error: {err}")]
    Notify {
        #[from]
        err: notify::Error,
    },
}

/// Errors that might occur while loading a built
/// library.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("an IO error occurred: {err}")]
    Io {
        #[from]
        err: std::io::Error,
    },
    #[error("failed to load library with libloading: {err}")]
    Library {
        #[from]
        err: libloading::Error,
    },
}

impl ExitStatusUnsuccessfulError {
    /// Produces the error if output indicates failure.
    pub fn from_output(output: &std::process::Output) -> Option<Self> {
        // Check for process failure.
        if !output.status.success() {
            let code = output.status.code();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Some(ExitStatusUnsuccessfulError { code, stderr });
        }
        None
    }
}

/// Watch the library at the given `Path`.
///
/// The given `Path` should point to the
/// `Cargo.toml` of the package used to build the
/// library.
///
/// When a library is being "watched", the library
/// will be re-built any time some filesystem
/// event occurs within the library's source
/// directory. The target used is the first
/// "dylib" discovered within the package.
///
/// The `notify` crate is used to watch for
/// file-system events in a cross-platform manner.
pub fn watch(path: &Path) -> Result<Watch, WatchError> {

    if !path.ends_with("Cargo.toml") && !path.ends_with("cargo.toml") {
        return Err(WatchError::InvalidPath);
    }

    // Run the `cargo metadata` command to
    // retrieve JSON containing lib target info.
    let manifest_path_str = format!("{}", path.display());

    let output = std::process::Command::new("cargo")
        .arg("metadata")
        .arg("--manifest-path")
        .arg(&manifest_path_str)
        .arg("--format-version")
        .arg("1")
        .output()?;

    // Check the exit status.
    if let Some(err) = ExitStatusUnsuccessfulError::from_output(&output) {
        return Err(WatchError::from(err));
    }

    // Read the stdout as JSON.
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    // A function to read paths and name out of JSON.
    let read_json = |json: &serde_json::Value| -> Option<(PathBuf, PathBuf, String)> {
        let obj = json.as_object()?;

        // Retrieve the target directory.
        let target_dir_str = obj.get("target_directory")?.as_str()?;
        let target_dir_path = Path::new(target_dir_str).to_path_buf();

        // Retrieve the first package as an object.
        let pkgs = obj.get("packages")?.as_array()?;

        // Find the package with the matching manifest.
        let pkg = pkgs.iter().find_map(|pkg| {
            let s = pkg.get("manifest_path")?.as_str()?;
            match s == manifest_path_str {
                true => Some(pkg),
                false => None,
            }
        })?;

        // Search the targets for one containing a dylib output.
        let targets = pkg.get("targets")?.as_array()?;
        let target = targets.iter().find_map(|target| {
            let kind = target.get("kind")?.as_array()?;
            if kind.iter().find(|k| k.as_str() == Some("dylib")).is_some() {
                return Some(target);
            } else {
                None
            }
        })?;

        // Target name and src path.
        let lib_name = target.get("name")?.as_str()?.to_string();
        let src_root_str = target.get("src_path")?.as_str()?;
        let src_root_path = Path::new(src_root_str).to_path_buf();

        Some((target_dir_path, src_root_path, lib_name))
    };

    let (target_dir_path, src_root_path, lib_name) =
        read_json(&json).ok_or(WatchError::NoDylibTarget)?;
    let src_dir_path = src_root_path
        .parent()
        .expect("src root has no parent directory");

    // Begin watching the src path.
    let (tx, event_rx) = crossbeam_channel::unbounded();

    let sender = ChannelSender(tx);

    let mut watcher = notify::recommended_watcher(sender)?;
    watcher.watch(src_dir_path, notify::RecursiveMode::Recursive)?;

    // Collect the paths.
    let manifest_path = path.to_path_buf();
    let src_path = src_dir_path.to_path_buf();

    // Collect the package info.
    let package_info = PackageInfo {
        manifest_path,
        src_path,
        lib_name,
        target_dir_path,
    };

    Ok(Watch {
        package_info,
        _watcher: watcher,
        event_rx,
    })
}

//------------------------[these are for `recommended_watcher`]
type ChannelMessage   = Result<notify::Event, notify::Error>;
type ChannelSendError =  crossbeam_channel::SendError<ChannelMessage>;

struct ChannelSender(crossbeam_channel::Sender<ChannelMessage>);

impl ChannelSender {
    pub fn send(&mut self, msg: ChannelMessage) -> Result<(), ChannelSendError> {
        self.0.send(msg)
    }
}

impl EventHandler for ChannelSender {
    fn handle_event(&mut self, event: Result<notify::Event, notify::Error>) {
        let _ = self.send(event);
    }
}

impl Watch {

    /// The path to the package's `Cargo.toml`.
    pub fn manifest_path(&self) -> &Path {
        &self.package_info.manifest_path
    }

    /// The path to the source directory being
    /// watched.
    pub fn src_path(&self) -> &Path {
        &self.package_info.src_path
    }

    /// Wait for the library to be re-built after
    /// some change.
    pub fn next(&self) -> Result<Package, NextError> {
        loop {
            let event = match self.event_rx.recv() {
                Err(_) => return Err(NextError::ChannelClosed),
                Ok(event) => event,
            };

            if check_raw_event(event?)? {
                return Ok(self.package());
            }
        }
    }

    /// The same as `next`, but returns early if
    /// there are no pending events.
    pub fn try_next(&self) -> Result<Option<Package>, NextError> {
        for event in self.event_rx.try_iter() {
            if check_raw_event(event?)? {
                return Ok(Some(self.package()));
            }
        }
        Ok(None)
    }

    /// Manually retrieve the library's package
    /// immediately without checking for file
    /// events.
    ///
    /// This is useful for triggering an initial
    /// build during model initialisation.
    pub fn package(&self) -> Package {
        let info = &self.package_info;
        Package { info }
    }
}

impl<'a> Package<'a> {

    /// The path to the package's `Cargo.toml`.
    pub fn manifest_path(&self) -> &Path {
        &self.info.manifest_path
    }

    /// The path to the source directory being watched.
    pub fn src_path(&self) -> &Path {
        &self.info.src_path
    }

    /// Builds the package's dynamic library target.
    pub fn build(&self) -> Result<Build, BuildError> {
        let PackageInfo {
            ref manifest_path,
            ref lib_name,
            ref target_dir_path,
            ..
        } = self.info;

        // Tell cargo to compile the package.
        let manifest_path_str = format!("{}", manifest_path.display());
        let output = std::process::Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(&manifest_path_str)
            .arg("--lib")
            .arg("--release")
            .output()?;

        // Check the exit status.
        if let Some(err) = ExitStatusUnsuccessfulError::from_output(&output) {
            return Err(BuildError::from(err));
        }

        // Time stamp the moment of build completion.
        let timestamp = SystemTime::now();

        Ok(Build {
            timestamp,
            output,
            lib_name:        lib_name.to_string(),
            target_dir_path: target_dir_path.to_path_buf(),
        })
    }
}

impl Build {

    /// The output of the cargo process.
    pub fn cargo_output(&self) -> &std::process::Output {
        &self.output
    }

    /// The moment at which the build was completed.
    pub fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    /// The path to the generated dylib target.
    pub fn dylib_path(&self) -> PathBuf {
        let file_stem = self.file_stem();
        self.target_dir_path
            .join("release")
            .join(file_stem)
            .with_extension(dylib_ext())
    }

    /// The path to the temporary dynamic library
    /// clone that will be created upon `load`.
    pub fn tmp_dylib_path(&self) -> PathBuf {
        tmp_dir()
            .join(self.tmp_file_stem())
            .with_extension(dylib_ext())
    }

    /// Copy the library to the platform's
    /// temporary directory and load it from
    /// there.
    ///
    /// Note that the copied dynamic library will
    /// be removed on `Drop`.
    pub fn load(&self) -> Result<TempLibrary, LoadError> {

        let dylib_path = self.dylib_path();
        let tmp_path   = self.tmp_dylib_path();
        let tmp_dir    = tmp_path.parent().expect("temp dylib path has no parent");

        // If the library already exists, load it.
        loop {

            if tmp_path.exists() {

                // This is some voodoo to enable
                // reloading of dylib on mac os
                if cfg!(target_os = "macos") {
                    std::process::Command::new("install_name_tool")
                        .current_dir(tmp_dir)
                        .arg("-id")
                        .arg("''")
                        .arg(
                            tmp_path
                                .file_name()
                                .expect("temp dylib path has no file name"),
                        )
                        .output()
                        .expect("ls command failed to start");
                }

                let lib = libloading::Library::new(&tmp_path)
                    .map(Some)
                    .map_err(|err| LoadError::Library { err })?;
                let path = tmp_path;
                let build_timestamp = self.timestamp;
                let tmp = TempLibrary {
                    build_timestamp,
                    path,
                    lib,
                };
                return Ok(tmp);
            }

            // Copy the dylib to the tmp location.
            std::fs::create_dir_all(tmp_dir).map_err(|err| LoadError::Io { err })?;
            std::fs::copy(&dylib_path, &tmp_path).map_err(|err| LoadError::Io { err })?;
        }
    }

    /// Load the library from it's existing
    /// location.
    ///
    /// Note that if you do this, you will have to
    /// ensure the returned `Library` is dropped
    /// before attempting to re-build the library.
    pub fn load_in_place(self) -> Result<libloading::Library, libloading::Error> {
        let dylib_path = self.dylib_path();
        libloading::Library::new(dylib_path)
    }

    // The file stem of the built dynamic library.
    fn file_stem(&self) -> String {

        // TODO: On windows, the generated lib
        // does not contain the "lib" prefix.
        //
        // A proper solution would likely involve
        // retrieving the file stem from cargo
        // itself.
        #[cfg(target_os = "windows")]
        {
            format!("{}", self.lib_name)
        }

        #[cfg(not(target_os = "windows"))]
        {
            format!("lib{}", self.lib_name)
        }
    }

    // Produce the file stem for the temporary
    // dynamic library clone that will be created
    // upon `load`.
    fn tmp_file_stem(&self) -> String {
        let timestamp_slug = slugify(format!("{}", humantime::format_rfc3339(self.timestamp)));
        format!("{}-{}", self.file_stem(), timestamp_slug)
    }
}

impl TempLibrary {

    /// The inner `libloading::Library`.
    ///
    /// This may also be accessed via the `Deref`
    /// implementation.
    pub fn lib(&self) -> &libloading::Library {
        self.lib
            .as_ref()
            .expect("lib should always be `Some` until `Drop`")
    }

    /// The time at which the original library was
    /// built.
    pub fn build_timestamp(&self) -> SystemTime {
        self.build_timestamp
    }

    /// The path at which the loaded temporary
    /// library is located.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempLibrary {
    type Target = libloading::Library;
    fn deref(&self) -> &Self::Target {
        self.lib()
    }
}

impl Drop for TempLibrary {
    fn drop(&mut self) {
        tracing::info!("dropping {:?}",self);
        std::mem::drop(self.lib.take());
        std::fs::remove_file(&self.path).ok();
    }
}

// The temporary directory used by hotlib.
fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join("hotlib")
}

// Whether or not the given event should trigger
// a rebuild.
fn _check_event(_event: notify::Event) -> bool {
    true
}

// Whether or not the given event should trigger
// a rebuild.
fn check_raw_event(event: notify::Event) -> Result<bool, NextError> {

    use notify::event::*;

    let kind = &event.kind;

    let close_write = match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        _ => false,
    };

    Ok(
        kind.is_create() 
        || kind.is_remove()
        || kind.is_modify()
        || close_write
    )
}

// Get the dylib extension for this platform.
//
// TODO: This should be exposed from cargo.
fn dylib_ext() -> &'static str {

    #[cfg(target_os = "linux")]
    {
        return "so";
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        return "dylib";
    }

    #[cfg(target_os = "windows")]
    {
        return "dll";
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "windows"
    )))]
    {
        panic!("unknown dynamic library for this platform")
    }
}
