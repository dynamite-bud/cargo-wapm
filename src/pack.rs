use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Error};
use cargo_metadata::{Metadata, Package, Target};
use clap::Parser;
use serde::Deserialize;
use wapm_toml::{Manifest, Module};

use crate::metadata::MetadataTable;

/// Compile a Rust crate to a WAPM package.
///
/// # Examples
///
/// ```rust,no_run
/// use clap::Parser;
/// use cargo_wapm::Pack;
///
/// # fn main() -> Result<(), anyhow::Error> {
/// let pack = Pack::parse();
/// let meta = pack.metadata()?;
///
/// for pkg in pack.resolve_packages(&meta) {
///     let dest = pack.generate_wapm_package(pkg, meta.target_directory.as_ref())?;
///     println!("Wrote the WAPM package for {} to {}", pkg.name, dest.display());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Parser)]
pub struct Pack {
    #[command(flatten)]
    manifest: clap_cargo::Manifest,
    #[command(flatten)]
    workspace: clap_cargo::Workspace,
    #[command(flatten)]
    features: clap_cargo::Features,
    /// Compile in debug mode.
    #[clap(long)]
    pub debug: bool,
    /// Where to save the compiled WAPM packages (defaults to "$target_dir/wapm/")
    #[clap(long, env)]
    out_dir: Option<PathBuf>,
}

impl Pack {
    /// Use the `cargo metadata` subcommand to learn about the workspace.
    #[tracing::instrument(skip_all)]
    pub fn metadata(&self) -> Result<Metadata, Error> {
        let mut cmd = self.manifest.metadata();
        self.features.forward_metadata(&mut cmd);
        let meta = cmd.exec()?;

        Ok(meta)
    }

    /// Figure out which packages the user wants to pack.
    #[tracing::instrument(skip_all)]
    pub fn resolve_packages<'meta>(&self, metadata: &'meta Metadata) -> Vec<&'meta Package> {
        let (packages, _excluded) = self.workspace.partition_packages(metadata);
        packages
    }

    /// Compile a crate to a WAPM package and save it on disk.
    #[tracing::instrument(skip_all)]
    pub fn generate_wapm_package(
        &self,
        pkg: &Package,
        target_dir: &Path,
    ) -> Result<PathBuf, Error> {
        let dest = self
            .out_dir
            .as_deref()
            .unwrap_or(target_dir)
            .join(&pkg.name);
        tracing::debug!(dest=%dest.display(), "Generating the WAPM package");

        if dest.exists() {
            tracing::debug!(
                dir=%dest.display(),
                "Removing previous generated package",
            );
            std::fs::remove_dir_all(&dest)
                .with_context(|| format!("Unable to remove \"{}\"", dest.display()))?;
        }

        let target = determine_target(pkg)?;
        let manifest: Manifest = generate_manifest(pkg, target)?;
        let modules = manifest
            .module
            .as_deref()
            .expect("We will always compile one module");
        let wasm_path = self.compile_to_wasm(pkg, target_dir, &modules[0], target)?;
        pack(&dest, &manifest, &wasm_path, pkg)?;

        Ok(dest)
    }

    fn compile_to_wasm(
        &self,
        pkg: &Package,
        target_dir: &Path,
        module: &Module,
        target: &Target,
    ) -> Result<PathBuf, Error> {
        let mut cmd = Command::new(cargo_bin());
        let target_triple = match module.abi {
            wapm_toml::Abi::Emscripten => "wasm32-unknown-emscripten",
            wapm_toml::Abi::Wasi => "wasm32-wasi",
            wapm_toml::Abi::None | wapm_toml::Abi::WASM4 => "wasm32-unknown-unknown",
        };

        cmd.arg("build")
            .arg("--quiet")
            .args(["--manifest-path", pkg.manifest_path.as_str()])
            .args(["--target", target_triple]);

        let clap_cargo::Features {
            all_features,
            no_default_features,
            ref features,
            ..
        } = self.features;
        if all_features {
            cmd.arg("--all-features");
        }
        if no_default_features {
            cmd.arg("--no-default-features");
        }
        if !features.is_empty() {
            cmd.arg(format!("--features={}", self.features.features.join(",")));
        }

        if !self.debug {
            cmd.arg("--release");
        }

        tracing::debug!(?cmd, "Compiling the WebAssembly package");

        let status = cmd.status().with_context(|| {
            format!(
                "Unable to start \"{}\". Is it installed?",
                cmd.get_program().to_string_lossy()
            )
        })?;

        if !status.success() {
            match status.code() {
                Some(code) => anyhow::bail!("Cargo exited unsuccessfully with exit code {}", code),
                None => anyhow::bail!("Cargo exited unsuccessfully"),
            }
        }

        let binary = target_dir
            .join(target_triple)
            .join(if self.debug { "debug" } else { "release" })
            .join(wasm_binary_name(target))
            .with_extension("wasm");

        anyhow::ensure!(
            binary.exists(),
            "Expected \"{}\" to exist",
            binary.display()
        );

        Ok(binary)
    }
}

