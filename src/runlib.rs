//! A tool that functionaries can use to create link metadata about a step.

use path_clean::clean;
use std::collections::{BTreeMap, HashSet};
use std::fs::{canonicalize as canonicalize_path, symlink_metadata, File};
use std::io::{self, BufReader, Write};
use std::process::Command;
use walkdir::WalkDir;

use crate::crypto::HashAlgorithm;
use crate::models::{Link, TargetDescription};
use crate::{
    crypto,
    crypto::PrivateKey,
    models::{LinkMetadataBuilder, VirtualTargetPath},
};
use crate::{Error, Result};

// TODO: improve doc comments :p

/// record_artifact is a function that reads and hashes an artifact given its path as a string literal,
/// returning the VirtualTargetPath and TargetDescription of the file as a tuple, wrapped in Result.
pub fn record_artifact(
    path: &str,
    hash_algorithms: &[HashAlgorithm],
) -> Result<(VirtualTargetPath, TargetDescription)> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let (_length, hashes) = crypto::calculate_hashes(&mut reader, hash_algorithms)?;
    Ok((VirtualTargetPath::new(String::from(path))?, hashes))
}

/// record_artifacts is a function that traverses through the passed slice of paths, hashes the content of files
/// encountered, and returns the path and hashed content in `BTreeMap` format, wrapped in Result.
/// If a step in record_artifact fails, the error is returned.
/// If no `hash_algorithms` are supplied, `Sha256` is assumed and used.
/// If a symbolic link cycle is detected in the material or product paths, it is skipped.
pub fn record_artifacts(
    paths: &[&str],
    hash_algorithms: Option<&[&str]>,
) -> Result<BTreeMap<VirtualTargetPath, TargetDescription>> {
    // Verify hash_algorithms inputs are valid
    let hash_mapping = HashAlgorithm::get_hash_mapping();
    let hash_algorithms = match hash_algorithms {
        Some(hashes) => {
            let mut map = vec![];
            for hash in hashes {
                if !hash_mapping.contains_key(*hash) {
                    return Err(Error::UnkonwnHashAlgorithm((*hash).to_string()));
                }
                let value = hash_mapping.get(*hash).unwrap();
                map.push(value.clone());
            }
            map
        }
        None => vec![HashAlgorithm::Sha256],
    };
    let hash_algorithms = &hash_algorithms[..];

    // Initialize artifacts
    let mut artifacts: BTreeMap<VirtualTargetPath, TargetDescription> = BTreeMap::new();
    // For each path provided, walk the directory and add all files to artifacts
    for path in paths {
        let mut walker = WalkDir::new(path).follow_links(true).into_iter();
        let mut visited_sym_links = HashSet::new();
        loop {
            let path = match walker.next() {
                Some(entry) => dir_entry_to_path(entry)?,
                None => break,
            };
            let file_type = std::fs::symlink_metadata(&path)?.file_type();
            // If entry is a symlink, check it's unvisited. If so, continue.
            if file_type.is_symlink() {
                if visited_sym_links.contains(&path) {
                    walker.skip_current_dir();
                } else {
                    visited_sym_links.insert(String::from(&path));
                    // s_path: the actual path the symbolic link is pointing to
                    let s_path = match std::fs::read_link(&path)?.as_path().to_str() {
                        Some(str) => String::from(str),
                        None => break,
                    };
                    if symlink_metadata(&s_path)?.file_type().is_file() {
                        let (virtual_target_path, hashes) =
                            record_artifact(&path, hash_algorithms)?;
                        artifacts.insert(virtual_target_path, hashes);
                    }
                }
            }
            // If entry is a file, open and hash the file
            if file_type.is_file() {
                let (virtual_target_path, hashes) = record_artifact(&path, hash_algorithms)?;
                artifacts.insert(virtual_target_path, hashes);
            }
        }
    }
    Ok(artifacts)
}

