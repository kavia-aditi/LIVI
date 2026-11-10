// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::path::PathBuf;
use std::process::Command;

use crate::util::{Arch, build_date, build_run, build_sha, copy_exe, exec, read, root, version};

const MAC_GST_PKGCONFIG: &str =
    "/Library/Frameworks/GStreamer.framework/Versions/1.0/lib/pkgconfig";

/// The executor verifies a fresh source manifest and exclusively reserves the
/// candidate directory before executing this command. No historical approval
/// is asserted, and build_sha() (with its Git fallback) is never called.
pub(crate) fn candidate_command(
    project: &std::path::Path,
    candidate: &std::path::Path,
    candidate_build_id: &str,
) -> Result<Command, String> {
    if candidate_build_id.is_empty()
        || candidate_build_id.len() > 128
        || !candidate_build_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err("candidate build requires a verified source identifier".into());
    }
    let mut command = Command::new("cargo");
    // Do not inherit development controls, compiler injection, or credentials.
    let allowed = [
        "PATH", "HOME", "USER", "LOGNAME", "TMPDIR", "SHELL",
        "CARGO_HOME", "RUSTUP_HOME", "DEVELOPER_DIR", "SDKROOT",
        "MACOSX_DEPLOYMENT_TARGET", "PKG_CONFIG_PATH",
    ];
    let environment: Vec<_> = allowed.iter().filter_map(|key| {
        std::env::var_os(key).map(|value| (*key, value))
    }).collect();
    command
        .env_clear()
        .envs(environment)
        .current_dir(project.join("native"))
        .args([
            "build",
            "--release",
            "--locked",
            "--offline",
            "--target",
            "aarch64-apple-darwin",
            "--message-format=json-render-diagnostics",
        ])
        .arg("--manifest-path")
        .arg(project.join("native/Cargo.toml"))
        .arg("--target-dir")
        .arg(candidate.join("target"))
        .env("CARGO_TARGET_DIR", candidate.join("target"))
        .env("CARGO_INCREMENTAL", "0")
        .env("LIVI_VERSION", version()?)
        .env("LIVI_BUILD_DATE", build_date())
        .env("LIVI_BUILD_SHA", candidate_build_id)
        .env("LIVI_BUILD_RUN", "")
        .env("PKG_CONFIG_ALLOW_CROSS", "1");
    // Retain the existing SDK discovery rule without consulting user secrets.
    if cfg!(target_os = "macos") && std::path::Path::new(MAC_GST_PKGCONFIG).is_dir() {
        command.env("PKG_CONFIG_PATH", MAC_GST_PKGCONFIG);
    }
    for package in ["livi-gst-host", "livi-helperd", "livi-core", "livi-ui"] {
        command.args(["-p", package]);
    }
    Ok(command)
}

fn cargo(manifest: &PathBuf, sub: &[&str]) -> Result<Command, String> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.env("LIVI_VERSION", version()?)
        .env("LIVI_BUILD_DATE", build_date())
        .env("LIVI_BUILD_SHA", build_sha())
        .env("LIVI_BUILD_RUN", build_run())
        .env("PKG_CONFIG_ALLOW_CROSS", "1");
    if cfg!(target_os = "macos") && std::path::Path::new(MAC_GST_PKGCONFIG).is_dir() {
        let prev = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
        let path = if prev.is_empty() {
            MAC_GST_PKGCONFIG.to_string()
        } else {
            format!("{MAC_GST_PKGCONFIG}:{prev}")
        };
        cmd.env("PKG_CONFIG_PATH", path);
    }
    cmd.args(sub).arg("--manifest-path").arg(manifest);
    Ok(cmd)
}

fn target_dir(manifest: &PathBuf) -> Result<PathBuf, String> {
    let meta = read(&mut cargo(
        manifest,
        &["metadata", "--format-version", "1", "--no-deps"],
    )?)?;
    let v: serde_json::Value = serde_json::from_str(&meta).map_err(|e| e.to_string())?;
    v["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata gave no target_directory".into())
}

pub fn run(arch: Arch) -> Result<(), String> {
    let root = root();
    let manifest = root.join("native/Cargo.toml");
    let cross = arch != Arch::host();

    let mut pkgs = vec!["livi-gst-host", "livi-helperd", "livi-core", "livi-ui"];
    if cfg!(target_os = "linux") {
        pkgs.push("livi-compositor");
    }

    let mut build = cargo(&manifest, &["build", "--release"])?;
    for p in &pkgs {
        build.args(["-p", p]);
    }
    if std::env::var_os("CI").is_some() {
        build.arg("--locked");
    }
    if cross {
        exec(Command::new("rustup").args(["target", "add", arch.triple()]))?;
        build.args(["--target", arch.triple()]);
    }
    exec(&mut build)?;

    let mut out = target_dir(&manifest)?;
    if cross {
        out.push(arch.triple());
    }
    out.push("release");

    let place = |name: &str, dest: PathBuf| -> Result<(), String> {
        copy_exe(&out.join(name), &dest)?;
        println!(
            "[build-native] {} <- {}",
            dest.display(),
            out.join(name).display()
        );
        Ok(())
    };
    let bin = root.join("out/bin");
    place("livi-gst-host", bin.join("livi-gst-host"))?;
    place("livi-helperd", bin.join("livi-helperd"))?;
    place("livi-core", bin.join("livi-core"))?;
    place("LIVI", bin.join("LIVI"))?;
    if cfg!(target_os = "linux") {
        place("livi-compositor", bin.join("livi-compositor"))?;
    }
    Ok(())
}

#[cfg(test)]
mod candidate_tests {
    use super::*;

    #[test]
    fn candidate_build_is_only_a_command_with_private_target_and_all_production_packages() {
        let base = std::path::Path::new("/project/dist/candidates/new");
        let command =
            candidate_command(std::path::Path::new("/project"), base, "candidate-record-1").unwrap();
        assert_eq!(command.get_program(), "cargo");
        let args: Vec<_> = command
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "build",
                "--release",
                "--locked",
                "--offline",
                "--target",
                "aarch64-apple-darwin",
                "--message-format=json-render-diagnostics",
                "--manifest-path",
                "/project/native/Cargo.toml",
                "--target-dir",
                "/project/dist/candidates/new/target",
                "-p",
                "livi-gst-host",
                "-p",
                "livi-helperd",
                "-p",
                "livi-core",
                "-p",
                "livi-ui",
            ]
        );
        for (key, expected) in [
            ("CARGO_TARGET_DIR", "/project/dist/candidates/new/target"),
            ("CARGO_INCREMENTAL", "0"),
            ("LIVI_BUILD_SHA", "candidate-record-1"),
            ("LIVI_BUILD_RUN", ""),
            ("PKG_CONFIG_ALLOW_CROSS", "1"),
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == key
                        && value == Some(std::ffi::OsStr::new(expected)))
            );
        }
        let expected_version = version().unwrap();
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "LIVI_VERSION"
                    && value == Some(std::ffi::OsStr::new(&expected_version)))
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "LIVI_BUILD_DATE"
                    && value.is_some_and(|date| date.to_string_lossy().ends_with(" UTC")))
        );
        for invalid in ["", "../unverified", "unverified id", &"x".repeat(129)] {
            assert!(candidate_command(std::path::Path::new("/project"), base, invalid).is_err());
        }
    }
}
