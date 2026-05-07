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

    if source_type == "docker_hub" {
        let status = tokio::process::Command::new("docker")
            .args(["pull", source])
            .status()
            .await
            .context("Failed to pull Docker image")?;

        if !status.success() {
            bail!("docker pull failed for {}", source);
        }
    } else {
        let _ = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", source])
            .arg(format!("/tmp/kdct-build-{}", name))
            .status()
            .await;
    }

    db.insert_image(&name, source, source_type)?;
    info!("Image {} loaded", name);
    Ok(name)
}

pub async fn inspect_exposed_ports(image: &str) -> Result<Vec<i64>> {
    let output = tokio::process::Command::new("docker")
        .args(["inspect", image])
        .output()
        .await
        .context("Failed to inspect Docker image")?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).context("Failed to parse docker inspect JSON")?;

    let config = &json[0]["Config"]["ExposedPorts"];
    let ports: Vec<i64> = config
        .as_object()
        .map(|obj| {
            obj.keys()
                .filter_map(|k| k.split('/').next()?.parse::<i64>().ok())
                .collect()
        })
        .unwrap_or_default();

    Ok(ports)
}
