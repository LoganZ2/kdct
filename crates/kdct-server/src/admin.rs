use anyhow::{Context, Result};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;

const API_PORT: u16 = 9933;

fn http_get(path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", API_PORT))
        .context("Failed to connect to API. Is kdcts running?")?;
    let req = format!("GET {} HTTP/1.0\r\nHost: localhost\r\n\r\n", path);
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Ok(resp[body_start..].trim().to_string())
}

fn http_post(path: &str, body: &str) -> Result<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", API_PORT))
        .context("Failed to connect to API. Is kdcts running?")?;
    let req = format!(
        "POST {} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        path,
        body.len(),
        body
    );
    stream.write_all(req.as_bytes())?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    let body_start = resp.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Ok(resp[body_start..].trim().to_string())
}

pub async fn admin_request(command: &str) -> Result<()> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (endpoint, body) = match parts.get(0).copied() {
        Some("deploy") if parts.len() >= 3 => {
            let b = json!({"image": parts[1], "node_id": parts[2].parse::<i64>().unwrap_or(0)});
            ("/api/deploy", Some(b.to_string()))
        }
        Some("stop") if parts.len() >= 3 => {
            let b = json!({"image": parts[1], "node_id": parts[2].parse::<i64>().unwrap_or(0)});
            ("/api/stop", Some(b.to_string()))
        }
        Some("deployments") => ("/api/deployments", None),
        _ => anyhow::bail!("Unknown command: {}", command),
    };

    let text = if let Some(b) = body {
        http_post(endpoint, &b)?
    } else {
        http_get(endpoint)?
    };

    if endpoint == "/api/deployments" {
        let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        if list.is_empty() {
            println!("(no containers running)");
        } else {
            for item in &list {
                println!(
                    "{} | image={} | host={} | ports={:?} | status={}",
                    item["container_name"].as_str().unwrap_or("-"),
                    item["image"].as_str().unwrap_or("-"),
                    item["hostname"].as_str().unwrap_or("-"),
                    item["ports"].as_array().map(|a| a.len()).unwrap_or(0),
                    item["status"].as_str().unwrap_or("-"),
                );
            }
        }
    } else if text.starts_with('{') {
        let err: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if let Some(e) = err["error"].as_str() {
            eprintln!("{}", e);
        } else {
            println!("{}", text);
        }
    } else {
        println!("{}", text);
    }

    Ok(())
}
