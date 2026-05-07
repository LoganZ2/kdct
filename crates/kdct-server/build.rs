use std::path::PathBuf;
use std::process::Command;

fn main() {
    let panel_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/kdct-panel");

    let panel_dir = match panel_dir.canonicalize() {
        Ok(d) => d,
        Err(_) => {
            println!("cargo:warning=Panel directory not found, skipping frontend build");
            return;
        }
    };

    let node_modules = panel_dir.join("node_modules");
    if !node_modules.exists() {
        println!("cargo:warning=Installing panel dependencies...");
        let status = Command::new("npm")
            .arg("install")
            .current_dir(&panel_dir)
            .status();

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                println!("cargo:warning=npm install failed with status: {}", s);
                return;
            }
            Err(e) => {
                println!("cargo:warning=npm not available, skipping frontend build: {}", e);
                return;
            }
        }
    }

    println!("cargo:warning=Building panel...");
    let status = Command::new("npm")
        .arg("run")
        .arg("build")
        .current_dir(&panel_dir)
        .status();

    match status {
        Ok(s) if s.success() => {
            let build_dir = panel_dir.join("build");
            if build_dir.join("index.html").exists() {
                println!("cargo:warning=Panel built successfully at {}", build_dir.display());
            } else {
                println!("cargo:warning=Panel build completed but index.html not found");
            }
        }
        Ok(s) => {
            println!("cargo:warning=Panel build failed with status: {}", s);
        }
        Err(e) => {
            println!("cargo:warning=npm run build failed: {}", e);
        }
    }
}
