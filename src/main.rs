#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate curl;
extern crate tar;
extern crate flate2;
extern crate semver;
extern crate toml;
extern crate tempdir;

use std::collections::{HashSet, BTreeMap};
use std::fs::{self, File};
use std::path::{Path};
use std::process::Command;
use std::io::{Read, Write};
use std::str;

const PREFIX: &str = "rustc-ap";

fn main() {
    println!("Learning rustc's version");
    let output = Command::new("rustc")
        .arg("+nightly")
        .arg("-vV")
        .arg("sysroot")
        .output()
        .expect("failed to spawn rustc");
    if !output.status.success() {
        panic!("failed to run rustc: {:?}", output);
    }

    let output = str::from_utf8(&output.stdout).unwrap();
    let commit = output.lines()
        .find(|l| l.starts_with("commit-hash"))
        .expect("failed to find commit hash")
        .split(' ')
        .nth(1)
        .unwrap();

    let tmpdir = tempdir::TempDir::new("foo").unwrap();
    let tmpdir = tmpdir.path();
    let dst = tmpdir.join(format!("rust-{}", commit));
    let ok = dst.join(".ok");
    if !ok.exists() {
        download_src(&tmpdir, commit);
    }

    println!("learning about the dependency graph");
    let metadata = Command::new("cargo")
        .arg("+nightly")
        .current_dir(dst.join("src/libsyntax"))
        .arg("metadata")
        .arg("--format-version=1")
        .output()
        .expect("failed to execute cargo");
    if !metadata.status.success() {
        panic!("failed to run rustc: {:?}", metadata);
    }
    let output = str::from_utf8(&metadata.stdout).unwrap();
    let output: Metadata = serde_json::from_str(output).unwrap();

    let syntax = output.packages
        .iter()
        .find(|p| p.name == "syntax")
        .expect("failed to find libsyntax");

    let mut crates = Vec::new();
    fill(&output, &syntax, &mut crates, &mut HashSet::new());

    let version_to_publish = get_version_to_publish();
    println!("going to publish {}", version_to_publish);

    for p in crates.iter() {
        publish(p, &commit, &version_to_publish);
    }
}

fn download_src(dst: &Path, commit: &str) {
    println!("downloading source tarball");
    let mut easy = curl::easy::Easy::new();

    let url = format!("https://github.com/rust-lang/rust/archive/{}.tar.gz",
                      commit);
    easy.get(true).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        }).unwrap();
        t.perform().unwrap();
    }
    assert_eq!(easy.response_code().unwrap(), 200);
    let mut archive = tar::Archive::new(flate2::bufread::GzDecoder::new(&data[..]));
    archive.unpack(dst).unwrap();

    let root = dst.join(format!("rust-{}", commit));
    fs::rename(root.join("src/Cargo.toml"), root.join("src/Cargo.toml.bk")).unwrap();

    File::create(&root.join(".ok")).unwrap();
}

fn fill<'a>(output: &'a Metadata,
            pkg: &'a Package,
            pkgs: &mut Vec<&'a Package>,
            seen: &mut HashSet<&'a str>) {
    if !seen.insert(&pkg.name) {
        return
    }
    let node = output.resolve.nodes
        .iter()
        .find(|n| n.id == pkg.id)
        .expect("failed to find resolve node for package");
    for dep in node.dependencies.iter() {
        let pkg = output.packages.iter().find(|p| p.id == *dep).unwrap();
        if pkg.source.is_none() {
            fill(output, pkg, pkgs, seen);
        }
    }
    pkgs.push(pkg);
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    source: Option<String>,
    manifest_path: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<ResolveNode>,
}

#[derive(Deserialize)]
struct ResolveNode {
    id: String,
    dependencies: Vec<String>,
}

fn get_version_to_publish() -> semver::Version {
    let mut cur = get_current_version();
    cur.major += 1;
    return cur
}

fn get_current_version() -> semver::Version {
    println!("fetching current version");
    let mut easy = curl::easy::Easy::new();

    let url = format!("https://crates.io/api/v1/crates/{}-syntax", PREFIX);
    easy.get(true).unwrap();
    easy.url(&url).unwrap();
    easy.follow_location(true).unwrap();
    let mut data = Vec::new();
    {
        let mut t = easy.transfer();
        t.write_function(|d| {
            data.extend_from_slice(d);
            Ok(d.len())
        }).unwrap();
        t.perform().unwrap();
    }
    if easy.response_code().unwrap() == 404 {
        return semver::Version::parse("0.0.0").unwrap()
    }

    assert_eq!(easy.response_code().unwrap(), 200);

    let output: Output = serde_json::from_slice(&data).unwrap();

    return output.krate.max_version;

    #[derive(Deserialize)]
    struct Output {
        #[serde(rename = "crate")]
        krate: Crate,
    }

    #[derive(Deserialize)]
    struct Crate {
        max_version: semver::Version,
    }
}