/// run_command is a function that, given command arguments, executes commands on a software supply chain step
/// and returns the stdout and stderr as byproducts.
/// The first element of cmd_args is used as executable and the rest as command arguments.
/// If a commands in run_command fails to execute, the error is returned.
pub fn run_command(cmd_args: &[&str], run_dir: Option<&str>) -> Result<BTreeMap<String, String>> {
    let executable = cmd_args[0];
    let args = (&cmd_args[1..])
        .iter()
        .map(|arg| {
            if VirtualTargetPath::new((*arg).into()).is_ok() {
                let absolute_path = canonicalize_path(*arg);
                match absolute_path {
                    Ok(path_buf) => match path_buf.to_str() {
                        Some(p) => p,
                        None => *arg,
                    },
                    Err(_) => *arg,
                };
            }
            *arg
        })
        .collect::<Vec<&str>>();

    let mut cmd = Command::new(executable);
    let mut cmd = cmd.args(args);

    if let Some(dir) = run_dir {
        cmd = cmd.current_dir(dir)
    }

    let output = cmd.output()?;

    // Emit stdout, stderror
    io::stdout().write_all(&output.stdout)?;
    io::stderr().write_all(&output.stderr)?;

    // Format output into Byproduct
    let mut byproducts: BTreeMap<String, String> = BTreeMap::new();
    // Write to byproducts
    let stdout = match String::from_utf8(output.stdout) {
        Ok(output) => output,
        Err(error) => {
            return Err(Error::from(io::Error::new(
                std::io::ErrorKind::Other,
                format!("Utf8Error: {}", error),
            )))
        }
    };
    let stderr = match String::from_utf8(output.stderr) {
        Ok(output) => output,
        Err(error) => {
            return Err(Error::from(io::Error::new(
                std::io::ErrorKind::Other,
                format!("Utf8Error: {}", error),
            )))
        }
    };
    let status = match output.status.code() {
        Some(code) => code.to_string(),
        None => "Process terminated by signal".to_string(),
    };

    byproducts.insert("stdout".to_string(), stdout);
    byproducts.insert("stderr".to_string(), stderr);
    byproducts.insert("return-value".to_string(), status);

    Ok(byproducts)
}

// TODO: implement default trait for in_toto_run's parameters

/// in_toto_run is a function that executes commands on a software supply chain step
/// (layout inspection coming soon), then generates and returns its corresponding Link metadata.
/// If a symbolic link cycle is detected in the material or product paths, it is skipped.
pub fn in_toto_run(
    name: &str,
    run_dir: Option<&str>,
    material_paths: &[&str],
    product_paths: &[&str],
    cmd_args: &[&str],
    key: Option<PrivateKey>,
    hash_algorithms: Option<&[&str]>,
    // env: Option<BTreeMap<String, String>>
) -> Result<Link> {
    // Record Materials: Given the material_paths, recursively traverse and record files in given path(s)
    let materials = record_artifacts(material_paths, hash_algorithms)?;

    // Execute commands provided in cmd_args
    let byproducts = run_command(cmd_args, run_dir)?;

    // Record Products: Given the product_paths, recursively traverse and record files in given path(s)
    let products = record_artifacts(product_paths, hash_algorithms)?;

    // Create link based on values collected above
    let link_metadata_builder = LinkMetadataBuilder::new()
        .name(name.to_string())
        .materials(materials)
        .byproducts(byproducts)
        .products(products);
    let link_metadata = link_metadata_builder.build()?;

    // TODO Sign the link with key param supplied. If no key param supplied, build & return link
    // If no key is found, dump out SignedMetadata with no signatures (for inspections)
    /* match key {
        Some(k)   => {
            // TODO: SignedMetadata and Link are different types. Need to consolidate
            let signed_link = link_metadata_builder.signed::<Json>(&k).unwrap();
            let json = serde_json::to_value(&signed_link).unwrap();
        },
        None => {
        }
    } */
    Link::from(&link_metadata)
}

// Helper functions specific to the runlib goes here

