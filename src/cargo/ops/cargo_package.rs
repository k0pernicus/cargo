use std::io::prelude::*;
use std::fs::{self, File};
use std::path::{self, Path, PathBuf};

use semver::VersionReq;
use tar::Archive;
use flate2::{GzBuilder, Compression};
use flate2::read::GzDecoder;

use core::{SourceId, Package, PackageId};
use core::dependency::Kind;
use sources::PathSource;
use util::{self, CargoResult, human, internal, ChainError, Config};
use ops;

struct Bomb { path: Option<PathBuf> }

impl Drop for Bomb {
    fn drop(&mut self) {
        match self.path.as_ref() {
            Some(path) => { let _ = fs::remove_file(path); }
            None => {}
        }
    }
}

pub fn package(manifest_path: &Path,
               config: &Config,
               verify: bool,
               list: bool,
               metadata: bool) -> CargoResult<Option<PathBuf>> {
    let mut src = try!(PathSource::for_path(manifest_path.parent().unwrap(),
                                            config));
    let pkg = try!(src.root_package());

    if metadata {
        try!(check_metadata(&pkg, config));
    }

    try!(check_dependencies(&pkg, config));

    if list {
        let root = pkg.root();
        let mut list: Vec<_> = try!(src.list_files(&pkg)).iter().map(|file| {
            util::without_prefix(&file, &root).unwrap().to_path_buf()
        }).collect();
        list.sort();
        for file in list.iter() {
            println!("{}", file.display());
        }
        return Ok(None)
    }

    let filename = format!("package/{}-{}.crate", pkg.name(), pkg.version());
    let target_dir = config.target_dir(&pkg);
    let dst = target_dir.join(&filename);
    if fs::metadata(&dst).is_ok() { return Ok(Some(dst)) }

    let mut bomb = Bomb { path: Some(dst.clone()) };

    try!(config.shell().status("Packaging", pkg.package_id().to_string()));
    try!(tar(&pkg, &src, config, &dst).chain_error(|| {
        human("failed to prepare local package for uploading")
    }));
    if verify {
        try!(run_verify(config, &pkg, &dst).chain_error(|| {
            human("failed to verify package tarball")
        }))
    }
    Ok(Some(bomb.path.take().unwrap()))
}

// check that the package has some piece of metadata that a human can
// use to tell what the package is about.
#[allow(deprecated)] // connect => join in 1.3
fn check_metadata(pkg: &Package, config: &Config) -> CargoResult<()> {
    let md = pkg.manifest().metadata();

    let mut missing = vec![];

    macro_rules! lacking {
        ($( $($field: ident)||* ),*) => {{
            $(
                if $(md.$field.as_ref().map_or(true, |s| s.is_empty()))&&* {
                    $(missing.push(stringify!($field).replace("_", "-"));)*
                }
            )*
        }}
    }
    lacking!(description, license || license_file, documentation || homepage || repository);

    if !missing.is_empty() {
        let mut things = missing[..missing.len() - 1].connect(", ");
        // things will be empty if and only if length == 1 (i.e. the only case
        // to have no `or`).
        if !things.is_empty() {
            things.push_str(" or ");
        }
        things.push_str(&missing.last().unwrap());

        try!(config.shell().warn(
            &format!("warning: manifest has no {things}. \
                    See http://doc.crates.io/manifest.html#package-metadata for more info.",
                    things = things)))
    }
    Ok(())
}

// Warn about wildcard deps which will soon be prohibited on crates.io
#[allow(deprecated)] // connect => join in 1.3
fn check_dependencies(pkg: &Package, config: &Config) -> CargoResult<()> {
    let wildcard = VersionReq::parse("*").unwrap();

    let mut wildcard_deps = vec![];
    for dep in pkg.dependencies() {
        if dep.kind() != Kind::Development && dep.version_req() == &wildcard {
            wildcard_deps.push(dep.name());
        }
    }

    if !wildcard_deps.is_empty() {
        let deps = wildcard_deps.connect(", ");
        try!(config.shell().warn(
            "warning: some dependencies have wildcard (\"*\") version constraints. \
             On December 11th, 2015, crates.io will begin rejecting packages with \
             wildcard dependency constraints. See \
             http://doc.crates.io/crates-io.html#using-crates.io-based-crates \
             for information on version constraints."));
        try!(config.shell().warn(
            &format!("dependencies for these crates have wildcard constraints: {}", deps)));
    }
    Ok(())
}

