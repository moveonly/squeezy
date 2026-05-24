use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use squeezy_core::{LanguageKind, Result, SqueezyError};
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

pub fn which(program: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(program).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    Some(PathBuf::from(path.trim()))
}

pub(crate) fn time_cargo_check(fixture: &Path) -> Result<u128> {
    let manifest = fixture.join("Cargo.toml");
    let mut command = Command::new("cargo");
    if manifest.exists() {
        command
            .arg("check")
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--quiet");
    } else {
        command.arg("check").arg("--quiet").current_dir(fixture);
    }

    let started = Instant::now();
    let status = command.status()?;
    let elapsed = started.elapsed().as_millis();
    if status.success() {
        Ok(elapsed)
    } else {
        Err(SqueezyError::Graph(format!(
            "compiler validation failed with {status}"
        )))
    }
}

pub(crate) fn time_cargo_check_optional(repo: &Path) -> (Option<u128>, String) {
    match time_cargo_check(repo) {
        Ok(ms) => (Some(ms), "cargo check succeeded".to_string()),
        Err(err) => (None, format!("cargo check failed: {err}")),
    }
}

pub(crate) fn time_dotnet_build(fixture: &Path) -> Result<u128> {
    let build_target = find_dotnet_build_target(fixture);
    let started = Instant::now();
    let mut command = Command::new("dotnet");
    command.arg("build");
    if let Some(target) = build_target {
        command.arg(target);
    }
    let status = command
        .arg("--nologo")
        .arg("-v")
        .arg("minimal")
        .current_dir(fixture)
        .status()?;
    let elapsed = started.elapsed().as_millis();
    if status.success() {
        Ok(elapsed)
    } else {
        Err(SqueezyError::Graph(format!(
            "dotnet build validation failed with {status}"
        )))
    }
}

pub(crate) fn time_dotnet_build_optional(repo: &Path) -> (Option<u128>, String) {
    match time_dotnet_build(repo) {
        Ok(ms) => (Some(ms), "dotnet build succeeded".to_string()),
        Err(err) => (None, format!("dotnet build failed: {err}")),
    }
}

pub(crate) fn find_dotnet_build_target(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    collect_dotnet_build_targets(root, root, 0, &mut candidates);
    // Prefer the shallowest candidate (root-level solution beats nested project),
    // then by extension priority (slnx > sln > csproj), then lexicographic.
    candidates.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.2.cmp(&right.2))
    });
    candidates.into_iter().map(|(_, _, path)| path).next()
}

pub(crate) fn collect_dotnet_build_targets(
    root: &Path,
    dir: &Path,
    depth: usize,
    out: &mut Vec<(usize, usize, PathBuf)>,
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "bin" | "obj" | "packages" | "target") {
                continue;
            }
            collect_dotnet_build_targets(root, &path, depth + 1, out);
            continue;
        }
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        let priority = match extension {
            "slnx" => 0,
            "sln" => 1,
            "csproj" => 2,
            _ => continue,
        };
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        out.push((priority, depth, relative));
    }
}

pub(crate) fn time_clang_syntax(
    fixture: &Path,
    compiler: &str,
    language: LanguageKind,
) -> Result<u128> {
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(fixture)?;
    let files = snapshot
        .files
        .into_iter()
        .filter(|record| record.language == language)
        .filter(|record| {
            !matches!(
                record
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str()),
                Some("h" | "hh" | "hpp" | "hxx")
            )
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Ok(0);
    }

    let started = Instant::now();
    let worker_count = std::thread::available_parallelism()
        .map(|threads| threads.get())
        .unwrap_or(1)
        .min(files.len())
        .max(1);
    let chunk_size = files.len().div_ceil(worker_count);
    let compiler = compiler.to_string();
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for chunk in files.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            let compiler = compiler.clone();
            handles.push(scope.spawn(move || -> Result<()> {
                for file in chunk {
                    let status = Command::new(&compiler)
                        .arg("-fsyntax-only")
                        .arg(&file.path)
                        .status()?;
                    if !status.success() {
                        return Err(SqueezyError::Graph(format!(
                            "{compiler} validation failed for {} with {status}",
                            file.relative_path
                        )));
                    }
                }
                Ok(())
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    return Err(SqueezyError::Graph(
                        "clang syntax worker panicked".to_string(),
                    ));
                }
            }
        }
        Ok(())
    })?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn time_clang_syntax_optional(
    repo: &Path,
    compiler: &str,
    language: LanguageKind,
) -> (Option<u128>, String) {
    match time_clang_syntax(repo, compiler, language) {
        Ok(ms) => (Some(ms), format!("{compiler} -fsyntax-only succeeded")),
        Err(err) => (None, format!("{compiler} -fsyntax-only failed: {err}")),
    }
}

#[cfg(test)]
#[path = "toolchain_tests.rs"]
mod tests;
