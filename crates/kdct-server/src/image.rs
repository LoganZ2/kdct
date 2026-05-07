use anyhow::{bail, Context, Result};
use tracing::info;

use crate::db::Database;

/// Load a Docker image (from Docker Hub or Git) and inspect its EXPOSE ports.
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
        // Pull the image
        let status = tokio::process::Command::new("docker")
            .args(["pull", source])
            .status()
            .await
            .context("Failed to pull Docker image")?;

        if !status.success() {
            bail!("docker pull failed for {}", source);
        }

        // Inspect EXPOSE ports
        let ports = inspect_image_exposed(source).await?;
        let image_id = db.insert_image(&name, source, source_type)?;

        // If no EXPOSE ports found, default to port 80
        let ports_to_insert = if ports.is_empty() {
            vec![80i64]
        } else {
            ports
        };

        for port in &ports_to_insert {
            db.insert_image_port(image_id, *port, "tcp")?;
        }

        info!("Image {} loaded with id={}, ports={:?}", name, image_id, ports_to_insert);
    } else {
        // Git source: just record the metadata, actual build happens on client
        let image_id = db.insert_image(&name, source, source_type)?;

        let _ = tokio::process::Command::new("git")
            .args(["clone", "--depth", "1", source])
            .arg(format!("/tmp/kdct-build-{}", name))
            .status()
            .await;

        let default_port = 80i64;
        db.insert_image_port(image_id, default_port, "tcp")?;

        info!("Git image {} loaded with id={}, default port {}", name, image_id, default_port);
    }

    Ok(name)
}

/// Inspect EXPOSE ports from a local Docker image.
async fn inspect_image_exposed(image: &str) -> Result<Vec<i64>> {
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
                .filter_map(|k| {
                    k.split('/')
                        .next()?
                        .parse::<i64>()
                        .ok()
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ports)
}

/// Configure route path for an image port.
pub async fn configure_route(
    db: &Database,
    image_name: &str,
    port: i64,
    path: &str,
) -> Result<()> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found")?;
    let ports = db.get_image_ports(image.id)?;

    let image_port = ports
        .iter()
        .find(|p| p.port == port)
        .context(format!("Port {} not exposed by image {}", port, image_name))?;

    db.set_image_route(image_port.id, path)?;
    db.update_image_status(image.id, "configured")?;

    info!(
        "Route configured: {} port {} -> path {}",
        image_name, port, path
    );

    Ok(())
}

/// Add an additional port mapping to an image (HOST:CONTAINER).
pub async fn add_port_mapping(db: &Database, image_name: &str, host_port: i64, container_port: i64) -> Result<()> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found")?;

    db.insert_image_port(image.id, container_port, "tcp")?;
    info!("Added port mapping: {}:{} for image {}", host_port, container_port, image_name);
    Ok(())
}

/// Show image details.
pub fn show_image(db: &Database, image_name: &str) -> Result<()> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found")?;

    println!("Image: {}", image.name);
    println!("  Source: {} ({})", image.source, image.source_type);
    println!("  Status: {}", image.status);

    let routes = db.get_image_routes(image.id)?;
    if routes.is_empty() {
        println!("  No routes configured");
    } else {
        println!("  Routes:");
        for (port, path) in &routes {
            let route_path = path.as_deref().unwrap_or("(not configured)");
            println!("    Port {} {} → {}", port.port, port.protocol, route_path);
        }
    }

    Ok(())
}