fn tar(pkg: &Package, src: &PathSource, config: &Config,
       dst: &Path) -> CargoResult<()> {

    if fs::metadata(&dst).is_ok() {
        return Err(human(format!("destination already exists: {}",
                                 dst.display())))
    }

    try!(fs::create_dir_all(dst.parent().unwrap()));

    let tmpfile = try!(File::create(dst));

    // Prepare the encoder and its header
    let filename = Path::new(dst.file_name().unwrap());
    let encoder = GzBuilder::new().filename(try!(util::path2bytes(filename)))
                                  .write(tmpfile, Compression::Best);

    // Put all package files into a compressed archive
    let ar = Archive::new(encoder);
    let root = pkg.root();
    for file in try!(src.list_files(pkg)).iter() {
        if &**file == dst { continue }
        let relative = util::without_prefix(&file, &root).unwrap();
        try!(check_filename(relative));
        let relative = try!(relative.to_str().chain_error(|| {
            human(format!("non-utf8 path in source directory: {}",
                          relative.display()))
        }));
        let mut file = try!(File::open(file));
        try!(config.shell().verbose(|shell| {
            shell.status("Archiving", &relative)
        }));
        let path = format!("{}-{}{}{}", pkg.name(), pkg.version(),
                           path::MAIN_SEPARATOR, relative);
        try!(ar.append_file(&path, &mut file).chain_error(|| {
            internal(format!("could not archive source file `{}`", relative))
        }));
    }
    try!(ar.finish());
    Ok(())
}

fn run_verify(config: &Config, pkg: &Package, tar: &Path)
              -> CargoResult<()> {
    try!(config.shell().status("Verifying", pkg));

    let f = try!(GzDecoder::new(try!(File::open(tar))));
    let dst = pkg.root().join(&format!("target/package/{}-{}",
                                       pkg.name(), pkg.version()));
    if fs::metadata(&dst).is_ok() {
        try!(fs::remove_dir_all(&dst));
    }
    let mut archive = Archive::new(f);
    try!(archive.unpack(dst.parent().unwrap()));
    let manifest_path = dst.join("Cargo.toml");

    // When packages are uploaded to the registry, all path dependencies are
    // implicitly converted to registry-based dependencies, so we rewrite those
    // dependencies here.
    //
    // We also make sure to point all paths at `dst` instead of the previous
    // location that the package was originally read from. In locking the
    // `SourceId` we're telling it that the corresponding `PathSource` will be
    // considered updated and we won't actually read any packages.
    let registry = try!(SourceId::for_central(config));
    let precise = Some("locked".to_string());
    let new_src = try!(SourceId::for_path(&dst)).with_precise(precise);
    let new_pkgid = try!(PackageId::new(pkg.name(), pkg.version(), &new_src));
    let new_summary = pkg.summary().clone().map_dependencies(|d| {
        if !d.source_id().is_path() { return d }
        d.clone_inner().set_source_id(registry.clone()).into_dependency()
    });
    let mut new_manifest = pkg.manifest().clone();
    new_manifest.set_summary(new_summary.override_id(new_pkgid));
    let new_pkg = Package::new(new_manifest, &manifest_path);

    // Now that we've rewritten all our path dependencies, compile it!
    try!(ops::compile_pkg(&new_pkg, None, &ops::CompileOptions {
        config: config,
        jobs: None,
        target: None,
        features: &[],
        no_default_features: false,
        spec: &[],
        filter: ops::CompileFilter::Everything,
        exec_engine: None,
        release: false,
        mode: ops::CompileMode::Build,
        target_rustc_args: None,
    }));

    Ok(())
}

// It can often be the case that files of a particular name on one platform
// can't actually be created on another platform. For example files with colons
// in the name are allowed on Unix but not on Windows.
//
// To help out in situations like this, issue about weird filenames when
// packaging as a "heads up" that something may not work on other platforms.
fn check_filename(file: &Path) -> CargoResult<()> {
    let name = match file.file_name() {
        Some(name) => name,
        None => return Ok(()),
    };
    let name = match name.to_str() {
        Some(name) => name,
        None => {
            return Err(human(format!("path does not have a unicode filename \
                                      which may not unpack on all platforms: {}",
                                     file.display())))
        }
    };
    let bad_chars = ['/', '\\', '<', '>', ':', '"', '|', '?', '*'];
    for c in bad_chars.iter().filter(|c| name.contains(**c)) {
        return Err(human(format!("cannot package a filename with a special \
                                  character `{}`: {}", c, file.display())))
    }
    Ok(())
}
