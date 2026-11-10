// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{Arch, copy_exe, copy_tree, exec, read, root, version};

const BUNDLE_ID: &str = "dev.f-io.livi";
const GST: &str = "gstreamer/macos-arm64";

/// Never calls the legacy packager. Failures retain the entire candidate.
pub fn run_candidate(output: PathBuf) -> Result<(), String> {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err("candidate packaging requires a macOS ARM64 host".into());
    }
    let plan = CandidatePlan::new(&root(), &output)?;
    let script = plan.project.join("utils/package_macos_candidate.py");
    let python = |operation: &str| {
        let mut command = Command::new("python3");
        command.arg("-B").arg(&script).arg(operation)
            .arg("--project").arg(&plan.project)
            .arg("--candidate").arg(&plan.directory);
        command
    };
    let mut identity = String::new();
    plan.visit(&mut |step| {
        println!("[candidate] {step:?}");
        match step {
            CandidateStep::ReserveExclusive(path) => {
                let parent = path.parent().ok_or("missing candidate parent")?;
                // Parent components were inspected by CandidatePlan. Recheck
                // immediately before reservation, including dangling symlinks.
                for ancestor in parent.ancestors().take_while(|p| *p != plan.project) {
                    match fs::symlink_metadata(ancestor) {
                        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() =>
                            return Err("unsafe candidate ancestor".into()),
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.to_string()),
                    }
                }
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                fs::create_dir(path).map_err(|e| format!("exclusive reservation: {e}"))?;
            }
            CandidateStep::SnapshotSource => identity = read(&mut python("snapshot"))?.trim().into(),
            CandidateStep::VerifyCandidateSource => {
                let verified = read(&mut python("verify"))?;
                if verified.trim() != identity {
                    return Err("candidate source identity changed".into());
                }
            }
            CandidateStep::BuildIsolated { .. } => {
                let create = |name: &str| fs::OpenOptions::new().write(true).create_new(true)
                    .open(plan.directory.join(name)).map_err(|e| e.to_string());
                let mut command = crate::build_native::candidate_command(
                    &plan.project, &plan.directory, &identity,
                )?;
                command.stdout(create("build.events.jsonl")?).stderr(create("build.log")?);
                let status = command.status().map_err(|e| format!("candidate build: {e}"))?;
                if !status.success() {
                    return Err(format!("offline ARM64 release build failed: {status}; see {}/build.log and build.events.jsonl; incomplete output retained", plan.directory.display()));
                }
                exec(&mut python("verify"))?;
            }
            CandidateStep::AssembleAndSign { .. } => exec(&mut python("assemble"))?,
            CandidateStep::VerifyStatic { .. } => exec(&mut python("static"))?,
            CandidateStep::StageExclusive { .. } => exec(&mut python("stage"))?,
            CandidateStep::CreateDmgNoOverwrite { .. } => exec(&mut python("dmg"))?,
            CandidateStep::VerifyDmg { .. } => exec(&mut python("verify-dmg"))?,
        }
        Ok(())
    })?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum CandidateStep {
    ReserveExclusive(PathBuf),
    SnapshotSource,
    VerifyCandidateSource,
    BuildIsolated { target: PathBuf },
    AssembleAndSign { app: PathBuf, binaries: PathBuf },
    VerifyStatic { app: PathBuf },
    StageExclusive { staging: PathBuf },
    CreateDmgNoOverwrite { staging: PathBuf, dmg: PathBuf },
    VerifyDmg { dmg: PathBuf },
}

struct CandidatePlan {
    project: PathBuf,
    directory: PathBuf,
    steps: Vec<CandidateStep>,
}

