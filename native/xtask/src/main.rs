// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

mod build_native;
mod package_linux;
mod package_mac;
mod util;

use std::process::ExitCode;

use util::Arch;

const USAGE: &str = "usage: cargo xtask <build-native|package-mac|package-linux> [--arch x64|arm64]\n       cargo xtask package-mac-candidate --arch arm64 --output-dir <new-path>";

fn candidate_args(args: &[String]) -> Result<std::path::PathBuf, String> {
    let mut arch = None;
    let mut output = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--arch" if arch.is_none() => arch = Some(it.next().ok_or(USAGE)?),
            "--output-dir" if output.is_none() => {
                output = Some(std::path::PathBuf::from(it.next().ok_or(USAGE)?));
            }
            _ => return Err(USAGE.into()),
        }
    }
    if arch.map(String::as_str) != Some("arm64") {
        return Err("candidate packaging requires explicit --arch arm64".into());
    }
    output
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| USAGE.into())
}

fn arch(args: &[String]) -> Result<Arch, String> {
    let mut it = args.iter();
    let mut arch = Arch::host();
    while let Some(a) = it.next() {
        let value = match a.strip_prefix("--arch=") {
            Some(v) => v.to_string(),
            None if a == "--arch" => it.next().cloned().ok_or(USAGE)?,
            None => return Err(format!("unknown argument {a}\n{USAGE}")),
        };
        arch = Arch::parse(&value).ok_or(USAGE)?;
    }
    Ok(arch)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build-native") => arch(&args[1..]).and_then(build_native::run),
        Some("package-mac") => arch(&args[1..]).and_then(package_mac::run),
        Some("package-linux") => arch(&args[1..]).and_then(package_linux::run),
        Some("package-mac-candidate") => {
            candidate_args(&args[1..]).and_then(package_mac::run_candidate)
        }
        _ => Err(USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[xtask] {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod candidate_cli_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }

    #[test]
    fn candidate_requires_explicit_arm64_and_output() {
        for valid in [
            vec!["--arch", "arm64", "--output-dir", "dist/candidates/new"],
            vec!["--output-dir", "dist/candidates/new", "--arch", "arm64"],
        ] {
            assert_eq!(
                candidate_args(&args(&valid)).unwrap(),
                std::path::PathBuf::from("dist/candidates/new")
            );
        }
        for invalid in [
            vec![],
            vec!["--arch", "arm64"],
            vec!["--arch"],
            vec!["--output-dir"],
            vec!["--output-dir", "dist/candidates/new"],
            vec!["--arch", "x64", "--output-dir", "dist/candidates/new"],
            vec!["--arch", "arm64", "--output-dir", ""],
            vec!["--arch", "arm64", "--output-dir", "x", "--output-dir", "y"],
            vec!["--arch", "arm64", "--arch", "arm64", "--output-dir", "x"],
            vec!["--arch", "arm64", "--output-dir", "x", "--execute"],
            vec!["--arch=arm64", "--output-dir", "x"],
        ] {
            assert!(candidate_args(&args(&invalid)).is_err());
        }
    }
}