fn publish(pkg: &Package, commit: &str, vers: &semver::Version) {
    println!("publishing {} {}", pkg.name, vers);

    let mut toml = String::new();
    File::open(&pkg.manifest_path).unwrap()
        .read_to_string(&mut toml).unwrap();
    let mut toml: toml::Value = toml.parse().unwrap();
    {
        let toml = toml.as_table_mut().unwrap();

        if let Some(p) = toml.get_mut("package") {
            let p = p.as_table_mut().unwrap();

            // Update the package's name and version to be consistent with what
            // we're publishing, which is a new version of these two and isn't
            // what is actually written down.
            let name = format!("{}-{}", PREFIX, pkg.name);
            p.insert("name".to_string(), name.into());
            p.insert("version".to_string(), vers.to_string().into());

            // Fill in some other metadata which isn't listed currently and
            // helps the crates published be consistent.
            p.insert("license".to_string(), "MIT / Apache-2.0".to_string().into());
            p.insert("description".to_string(), format!("\
                Automatically published version of the package `{}` \
                in the rust-lang/rust repository from commit {} \
            ", pkg.name, commit).into());
            p.insert(
                "repository".to_string(),
                "https://github.com/rust-lang/rust".to_string().into(),
            );
        }

        // Fill in the `[lib]` section with an extra `name` key indicating the
        // original name (so the crate name is right). Also remove `crate-type`
        // so it's not compiled as a dylib.
        if let Some(lib) = toml.get_mut("lib") {
            let lib = lib.as_table_mut().unwrap();
            let name = pkg.name.to_string();
            lib.insert("name".to_string(), toml::Value::String(name));
            lib.remove("crate-type");
        }

        // A few changes to dependencies:
        //
        // * Remove `path` dependencies, changing them to crates.io dependencies
        //   at the `vers` specified above
        // * Update the name of `path` dependencies to what we're publishing,
        //   which is crates with a prefix.
        // * Synthesize a dependency on `term`. Currently crates depend on
        //   `term` through the sysroot instead of via `Cargo.toml`, so we need
        //   to change that for the published versions.
        if let Some(deps) = toml.remove("dependencies") {
            let mut deps = deps.as_table().unwrap().iter().map(|(name, dep)| {
                let table = match dep.as_table() {
                    Some(s) if s.contains_key("path") => s,
                    _ => return (name.clone(), dep.clone()),
                };
                let mut new_table = BTreeMap::new();
                for (k, v) in table {
                    if k != "path" {
                        new_table.insert(k.to_string(), v.clone());
                    }
                }
                new_table.insert(
                    "version".to_string(),
                    toml::Value::String(vers.to_string()),
                );
                (format!("{}-{}", PREFIX, name), new_table.into())
            }).collect::<Vec<_>>();
            deps.push(("term".to_string(), "0.4".to_string().into()));
            toml.insert(
                "dependencies".to_string(),
                toml::Value::Table(deps.into_iter().collect()),
            );
        }
    }

    let toml = toml.to_string();
    File::create(&pkg.manifest_path).unwrap()
        .write_all(toml.as_bytes()).unwrap();

    let path = Path::new(&pkg.manifest_path).parent().unwrap();

    alter_lib_rs(path);

    let result = Command::new("cargo")
        .arg("+nightly")
        .arg("publish")
        .arg("--allow-dirty")
        .arg("--no-verify")
        .current_dir(path)
        .status()
        .expect("failed to spawn cargo");
    assert!(result.success());
}

// TODO: this function shouldn't be necessary, we can change upstream libsyntax
//       to not need these modifications.
fn alter_lib_rs(path: &Path) {
    let lib = path.join("lib.rs");
    if !lib.exists() {
        return
    }
    let mut contents = String::new();
    File::open(&lib).unwrap()
        .read_to_string(&mut contents).unwrap();

    // Inject #![feature(rustc_private)]. This is a hack, let's fix upstream so
    // we don't have to do this.
    let needle = "\n#![feature(";
    if let Some(i) = contents.find(needle) {
        contents.insert_str(i + needle.len(), "rustc_private, ");
    }

    // Delete __build_diagnostic_array!. This is a hack, let's fix upstream so
    // we don't have to do this.
    if let Some(i) = contents.find("__build_diagnostic_array! {") {
        contents.truncate(i);
        contents.push_str("fn _foo() {}\n");
    }

    File::create(&lib).unwrap()
        .write_all(contents.as_bytes()).unwrap()
}