impl CandidatePlan {
    fn new(project: &Path, requested: &Path) -> Result<Self, String> {
        use std::path::Component;
        let project = project.canonicalize().map_err(|e| e.to_string())?;
        if requested
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err("candidate path must not contain traversal components".into());
        }
        let directory = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            project.join(requested)
        };
        let parent = project.join("dist/candidates");
        if directory.parent() != Some(parent.as_path())
            || directory.file_name().is_none()
            || directory
                .extension()
                .is_some_and(|ext| ext == "app" || ext == "dmg")
        {
            return Err(
                "candidate output must be a new directory directly under dist/candidates".into(),
            );
        }
        // Inspect dangling symlinks too; do not resolve a destination through
        // an ancestor that points outside the project.
        for ancestor in directory.ancestors().take_while(|p| *p != project) {
            match fs::symlink_metadata(ancestor) {
                Ok(meta)
                    if ancestor == directory || meta.file_type().is_symlink() || !meta.is_dir() =>
                {
                    return Err("candidate path exists or has an unsafe ancestor".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("candidate path inspection failed: {e}")),
            }
        }
        let app = directory.join("LIVI.app");
        let staging = directory.join("dmg");
        let dmg = directory.join("LIVI-macos-arm64-candidate.dmg");
        let steps = vec![
            CandidateStep::ReserveExclusive(directory.clone()),
            CandidateStep::SnapshotSource,
            CandidateStep::VerifyCandidateSource,
            CandidateStep::BuildIsolated {
                target: directory.join("target"),
            },
            CandidateStep::AssembleAndSign {
                app: app.clone(),
                binaries: directory.join("target/aarch64-apple-darwin/release"),
            },
            CandidateStep::VerifyStatic { app },
            CandidateStep::StageExclusive {
                staging: staging.clone(),
            },
            CandidateStep::CreateDmgNoOverwrite {
                staging,
                dmg: dmg.clone(),
            },
            CandidateStep::VerifyDmg { dmg },
        ];
        Ok(Self {
            project,
            directory,
            steps,
        })
    }

    /// Pure ordering shared by the executor and failure-injection tests.
    fn visit(
        &self,
        visitor: &mut impl FnMut(&CandidateStep) -> Result<(), String>,
    ) -> Result<(), String> {
        for step in &self.steps {
            visitor(step)?;
        }
        Ok(())
    }
}

const BLUETOOTH_USAGE: (&str, &str) = (
    "NSBluetoothAlwaysUsageDescription",
    "LIVI uses Bluetooth to discover and pair your Android phone and start experimental wireless Android Auto.",
);

