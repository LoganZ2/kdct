use anyhow::{bail, Context, Result};
use tracing::info;

use crate::db::Database;

pub async fn load_image(db: &Database, source: &str, custom_name: Option<&str>) -> Result<String> {
    let source_type = if source.starts_with("http") || source.ends_with(".git") {
        "git"
    } else {
        "docker_hub"
    };

    let name = if let Some(n) = custom_name {
        n.to_string()
    } else {
        match source_type {
            "docker_hub" => source.to_string(),
            "git" => {
                source
                    .rsplit('/')
                    .next()
                    .unwrap_or(source)
                    .trim_end_matches(".git")
                    .to_string()
            }
            _ => source.to_string(),
        }
    };

    info!("Loading image: {} (type: {})", name, source_type);

    if source_type == "git" {
        let output = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", source])
            .arg(format!("/tmp/kdct-build-{}", name))
            .output()
            .await
            .context("Failed to clone git repository")?;

        if !output.status.success() {
            bail!("git clone failed for {}", source);
        }

        let dockerfile_path = format!("/tmp/kdct-build-{}/Dockerfile", name);
        if !std::path::Path::new(&dockerfile_path).exists() {
            bail!(
                "No Dockerfile found in {}. A Dockerfile is required for Git-sourced images.",
                source
            );
        }
    }

    db.insert_image(&name, source, source_type)?;
    info!("Image {} loaded", name);
    Ok(name)
}
