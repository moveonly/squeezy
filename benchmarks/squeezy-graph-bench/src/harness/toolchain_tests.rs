use std::{fs, path::PathBuf};

use crate::util::temp_dir;

use super::find_dotnet_build_target;

#[test]
fn find_dotnet_build_target_prefers_root_solution_over_nested_slnx() {
    let root = temp_dir("dotnet-build-target-priority").unwrap();
    fs::create_dir_all(root.join("nested/very/deep")).unwrap();
    fs::write(root.join("App.sln"), "").unwrap();
    fs::write(root.join("nested/very/deep/Inner.slnx"), "").unwrap();
    fs::write(root.join("nested/very/deep/Inner.csproj"), "").unwrap();

    assert_eq!(
        find_dotnet_build_target(&root),
        Some(PathBuf::from("App.sln"))
    );
}

#[test]
fn find_dotnet_build_target_prefers_slnx_over_sln_at_same_depth() {
    let root = temp_dir("dotnet-build-target-extension-priority").unwrap();
    fs::write(root.join("App.sln"), "").unwrap();
    fs::write(root.join("App.slnx"), "").unwrap();
    fs::write(root.join("App.csproj"), "").unwrap();

    assert_eq!(
        find_dotnet_build_target(&root),
        Some(PathBuf::from("App.slnx"))
    );
}
