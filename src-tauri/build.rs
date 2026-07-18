use std::{env, fs, path::PathBuf};

use tauri_build::{Attributes, WindowsAttributes};

fn main() {
    println!("cargo:rerun-if-changed=assets/skill-groups.json");
    println!("cargo:rerun-if-changed=assets/master-traits.json");
    println!("cargo:rerun-if-changed=assets/sigil-traits.json");
    println!("cargo:rerun-if-changed=assets/weapon-data.json");
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
    let skill_groups = fs::read_to_string("assets/skill-groups.json")
        .expect("Could not read bundled skill grouping map");
    for character in ["Pl2400", "Pl2500", "Pl2600", "Pl2700", "Pl2800", "Pl2900"] {
        assert!(
            skill_groups.contains(&format!("\"{character}\"")),
            "Bundled skill grouping map is missing expansion character {character}"
        );
    }
    let master_traits = fs::read_to_string("assets/master-traits.json")
        .expect("Could not read bundled Master Traits resource");
    for character in [
        "Pl0000", "Pl2400", "Pl2500", "Pl2600", "Pl2700", "Pl2800", "Pl2900",
    ] {
        assert!(
            master_traits.contains(&format!("\"{character}\"")),
            "Bundled Master Traits resource is missing character {character}"
        );
    }
    let sigil_traits = fs::read_to_string("assets/sigil-traits.json")
        .expect("Could not read bundled game 2.0 sigil trait resource");
    for sigil_id in ["3760801040", "1862062726", "1225749252", "3634652401"] {
        assert!(
            sigil_traits.contains(&format!("\"sigilId\":{sigil_id}")),
            "Bundled sigil traits are missing game 2.0 sigil {sigil_id}"
        );
    }
    let weapon_data = fs::read_to_string("assets/weapon-data.json")
        .expect("Could not read bundled game 2.0 weapon progression resource");
    assert!(
        weapon_data.contains("162540"),
        "Bundled weapon progression resource is missing level 150"
    );

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("../target"));
    println!(
        "cargo:rerun-if-changed={}",
        target_dir.join("release/hook.dll").display()
    );
    fs::copy(target_dir.join("release/hook.dll"), "hook.dll")
        .expect("Could not copy the freshly built hook.dll into the Tauri resources");

    if cfg!(debug_assertions) {
        tauri_build::build();
    } else {
        let windows = WindowsAttributes::new().app_manifest(include_str!("manifest.xml"));

        tauri_build::try_build(Attributes::new().windows_attributes(windows))
            .expect("Could not build Tauri app.")
    }
}
