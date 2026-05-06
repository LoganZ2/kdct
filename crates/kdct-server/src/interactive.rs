use anyhow::{bail, Context, Result};
use dialoguer::{Input, theme::ColorfulTheme};

use crate::db::Database;
use crate::image;

pub async fn configure_routes_interactive(
    db: &Database,
    image_name: &str,
    env_vars: &[String],
) -> Result<()> {
    let image = db
        .get_image_by_name(image_name)?
        .context("Image not found. Use 'image load' first.")?;

    // Parse and store env vars
    let envs: Vec<(String, String)> = parse_env_vars(env_vars)?;
    if !envs.is_empty() {
        db.set_image_envs(image.id, &envs)?;
        println!("\n  Environment variables:");
        for (k, v) in &envs {
            println!("    {}={}", k, v);
        }
    }

    // Get ports with their route status
    let routes = db.get_image_routes(image.id)?;
    let unconfigured: Vec<_> = routes.iter().filter(|(_, path)| path.is_none()).collect();
    let configured: Vec<_> = routes.iter().filter(|(_, path)| path.is_some()).collect();

    if routes.is_empty() {
        bail!("No ports found for image '{}'. Try loading it again.", image_name);
    }

    println!("\n  Image: {} ({})", image.name, image.source);

    if !configured.is_empty() {
        println!("  Already configured:");
        for (port, path) in &configured {
            println!("    Port {} -> {}", port.port, path.as_ref().unwrap());
        }
    }

    if unconfigured.is_empty() {
        if configured.is_empty() {
            bail!("No ports to configure.");
        }
        println!("\n  All ports configured.\n");
        return Ok(());
    }

    println!("\n  Unconfigured ports: {}\n", unconfigured.len());

    let theme = ColorfulTheme::default();

    for (port, _) in unconfigured {
        println!("  ── Port {}/{} ──", port.port, port.protocol);

        let validation = |input: &String| -> std::result::Result<(), String> {
            if !input.starts_with('/') {
                return Err("Path must start with '/'".into());
            }
            Ok(())
        };

        let path: String = Input::with_theme(&theme)
            .with_prompt("  Route path")
            .default("/".into())
            .validate_with(validation)
            .interact_text()?;

        image::configure_route(db, image_name, port.port, &path).await?;

        println!("  ✓ Route saved: {} -> {}:{}\n", path, image_name, port.port);
    }

    println!("Configuration saved.\n");

    Ok(())
}

fn parse_env_vars(env_vars: &[String]) -> Result<Vec<(String, String)>> {
    let mut envs = Vec::new();
    for var in env_vars {
        match var.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                envs.push((k.trim().to_string(), v.trim().to_string()));
            }
            _ => bail!("Invalid env format: '{}'. Use KEY=VALUE", var),
        }
    }
    Ok(envs)
}
