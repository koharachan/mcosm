use crate::args::Args;
use crate::coordinate_system::geographic::LLBBox;
use crate::coordinate_system::transformation::CoordTransformer;
use crate::data_processing::{generate_world_from_data, GenerationOptions};
use crate::retrieve_data;
use crate::world_editor::WorldFormat;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const PROGRESS_VERSION: u32 = 1;
const REGION_SIZE_BLOCKS: i32 = 512;

#[derive(Serialize, Deserialize, Default)]
struct ProgressState {
    version: u32,
    total_tiles: (usize, usize),
    completed: Vec<(usize, usize)>,
    failed: Vec<(usize, usize)>,
}

impl ProgressState {
    fn completed_set(&self) -> HashSet<(usize, usize)> {
        self.completed.iter().cloned().collect()
    }
}

fn progress_path(final_dir: &Path) -> PathBuf {
    final_dir.join(".arnis_progress.json")
}

fn load_progress(path: &Path) -> Option<ProgressState> {
    let data = std::fs::read_to_string(path).ok()?;
    let state: ProgressState = serde_json::from_str(&data).ok()?;
    if state.version != PROGRESS_VERSION {
        return None;
    }
    Some(state)
}

fn save_progress(path: &Path, state: &ProgressState) -> Result<(), String> {
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Split a bbox into tiles. Returns (tile_x, tile_z, core_bbox, data_bbox).
fn split_bbox(
    bbox: LLBBox,
    tiles_x: usize,
    tiles_z: usize,
    overlap_ratio: f64,
) -> Vec<(usize, usize, LLBBox, LLBBox)> {
    let min_lat = bbox.min().lat();
    let min_lng = bbox.min().lng();
    let max_lat = bbox.max().lat();
    let max_lng = bbox.max().lng();

    let lat_step = (max_lat - min_lat) / tiles_z as f64;
    let lng_step = (max_lng - min_lng) / tiles_x as f64;

    let lat_overlap = lat_step * overlap_ratio;
    let lng_overlap = lng_step * overlap_ratio;

    let mut result = Vec::with_capacity(tiles_x * tiles_z);

    for tz in 0..tiles_z {
        for tx in 0..tiles_x {
            let core_min_lat = min_lat + tz as f64 * lat_step;
            let core_max_lat = min_lat + (tz + 1) as f64 * lat_step;
            let core_min_lng = min_lng + tx as f64 * lng_step;
            let core_max_lng = min_lng + (tx + 1) as f64 * lng_step;

            let data_min_lat = (core_min_lat - lat_overlap).max(-90.0);
            let data_max_lat = (core_max_lat + lat_overlap).min(90.0);
            let data_min_lng = (core_min_lng - lng_overlap).max(-180.0);
            let data_max_lng = (core_max_lng + lng_overlap).min(180.0);

            let core = LLBBox::new(core_min_lat, core_min_lng, core_max_lat, core_max_lng)
                .expect("valid core bbox");
            let data = LLBBox::new(data_min_lat, data_min_lng, data_max_lat, data_max_lng)
                .expect("valid data bbox");

            result.push((tx, tz, core, data));
        }
    }

    result
}

/// Compute which region coordinates belong to a given core bbox, using global bbox for coordinate origin.
fn core_region_range(core_bbox: LLBBox, global_bbox: LLBBox, scale: f64) -> Option<(i32, i32, i32, i32)> {
    let (transformer, _) = CoordTransformer::llbbox_to_xzbbox(&global_bbox, scale).ok()?;

    let min_x = transformer.transform_point(crate::coordinate_system::geographic::LLPoint::new(
        core_bbox.min().lat(),
        core_bbox.min().lng(),
    ).unwrap()).x as i32;
    let max_x = transformer.transform_point(crate::coordinate_system::geographic::LLPoint::new(
        core_bbox.max().lat(),
        core_bbox.max().lng(),
    ).unwrap()).x as i32;
    let min_z = transformer.transform_point(crate::coordinate_system::geographic::LLPoint::new(
        core_bbox.min().lat(),
        core_bbox.min().lng(),
    ).unwrap()).z as i32;
    let max_z = transformer.transform_point(crate::coordinate_system::geographic::LLPoint::new(
        core_bbox.max().lat(),
        core_bbox.max().lng(),
    ).unwrap()).z as i32;

    let min_rx = min_x.div_euclid(REGION_SIZE_BLOCKS);
    let max_rx = max_x.div_euclid(REGION_SIZE_BLOCKS);
    let min_rz = min_z.div_euclid(REGION_SIZE_BLOCKS);
    let max_rz = max_z.div_euclid(REGION_SIZE_BLOCKS);

    Some((min_rx, max_rx, min_rz, max_rz))
}

/// Move region files that belong to the core range from temp_dir to final_dir.
fn merge_tile_regions(
    temp_dir: &Path,
    final_dir: &Path,
    min_rx: i32,
    max_rx: i32,
    min_rz: i32,
    max_rz: i32,
) -> Result<(), String> {
    let temp_region = temp_dir.join("region");
    let final_region = final_dir.join("region");
    std::fs::create_dir_all(&final_region).map_err(|e| e.to_string())?;

    if !temp_region.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&temp_region).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Parse r.X.Z.mca
        if !name_str.starts_with("r.") || !name_str.ends_with(".mca") {
            continue;
        }
        let parts: Vec<&str> = name_str.trim_start_matches("r.").trim_end_matches(".mca").split('.').collect();
        if parts.len() != 2 {
            continue;
        }
        let rx: i32 = parts[0].parse().map_err(|_| format!("invalid region filename: {}", name_str))?;
        let rz: i32 = parts[1].parse().map_err(|_| format!("invalid region filename: {}", name_str))?;

        if rx >= min_rx && rx <= max_rx && rz >= min_rz && rz <= max_rz {
            let src = entry.path();
            let dst = final_region.join(&name);
            // If destination exists (from a previous run or overlapping tile), overwrite
            std::fs::copy(&src, &dst).map_err(|e| format!("copy {}: {}", name_str, e))?;
        }
    }

    Ok(())
}