/// dir_entry_to_path is a function that, given a DirEntry, return the entry's path as a String
/// wrapped in Result. If the entry's path is invalid, error is returned.
fn dir_entry_to_path(
    entry: std::result::Result<walkdir::DirEntry, walkdir::Error>,
) -> Result<String> {
    let path = match entry {
        Ok(dir_entry) => match dir_entry.path().to_str() {
            Some(str) => String::from(str),
            None => {
                return Err(Error::Programming(format!(
                    "Invalid Path {}; non-UTF-8 string",
                    dir_entry.path().display()
                )))
            }
        },
        // If WalkDir errored, check if it's due to a symbolic link loop sighted,
        // if so, override the error and continue using the symbolic link path.
        // If this doesn't work, something hacky to consider would be reinvoking WalkDir
        // using the error_path as root.

        // Current behavior: when symbolic link is a directory and directly loops to parent,
        // it skips the symbolic link recording.
        // If this is not the desired behavior and we want to record the symbolic link's content
        // , we can probably do it in a hacky way by recursively calling record_artifacts and
        // extending the results to artifacts variable.
        Err(error) => {
            if error.loop_ancestor().is_some() {
                match error.path() {
                    None => {
                        return Err(Error::from(io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Walkdir Error: {}", error),
                        )))
                    }
                    Some(error_path) => {
                        let sym_path = match error_path.to_str() {
                            Some(str) => String::from(str),
                            None => {
                                return Err(Error::Programming(format!(
                                    "Invalid Path {}; non-UTF-8 string",
                                    error_path.display()
                                )))
                            }
                        };
                        // TODO: Emit a warning that a symlink cycle is detected and it will be skipped
                        // Add it to the link itself
                        sym_path
                    }
                }
            } else {
                return Err(Error::from(io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Walkdir Error: {}", error),
                )));
            }
        }
    };
    Ok(clean(&path))
}

#[cfg(test)]
mod test {
    use data_encoding::HEXLOWER;
    use std::collections::HashMap;

    use super::*;

    fn create_target_description(
        hash_algorithm: crypto::HashAlgorithm,
        hash_value: &[u8],
    ) -> TargetDescription {
        let mut hash = HashMap::new();
        hash.insert(
            hash_algorithm,
            crypto::HashValue::new(HEXLOWER.decode(hash_value).unwrap()),
        );
        hash
    }

    #[test]
    fn test_record_artifacts() {
        let mut expected: BTreeMap<VirtualTargetPath, TargetDescription> = BTreeMap::new();
        expected.insert(
            VirtualTargetPath::new("tests/test_runlib/symbolic_to_license_file".to_string())
                .unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"61ed40687d2656636a04680013dffe41d5c724201edaa84045e0677b8e2064d6",
            ),
        );
        expected.insert(
            VirtualTargetPath::new("tests/test_runlib/.hidden/foo".to_string()).unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"7d865e959b2466918c9863afca942d0fb89d7c9ac0c99bafc3749504ded97730",
            ),
        );
        expected.insert(
            VirtualTargetPath::new("tests/test_runlib/.hidden/.bar".to_string()).unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
            ),
        );
        expected.insert(
            VirtualTargetPath::new("tests/test_runlib/hello./world".to_string()).unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"25623b53e0984428da972f4c635706d32d01ec92dcd2ab39066082e0b9488c9d",
            ),
        );
        expected.insert(
            VirtualTargetPath::new(
                "tests/test_runlib/hello./symbolic_to_nonparent_folder/.bar".to_string(),
            )
            .unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
            ),
        );
        expected.insert(
            VirtualTargetPath::new("tests/test_runlib/symbolic_to_file".to_string()).unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"25623b53e0984428da972f4c635706d32d01ec92dcd2ab39066082e0b9488c9d",
            ),
        );
        expected.insert(
            VirtualTargetPath::new(
                "tests/test_runlib/hello./symbolic_to_nonparent_folder/foo".to_string(),
            )
            .unwrap(),
            create_target_description(
                crypto::HashAlgorithm::Sha256,
                b"7d865e959b2466918c9863afca942d0fb89d7c9ac0c99bafc3749504ded97730",
            ),
        );
        assert_eq!(
            record_artifacts(&["tests/test_runlib"], None).unwrap(),
            expected
        );
        assert_eq!(record_artifacts(&["tests"], None).is_ok(), true);
        assert_eq!(
            record_artifacts(&["file-does-not-exist"], None).is_err(),
            true
        );
    }

    #[test]
    fn test_run_command() {
        let byproducts = run_command(&["sh", "-c", "printf hello"], Some("tests")).unwrap();
        let mut expected = BTreeMap::new();
        expected.insert("stdout".to_string(), "hello".to_string());
        expected.insert("stderr".to_string(), "".to_string());
        expected.insert("return-value".to_string(), "0".to_string());

        assert_eq!(byproducts, expected);

        assert_eq!(
            run_command(&["command-does-not-exist", "true"], None).is_err(),
            true
        );
    }
}
