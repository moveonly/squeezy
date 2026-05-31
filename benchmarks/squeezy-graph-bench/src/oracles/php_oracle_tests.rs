use std::fs;

use crate::util::temp_dir;

use super::{locate_php_oracle_helper, run_php_oracle};

fn php_is_available() -> bool {
    std::process::Command::new("php")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn helper_with_vendor() -> Option<std::path::PathBuf> {
    let helper = locate_php_oracle_helper()?;
    let vendor = helper.parent()?.join("vendor");
    if !vendor.exists() {
        eprintln!(
            "skipping: composer install not run in {}",
            helper.parent().map(|p| p.display().to_string()).unwrap_or_default(),
        );
        return None;
    }
    Some(helper)
}

#[test]
fn php_oracle_walks_namespace_class_interface_trait_enum_and_returns_rows() {
    if !php_is_available() {
        eprintln!("skipping: php not installed");
        return;
    }
    let Some(helper) = helper_with_vendor() else {
        return;
    };
    let root = temp_dir("php-oracle-shapes").unwrap();
    fs::write(
        root.join("Service.php"),
        r#"<?php
namespace Foo\Bar;

interface IRunner {}
trait Loggable {}
class Service extends Base implements IRunner { use Loggable; }
enum Status: string { case Ok = 'ok'; }
"#,
    )
    .unwrap();
    let scan = run_php_oracle(&helper, &root).unwrap();
    assert!(scan
        .symbols
        .counts
        .keys()
        .any(|key| key.kind == "Class" && key.name == "Service"));
    assert!(scan
        .symbols
        .counts
        .keys()
        .any(|key| key.kind == "Interface" && key.name == "IRunner"));
    assert!(scan
        .symbols
        .counts
        .keys()
        .any(|key| key.kind == "Trait" && key.name == "Loggable"));
    assert!(scan
        .symbols
        .counts
        .keys()
        .any(|key| key.kind == "Enum" && key.name == "Status"));
    assert!(scan
        .edges
        .counts
        .keys()
        .any(|key| key.kind == "Extends" && key.name == "Service->Base"));
    assert!(scan
        .edges
        .counts
        .keys()
        .any(|key| key.kind == "Implements" && key.name == "Service->IRunner"));
    assert!(scan
        .edges
        .counts
        .keys()
        .any(|key| key.kind == "UsesTrait" && key.name == "Service->Loggable"));
}

#[test]
fn php_oracle_records_parse_errors_as_unparseable() {
    if !php_is_available() {
        eprintln!("skipping: php not installed");
        return;
    }
    let Some(helper) = helper_with_vendor() else {
        return;
    };
    let root = temp_dir("php-oracle-broken").unwrap();
    fs::write(root.join("broken.php"), "<?php\nclass {\n").unwrap();
    let scan = run_php_oracle(&helper, &root).unwrap();
    assert!(
        scan.unparseable_files
            .iter()
            .any(|file| file == "broken.php"),
        "broken file should be reported as unparseable",
    );
}
