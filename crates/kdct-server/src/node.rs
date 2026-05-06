use anyhow::Result;

use crate::db::Database;

/// Show connected nodes.
pub fn list_nodes(db: &Database) -> Result<()> {
    let nodes = db.list_nodes()?;

    if nodes.is_empty() {
        println!("No nodes found.");
        return Ok(());
    }

    println!(
        "{:<4} {:<20} {:<12} {:<8} {:>6} {:>6} {}",
        "ID", "HOSTNAME", "OS", "STATUS", "CPU", "MEM", "DOCKER"
    );
    for n in &nodes {
        println!(
            "{:<4} {:<20} {:<12} {:<8} {:>5}c {:>4}MB {}",
            n.id, n.hostname, n.os, n.status, n.cpu_cores, n.memory_mb, n.docker_version
        );
    }

    Ok(())
}

/// Show details for a specific node.
pub fn show_node(db: &Database, node_id: i64) -> Result<()> {
    let nodes = db.list_nodes()?;
    let node = nodes
        .iter()
        .find(|n| n.id == node_id)
        .ok_or_else(|| anyhow::anyhow!("Node not found: {}", node_id))?;

    println!("Node: {} (ID: {})", node.hostname, node.id);
    println!("  OS: {} {}", node.os, node.arch);
    println!("  Docker: {}", node.docker_version);
    println!("  Status: {}", node.status);
    println!(
        "  Resources: {} CPU cores, {} MB memory",
        node.cpu_cores, node.memory_mb
    );
    println!(
        "  Port range: {}-{}",
        node.port_range_start, node.port_range_end
    );

    Ok(())
}
