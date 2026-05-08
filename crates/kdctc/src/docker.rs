use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

pub async fn docker_pull(image: &str) -> Result<()> {
    info!("Pulling image: {}", image);
    let status = Command::new("docker")
        .args(["pull", image])
        .status()
        .await
        .with_context(|| format!("Failed to pull image: {}", image))?;
    if status.success() {
        info!("Pull complete: {}", image);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker pull failed for {}", image))
    }
}

pub async fn docker_run(
    image_tag: &str,
    container_name: &str,
    port_map: &[(u16, u16)],
    env: &[(String, String)],
) -> Result<Vec<u16>> {
    info!("Running container: {} ({})", container_name, image_tag);

    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("-d")
        .arg("--name")
        .arg(container_name);

    for (host_port, container_port) in port_map {
        cmd.arg("-p").arg(format!("{}:{}", host_port, container_port));
    }

    for (k, v) in env {
        cmd.arg("-e").arg(format!("{}={}", k, v));
    }

    cmd.arg(image_tag);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("Failed to run container: {}", container_name))?;

    if output.status.success() {
        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!("Container started: {} ({})", container_name, container_id);

        let exposed_ports: Vec<u16> = port_map.iter().map(|(h, _)| *h).collect();
        Ok(exposed_ports)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("docker run failed: {}", stderr);
        Err(anyhow::anyhow!(
            "docker run failed for {}: {}",
            container_name,
            stderr
        ))
    }
}

pub async fn docker_stop(container_name: &str) -> Result<()> {
    info!("Stopping container: {}", container_name);
    let status = Command::new("docker")
        .args(["stop", container_name])
        .status()
        .await
        .with_context(|| format!("Failed to stop container: {}", container_name))?;

    if status.success() {
        let _ = Command::new("docker")
            .args(["rm", container_name])
            .status()
            .await;
        info!("Container stopped and removed: {}", container_name);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker stop failed for {}", container_name))
    }
}

pub async fn docker_build(git_url: &str, branch: &str, image_tag: &str) -> Result<()> {
    info!("Building image {} from {} (branch: {})", image_tag, git_url, branch);

    let tmp_dir = std::env::temp_dir().join(format!("kdct-build-{}", sanitize_tag(image_tag)));
    if tmp_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
    }

    let clone_status = Command::new("git")
        .args(["clone", "--depth", "1", "--branch", branch, git_url])
        .arg(&tmp_dir)
        .status()
        .await
        .with_context(|| format!("Failed to clone {}", git_url))?;

    if !clone_status.success() {
        return Err(anyhow::anyhow!("git clone failed for {}", git_url));
    }

    let build_status = Command::new("docker")
        .args(["build", "-t", image_tag, "."])
        .current_dir(&tmp_dir)
        .status()
        .await
        .with_context(|| format!("docker build failed for {}", image_tag))?;

    // Cleanup
    let _ = tokio::fs::remove_dir_all(&tmp_dir).await;

    if build_status.success() {
        info!("Build complete: {}", image_tag);
        Ok(())
    } else {
        Err(anyhow::anyhow!("docker build failed for {}", image_tag))
    }
}

fn sanitize_tag(tag: &str) -> String {
    tag.replace(['/', ':', '@', '\\'], "_")
}
