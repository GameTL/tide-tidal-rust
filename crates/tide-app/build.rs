use std::{collections::HashMap, env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=TIDAL_FONT_DIR");
    let local_fonts = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("fonts");
    println!("cargo:rerun-if-changed={}", local_fonts.display());
    let font_dir = env::var_os("TIDAL_FONT_DIR")
        .map(PathBuf::from)
        .or_else(|| local_fonts.is_dir().then_some(local_fonts));
    let typography = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("typography.slint");
    let mut source = String::new();
    let family = if let Some(dir) = font_dir {
        let dir = fs::canonicalize(dir)?;
        println!("cargo:rerun-if-changed={}", dir.display());
        for face in ["Light", "Regular", "Medium", "DemiBold", "Bold"] {
            let path = ["otf", "ttf"]
                .iter()
                .map(|ext| dir.join(format!("Nationale-{face}.{ext}")))
                .find(|path| path.is_file())
                .ok_or_else(|| format!("Missing Nationale {face} font in {}", dir.display()))?;
            source.push_str(&format!("import {:?};\n", path));
        }
        "Nationale"
    } else {
        "Helvetica Neue"
    };
    source.push_str(&format!(
        "export global Typography {{ out property <string> family: \"{family}\"; }}\n"
    ));
    fs::write(&typography, source)?;
    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(HashMap::from([("typography".into(), typography)]));
    slint_build::compile_with_config("ui/app.slint", config)?;
    Ok(())
}