fn determine_target(pkg: &Package) -> Result<&Target, Error> {
    let candidates: Vec<_> = pkg
        .targets
        .iter()
        .filter(|t| is_webassembly_library(t) || t.is_bin())
        .collect();
    match *candidates.as_slice() {
        [single_target] => Ok(single_target),
        [] => anyhow::bail!(
            "The {} package doesn't contain any binaries or \"cdylib\" libraries",
            pkg.name
        ),
        [..] => anyhow::bail!(
            "Unable to decide what to publish. Expected one executable or \"cdylib\" library, but found {}",
            candidates.iter()
                .map(|t| format!("{} ({:?})", t.name, t.kind))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn is_webassembly_library(target: &Target) -> bool {
    target.kind.iter().any(|k| k == "cdylib")
}

#[tracing::instrument(skip_all)]
fn pack(dest: &Path, manifest: &Manifest, wasm_path: &Path, pkg: &Package) -> Result<(), Error> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Unable to create the \"{}\" directory", dest.display()))?;

    let manifest_path = dest.join("wapm.toml");
    let toml = toml::to_string(manifest).context("Unable to serialize the wapm.toml")?;
    tracing::debug!(
        path = %manifest_path.display(),
        bytes = toml.len(),
        "Writing manifest",
    );
    std::fs::write(&manifest_path, toml.as_bytes())
        .with_context(|| format!("Unable to write to \"{}\"", manifest_path.display()))?;

    let new_wasm_path = dest.join(wasm_path.file_name().unwrap());
    copy(wasm_path, new_wasm_path)?;

    let base_dir = pkg.manifest_path.parent().unwrap();

    if let Some(license_file) = pkg.license_file.as_ref() {
        let license_file = base_dir.join(license_file);
        let dest = dest.join(Path::new(&license_file).file_name().unwrap());
        copy(license_file, dest)?;
    }

    if let Some(readme) = pkg.readme.as_ref() {
        let readme = base_dir.join(readme);
        let dest = dest.join(readme.file_name().unwrap());
        copy(readme, dest)?;
    }

    for module in manifest.module.as_deref().unwrap_or_default() {
        if let Some(bindings) = &module.bindings {
            let base_dir = base_dir.as_std_path();
            for path in bindings.referenced_files(base_dir)? {
                // Note: we want to maintain the same location relative to the
                // Cargo.toml file
                let relative_path = path.strip_prefix(base_dir).with_context(|| {
                    format!(
                        "\"{}\" should be inside \"{}\"",
                        path.display(),
                        base_dir.display(),
                    )
                })?;
                let dest = dest.join(relative_path);
                copy(path, dest)?;
            }
        }
    }

    Ok(())
}

fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<(), Error> {
    let from = from.as_ref();
    let to = to.as_ref();

    tracing::debug!(
        from = %from.display(),
        to = %to.display(),
        "Copying file",
    );
    std::fs::copy(from, to).with_context(|| {
        format!(
            "Unable to copy \"{}\" to \"{}\"",
            from.display(),
            to.display()
        )
    })?;

    Ok(())
}

fn wasm_binary_name(target: &Target) -> String {
    // Because reasons, `rustc` will leave dashes in a binary's name but
    // libraries are converted to underscores.
    if target.is_bin() {
        target.name.clone()
    } else {
        target.name.replace('-', "_")
    }
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"))
}

#[tracing::instrument(skip_all)]
fn generate_manifest(pkg: &Package, target: &Target) -> Result<Manifest, Error> {
    tracing::trace!(?target, "Generating manifest");

    let MetadataTable {
        wapm:
            crate::metadata::Wapm {
                wasmer_extra_flags,
                fs,
                abi,
                namespace,
                package,
                bindings,
            },
    } = MetadataTable::deserialize(&pkg.metadata)
        .context("Unable to deserialize the [metadata] table")?;

    match pkg.description.as_deref() {
        Some("") => anyhow::bail!("The \"description\" field in your Cargo.toml is empty"),
        Some(_) => {}
        None => anyhow::bail!("The \"description\" field in your Cargo.toml wasn't set"),
    }

    let package_name = format!("{}/{}", namespace, package.as_deref().unwrap_or(&pkg.name));

    let module = Module {
        name: target.name.clone(),
        source: PathBuf::from(wasm_binary_name(target)).with_extension("wasm"),
        abi,
        bindings,
        interfaces: None,
        kind: None,
    };

    let command = if target.is_bin() {
        let cmd = wapm_toml::Command::V1(wapm_toml::CommandV1 {
            module: target.name.clone(),
            name: target.name.clone(),
            package: Some(package_name.clone()),
            main_args: None,
        });
        Some(vec![cmd])
    } else {
        None
    };

    Ok(Manifest {
        package: wapm_toml::Package {
            name: package_name,
            version: pkg.version.clone(),
            description: pkg.description.clone().unwrap_or_default(),
            license: pkg.license.clone(),
            license_file: pkg.license_file().map(|p| p.into_std_path_buf()),
            readme: pkg.readme().map(|p| p.into_std_path_buf()),
            repository: pkg.repository.clone(),
            homepage: pkg.homepage.clone(),
            wasmer_extra_flags,
            disable_command_rename: false,
            rename_commands_to_raw_command_name: false,
        },
        module: Some(vec![module]),
        command,
        fs,
        dependencies: None,
        base_directory_path: PathBuf::new(),
    })
}