fn plist(entries: &[(&str, &str)]) -> String {
    let body: Vec<String> = entries
        .iter()
        .map(|(k, v)| {
            let value = match *v {
                "true" => "<true/>".to_string(),
                "false" => "<false/>".to_string(),
                s => format!("<string>{s}</string>"),
            };
            format!("  <key>{k}</key>\n  {value}")
        })
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n{}\n</dict>\n</plist>\n",
        body.join("\n")
    )
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            files_under(&entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

fn relink(file: &Path, rpath: &str) -> Result<(), String> {
    let load = read(Command::new("otool").arg("-l").arg(file))?;
    let lines: Vec<&str> = load.lines().map(str::trim).collect();
    let mut cmd = Command::new("install_name_tool");
    for (i, l) in lines.iter().enumerate() {
        if *l == "cmd LC_RPATH"
            && let Some(path) = lines
                .get(i + 2)
                .and_then(|p| p.strip_prefix("path "))
                .and_then(|p| p.split(" (offset").next())
        {
            cmd.args(["-delete_rpath", path]);
        }
    }
    exec(cmd.args(["-add_rpath", rpath]).arg(file))
}

fn sign(file: &Path, entitlements: Option<&Path>) -> Result<(), String> {
    let mut cmd = Command::new("codesign");
    cmd.args([
        "--force",
        "--sign",
        "-",
        "--timestamp=none",
        "--options",
        "runtime",
    ]);
    if let Some(e) = entitlements {
        cmd.arg("--entitlements").arg(e);
    }
    exec(cmd.arg(file))
}

fn mach_o(file: &Path) -> bool {
    read(Command::new("file").arg("-b").arg(file)).is_ok_and(|t| t.starts_with("Mach-O"))
}

fn has_diskutil_images() -> bool {
    Command::new("diskutil")
        .args(["image", "create", "from", "--help"])
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn run(arch: Arch) -> Result<(), String> {
    if !cfg!(target_os = "macos") {
        return Err("package-mac runs on macOS".into());
    }
    crate::build_native::run(arch)?;

    let root = root();
    let version = version()?;
    let dist = root.join("dist");
    let app = dist.join("LIVI.app");
    let contents = app.join("Contents");
    let res = contents.join("Resources");
    let _ = fs::remove_dir_all(&app);
    fs::create_dir_all(contents.join("MacOS")).map_err(|e| e.to_string())?;

    let built = |name: &str| root.join("out/bin").join(name);
    copy_exe(&built("LIVI"), &contents.join("MacOS/LIVI"))?;
    copy_exe(&built("livi-core"), &res.join("core/livi-core"))?;
    copy_exe(&built("livi-helperd"), &res.join("driver/livi-helperd"))?;
    copy_exe(&built("livi-gst-host"), &res.join("gst-host/livi-gst-host"))?;

    let gst_src = root.join("assets").join(GST);
    let skip = |p: &Path| {
        p.extension().is_some_and(|e| e == "a")
            || p.strip_prefix(&gst_src)
                .is_ok_and(|r| r.starts_with("share"))
    };
    copy_tree(&gst_src, &res.join(GST), &skip)?;
    fs::copy(
        root.join("assets/icons/mac/livi.icns"),
        res.join("livi.icns"),
    )
    .map_err(|e| e.to_string())?;
    for legal in ["LICENSE", "NOTICE"] {
        fs::copy(root.join(legal), res.join(legal)).map_err(|e| e.to_string())?;
    }

    fs::write(
        contents.join("Info.plist"),
        plist(&[
            ("CFBundleIdentifier", BUNDLE_ID),
            ("CFBundleName", "LIVI"),
            ("CFBundleDisplayName", "LIVI"),
            ("CFBundleExecutable", "LIVI"),
            ("CFBundleIconFile", "livi.icns"),
            ("CFBundlePackageType", "APPL"),
            ("CFBundleShortVersionString", &version),
            ("CFBundleVersion", &version),
            ("CFBundleInfoDictionaryVersion", "6.0"),
            ("LSMinimumSystemVersion", "11.0"),
            ("LSApplicationCategoryType", "public.app-category.utilities"),
            ("NSHighResolutionCapable", "true"),
            (
                "NSHumanReadableCopyright",
                "Copyright © 2026 Lasse Heitgres",
            ),
            (
                "NSCameraUsageDescription",
                "LIVI requires camera access for CarPlay.",
            ),
            (
                "NSMicrophoneUsageDescription",
                "LIVI requires microphone access for voice control.",
            ),
            (
                "NSLocalNetworkUsageDescription",
                "LIVI talks to a LIVI Link dongle on the local network.",
            ),
            BLUETOOTH_USAGE,
        ]),
    )
    .map_err(|e| e.to_string())?;

    relink(
        &contents.join("MacOS/LIVI"),
        &format!("@executable_path/../Resources/{GST}/lib"),
    )?;
    relink(
        &res.join("gst-host/livi-gst-host"),
        &format!("@loader_path/../{GST}/lib"),
    )?;

    let inherit = root.join("assets/mac/entitlements.mac.inherit.plist");
    let main = root.join("assets/mac/entitlements.mac.plist");
    let mut nested = Vec::new();
    files_under(&res, &mut nested)?;
    let nested: Vec<PathBuf> = nested.into_iter().filter(|f| mach_o(f)).collect();
    let executable = |f: &PathBuf| {
        let exec = fs::metadata(f).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
        exec && !f.extension().is_some_and(|e| e == "dylib" || e == "so")
    };
    for f in nested.iter().filter(|f| !executable(f)) {
        sign(f, None)?;
    }
    for f in nested.iter().filter(|f| executable(f)) {
        sign(f, Some(&inherit))?;
    }
    sign(&app, Some(&main))?;
    exec(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict", "--verbose=2"])
            .arg(&app),
    )?;

    // A valid signature does not prove plugins load or H.264 decoding works.
    exec(
        Command::new("python3")
            .arg(root.join("utils/verify_gstreamer_macos.py"))
            .arg(&app)
            .args([
                "--arch",
                if arch.name() == "arm64" {
                    "arm64"
                } else {
                    "x86_64"
                },
            ]),
    )?;
    exec(
        Command::new("/bin/bash")
            .arg(root.join("scripts/gstreamer/smoke-test-macos.sh"))
            .arg(res.join(GST)),
    )?;

    let staging = dist.join("dmg");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    exec(
        Command::new("ditto")
            .arg(&app)
            .arg(staging.join("LIVI.app")),
    )?;
    std::os::unix::fs::symlink("/Applications", staging.join("Applications"))
        .map_err(|e| e.to_string())?;
    let dmg = dist.join(format!("LIVI-{version}-macos-{}.dmg", arch.name()));
    let _ = fs::remove_file(&dmg);
    if has_diskutil_images() {
        exec(
            Command::new("diskutil")
                .args([
                    "image",
                    "create",
                    "from",
                    "--format",
                    "UDZO",
                    "--volumeName",
                    "LIVI",
                ])
                .arg(&staging)
                .arg(&dmg),
        )?;
    } else {
        exec(
            Command::new("hdiutil")
                .args([
                    "create",
                    "-volname",
                    "LIVI",
                    "-format",
                    "UDZO",
                    "-srcfolder",
                ])
                .arg(&staging)
                .arg(&dmg),
        )?;
    }
    let _ = fs::remove_dir_all(&staging);
    println!("[package-mac] {}", dmg.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "livi-packager-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            // Never adopt or delete a tree that this test did not create.
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn candidate_plan_is_read_only_and_uses_only_candidate_owned_outputs() {
        let fixture = Fixture::new();
        let requested = Path::new("dist/candidates/unique");
        let plan = CandidatePlan::new(&fixture.0, requested).unwrap();
        let mut calls = 0;
        plan.visit(&mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, 9);
        assert!(matches!(plan.steps[0], CandidateStep::ReserveExclusive(_)));
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
        for step in &plan.steps {
            let paths: Vec<&Path> = match step {
                CandidateStep::SnapshotSource | CandidateStep::VerifyCandidateSource => vec![],
                CandidateStep::ReserveExclusive(path) => vec![path],
                CandidateStep::BuildIsolated { target } => vec![target],
                CandidateStep::AssembleAndSign { app, binaries } => vec![app, binaries],
                CandidateStep::VerifyStatic { app } => vec![app],
                CandidateStep::StageExclusive { staging } => vec![staging],
                CandidateStep::CreateDmgNoOverwrite { staging, dmg } => vec![staging, dmg],
                CandidateStep::VerifyDmg { dmg } => vec![dmg],
            };
            assert!(paths.iter().all(|p| p.starts_with(&plan.directory)));
        }
    }

    #[test]
    fn fake_runner_stops_at_every_failed_step() {
        let fixture = Fixture::new();
        let plan = CandidatePlan::new(&fixture.0, Path::new("dist/candidates/new")).unwrap();
        for fail_at in 0..plan.steps.len() {
            let mut calls = 0;
            let error = plan
                .visit(&mut |step| {
                    assert_eq!(step, &plan.steps[calls]);
                    calls += 1;
                    if calls == fail_at + 1 {
                        Err("simulated failure".into())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_eq!(error, "simulated failure");
            assert_eq!(calls, fail_at + 1);
        }
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn protected_and_traversal_paths_are_rejected() {
        let fixture = Fixture::new();
        for path in [
            "",
            "dist",
            "dist/candidates",
            "dist/LIVI.app",
            "dist/old.dmg",
            "out/bin",
            "/Applications/LIVI.app",
            "dist/candidates/../unsafe",
            "./dist/candidates/new",
            "dist/candidates/nested/new",
            "dist/candidates/LIVI.app",
            "dist/candidates/new.dmg",
        ] {
            assert!(
                CandidatePlan::new(&fixture.0, Path::new(path)).is_err(),
                "{path}"
            );
        }
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    }

    #[test]
    fn collisions_and_dangling_links_preserve_existing_artifacts() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("dist/candidates/occupied")).unwrap();
        fs::create_dir(fixture.0.join("dist/LIVI.app")).unwrap();
        fs::write(fixture.0.join("dist/old.dmg"), b"preserve").unwrap();
        fs::write(fixture.0.join("dist/candidates/file"), b"keep").unwrap();
        std::os::unix::fs::symlink("missing", fixture.0.join("dist/candidates/dangling")).unwrap();
        for path in ["occupied", "file", "dangling"] {
            assert!(
                CandidatePlan::new(&fixture.0, &Path::new("dist/candidates").join(path)).is_err()
            );
        }
        assert_eq!(
            fs::read(fixture.0.join("dist/old.dmg")).unwrap(),
            b"preserve"
        );
        assert_eq!(
            fs::read(fixture.0.join("dist/candidates/file")).unwrap(),
            b"keep"
        );
        assert!(fixture.0.join("dist/LIVI.app").is_dir());
    }

    #[test]
    fn symlinked_ancestors_cannot_redirect_candidate_output() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("dist")).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), fixture.0.join("dist/candidates"))
            .unwrap();
        assert!(CandidatePlan::new(&fixture.0, Path::new("dist/candidates/new")).is_err());
    }

    #[test]
    fn bluetooth_usage_description_is_exactly_the_approved_candidate_text() {
        assert_eq!(BLUETOOTH_USAGE.0, "NSBluetoothAlwaysUsageDescription");
        assert_eq!(
            BLUETOOTH_USAGE.1,
            "LIVI uses Bluetooth to discover and pair your Android phone and start experimental wireless Android Auto."
        );
        let xml = plist(&[BLUETOOTH_USAGE]);
        assert_eq!(xml.matches("<key>").count(), 1);
        assert!(xml.contains(&format!("<string>{}</string>", BLUETOOTH_USAGE.1)));
    }
}
