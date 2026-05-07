use anyhow::Result;

use crate::db::Database;

fn green(s: &str) -> String {
    format!("\x1b[32m{}\x1b[0m", s)
}

fn red(s: &str) -> String {
    format!("\x1b[31m{}\x1b[0m", s)
}

fn display_width(s: &str) -> usize {
    // Strip ANSI escape sequences to compute visual width
    let mut width = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            width += 1;
        }
    }
    width
}

fn pad_visual(s: &str, width: usize) -> String {
    let visual = display_width(s);
    if visual >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - visual))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

/// Show connected nodes.
pub fn list_nodes(db: &Database) -> Result<()> {
    let nodes = db.list_nodes()?;

    if nodes.is_empty() {
        println!("No nodes found.");
        return Ok(());
    }

    // Compute column widths
    let max_host = nodes.iter().map(|n| n.hostname.len()).max().unwrap_or(8).min(28);
    let max_docker = nodes.iter().map(|n| {
        // Extract just "29.4.0" from "Docker version 29.4.0, build ..."
        n.docker_version.split_whitespace()
            .nth(2)
            .unwrap_or(&n.docker_version)
            .len()
    }).max().unwrap_or(8).min(20);

    let hdr_id = " ID";
    let hdr_host = pad_visual("HOSTNAME", max_host + 2);
    let hdr_os = pad_visual("OS", 8);
    let hdr_status = "STATUS   ";
    let hdr_cpu = "  CPU";
    let hdr_mem = "     MEM";
    let hdr_docker = " DOCKER";

    println!(
        "{}{}{}{}{}{}{}",
        hdr_id, hdr_host, hdr_os, hdr_status, hdr_cpu, hdr_mem, hdr_docker
    );

    let total = 4 + (max_host + 2) + 8 + 10 + 6 + 8 + max_docker + 2;
    println!("{}", "-".repeat(total));

    for n in &nodes {
        let (icon, status_str) = if n.status == "online" {
            ("●", green("online"))
        } else {
            ("○", red("offline"))
        };

        let mem_gb = n.memory_mb as f64 / 1024.0;

        let host = truncate(&n.hostname, max_host);
        let host_col = pad_visual(&host, max_host + 2);

        let os_col = pad_visual(&n.os, 8);
        let status_col = pad_visual(&status_str, 10);

        let docker_short = n.docker_version.split_whitespace()
            .nth(2)
            .unwrap_or(&n.docker_version)
            .to_string();

        println!(
            "{} {:<4} {}{}{} {:>4}c  {:>5.1}GB  {}",
            icon, n.id, host_col, os_col, status_col,
            n.cpu_cores, mem_gb, docker_short
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

    let (icon, status_str) = if node.status == "online" {
        ("●", green("online"))
    } else {
        ("○", red("offline"))
    };

    let mem_gb = node.memory_mb as f64 / 1024.0;

    println!("{} {}  (ID: {})", icon, node.hostname, node.id);
    println!("  Status:    {}", status_str);
    println!("  OS:        {} {}", node.os, node.arch);
    println!("  Docker:    {}", node.docker_version);
    println!("  CPU:       {} cores", node.cpu_cores);
    println!("  Memory:    {:.1} GB", mem_gb);
    println!("  Ports:     {}-{}", node.port_range_start, node.port_range_end);

    Ok(())
}