/// Run chunked generation for large areas.
/// Currently supports Java Anvil only.
pub fn run_chunked_generation(
    args: &Args,
    file_path: &str,
    options: &GenerationOptions,
) -> Result<PathBuf, String> {
    if options.format == WorldFormat::BedrockMcWorld {
        return Err("Chunked generation is only supported for Java Edition worlds.".to_string());
    }

    let tiles_x = args.tiles_x;
    let tiles_z = args.tiles_z;
    let overlap = args.tile_overlap;

    if tiles_x == 1 && tiles_z == 1 {
        // Fallback to normal generation
        let raw_data = retrieve_data::fetch_data_from_file(file_path)
            .map_err(|e| format!("Failed to fetch data: {}", e))?;
        let mut opts = options.clone();
        opts.path = options.path.clone();
        generate_world_from_data(args, raw_data, opts)?;
        return Ok(options.path.clone());
    }

    let final_dir = options.path.clone();
    let progress_file = progress_path(&final_dir);
    let mut progress = load_progress(&progress_file).unwrap_or_default();
    progress.version = PROGRESS_VERSION;
    progress.total_tiles = (tiles_x, tiles_z);

    let completed = progress.completed_set();
    let tiles = split_bbox(args.bbox, tiles_x, tiles_z, overlap);

    println!(
        "Running chunked generation: {}x{} tiles, {} total",
        tiles_x,
        tiles_z,
        tiles.len()
    );

    // Ensure final world skeleton exists (level.dat, region dir, etc.)
    // We create it by generating a dummy tiny tile or reuse world_utils.
    // Simpler: let tile (0,0) create the full skeleton, others just copy regions.
    let mut skeleton_created = final_dir.join("level.dat").exists();

    for (tx, tz, core_bbox, data_bbox) in tiles {
        if completed.contains(&(tx, tz)) {
            println!("Skipping completed tile ({}, {})", tx, tz);
            continue;
        }

        println!("--- Processing tile ({}, {}) / ({}, {}) ---", tx, tz, tiles_x - 1, tiles_z - 1);
        println!("  Core bbox: {:?}", core_bbox);
        println!("  Data bbox: {:?}", data_bbox);

        let tile_dir = final_dir.join(format!(".tile_{}_{}", tx, tz));
        std::fs::create_dir_all(&tile_dir).map_err(|e| e.to_string())?;

        let raw_data = match retrieve_data::fetch_data_from_file_with_bbox(file_path, &data_bbox) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Tile ({}, {}) data fetch failed: {}", tx, tz, e);
                progress.failed.push((tx, tz));
                save_progress(&progress_file, &progress).map_err(|e| e.to_string())?;
                continue;
            }
        };

        // Use global bbox for coordinate transformation so all tiles share the same origin.
        // Data is already filtered by data_bbox during fetch.
        let tile_args = args.clone();

        let tile_opts = GenerationOptions {
            path: tile_dir.clone(),
            format: options.format,
            level_name: options.level_name.clone(),
            spawn_point: None,
        };

        if let Err(e) = generate_world_from_data(&tile_args, raw_data, tile_opts) {
            eprintln!("Tile ({}, {}) generation failed: {}", tx, tz, e);
            progress.failed.push((tx, tz));
            save_progress(&progress_file, &progress).map_err(|e| e.to_string())?;
            continue;
        }

        // For the first successful tile, copy world skeleton to final dir
        if !skeleton_created {
            copy_world_skeleton(&tile_dir, &final_dir)?;
            skeleton_created = true;
        }

        // Move core regions to final dir
        if let Some((min_rx, max_rx, min_rz, max_rz)) = core_region_range(core_bbox, args.bbox, args.scale) {
            println!("  Core region range: rx={}..{}, rz={}..{}", min_rx, max_rx, min_rz, max_rz);
            if let Err(e) = merge_tile_regions(&tile_dir, &final_dir, min_rx, max_rx, min_rz, max_rz) {
                eprintln!("Tile ({}, {}) region merge failed: {}", tx, tz, e);
                progress.failed.push((tx, tz));
                save_progress(&progress_file, &progress).map_err(|e| e.to_string())?;
                continue;
            }
        }

        // Clean up temp tile dir
        // let _ = std::fs::remove_dir_all(&tile_dir);
        println!("  Temp tile dir kept for debugging: {}", tile_dir.display());

        progress.completed.push((tx, tz));
        save_progress(&progress_file, &progress).map_err(|e| e.to_string())?;
        println!("Tile ({}, {}) completed.", tx, tz);
    }

    println!("Chunked generation finished. World at: {}", final_dir.display());
    Ok(final_dir)
}

fn copy_world_skeleton(src: &Path, dst: &Path) -> Result<(), String> {
    // Copy level.dat
    let src_level = src.join("level.dat");
    let dst_level = dst.join("level.dat");
    if src_level.exists() {
        std::fs::copy(&src_level, &dst_level).map_err(|e| e.to_string())?;
    }

    // Copy icon.png
    let src_icon = src.join("icon.png");
    let dst_icon = dst.join("icon.png");
    if src_icon.exists() {
        std::fs::copy(&src_icon, &dst_icon).map_err(|e| e.to_string())?;
    }

    // Copy datapacks if present
    let src_dp = src.join("datapacks");
    let dst_dp = dst.join("datapacks");
    if src_dp.exists() {
        copy_dir_recursive(&src_dp, &dst_dp)?;
    }

    // Ensure region dir exists
    std::fs::create_dir_all(dst.join("region")).map_err(|e| e.to_string())?;

    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
