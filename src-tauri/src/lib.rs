use base64::{engine::general_purpose, Engine as _};
use image::{ImageFormat, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use texture_synthesis as texsynth;

const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "webp", "bmp", "tif", "tiff",
];

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SeamlessOptions {
    mode: String,
    output_format: String,
    same_folder: bool,
    output_dir: String,
    suffix: String,
    recursive: bool,
    overwrite: bool,
    blend_percent: f32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessResult {
    input_path: String,
    output_path: Option<String>,
    status: String,
    message: String,
}

fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn collect_images(path: &Path, recursive: bool, output: &mut Vec<String>) {
    if path.is_file() {
        if is_image_file(path) {
            output.push(path.display().to_string());
        }
        return;
    }

    if !path.is_dir() {
        return;
    }

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_file() && is_image_file(&entry_path) {
            output.push(entry_path.display().to_string());
        } else if recursive && entry_path.is_dir() {
            collect_images(&entry_path, recursive, output);
        }
    }
}

fn image_mime_type(path: &Path) -> Result<&'static str, String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg") | Some("jpeg") | Some("jpe") | Some("jfif") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        Some("bmp") => Ok("image/bmp"),
        Some("tif") | Some("tiff") => Ok("image/tiff"),
        Some(extension) => Err(format!("Unsupported preview image extension: {extension}")),
        None => Err("Preview image has no file extension.".to_string()),
    }
}

fn sanitized_suffix(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "_seamless".to_string();
    }

    let safe = trimmed
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();

    if safe.is_empty() {
        "_seamless".to_string()
    } else {
        safe
    }
}

fn normalized_format(value: &str) -> Result<(&'static str, ImageFormat), String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(("png", ImageFormat::Png)),
        "webp" => Ok(("webp", ImageFormat::WebP)),
        other => Err(format!("Unsupported output format: {other}")),
    }
}

fn normalized_mode(value: &str) -> Result<&'static str, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Ok("horizontal"),
        "vertical" => Ok("vertical"),
        "tile" => Ok("tile"),
        other => Err(format!("Unsupported seamless mode: {other}")),
    }
}

fn output_path_for(input_path: &Path, options: &SeamlessOptions) -> Result<PathBuf, String> {
    let (extension, _) = normalized_format(&options.output_format)?;
    let source_dir = input_path
        .parent()
        .ok_or_else(|| "Unable to resolve source folder.".to_string())?;
    let target_dir = if options.same_folder {
        source_dir.to_path_buf()
    } else {
        let output_dir = options.output_dir.trim();
        if output_dir.is_empty() {
            return Err("Choose an output folder or enable same-folder output.".to_string());
        }
        PathBuf::from(output_dir)
    };

    fs::create_dir_all(&target_dir)
        .map_err(|error| format!("Unable to create output folder: {error}"))?;

    let stem = input_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("image");
    let suffix = sanitized_suffix(&options.suffix);
    let base_name = format!("{stem}{suffix}");
    let candidate = target_dir.join(format!("{base_name}.{extension}"));
    if options.overwrite || !candidate.exists() {
        return Ok(candidate);
    }

    for index in 2..10_000 {
        let candidate = target_dir.join(format!("{base_name}-{index}.{extension}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err("Unable to find an unused output filename.".to_string())
}

fn sample_shifted(source: &RgbaImage, x: u32, y: u32, shift_x: u32, shift_y: u32) -> Rgba<u8> {
    let width = source.width();
    let height = source.height();
    *source.get_pixel((x + shift_x) % width, (y + shift_y) % height)
}

fn seam_weight(index: u32, size: u32, blend_percent: f32) -> f32 {
    if size <= 2 {
        return 0.0;
    }

    let percent = blend_percent.clamp(4.0, 45.0);
    let band = ((size as f32) * percent / 100.0).round().max(2.0);
    let radius = (band / 2.0).max(1.0);
    let center = size as f32 / 2.0;
    let distance = ((index as f32 + 0.5) - center).abs();
    let t = (1.0 - distance / radius).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn edge_band(size: u32, blend_percent: f32) -> u32 {
    if size <= 2 {
        return 1;
    }

    let percent = (blend_percent.clamp(4.0, 45.0) * 0.5).max(2.0);
    (((size as f32) * percent / 100.0).round() as u32)
        .max(1)
        .min((size / 2).max(1))
}

fn synthesis_band(size: u32, blend_percent: f32) -> u32 {
    if size <= 2 {
        return 1;
    }

    let raw = ((size as f32) * blend_percent.clamp(8.0, 42.0) / 100.0).round() as u32;
    raw.max(8).min((size / 3).max(1))
}

fn edge_weight(index: u32, band: u32) -> f32 {
    if band <= 1 {
        return 1.0;
    }
    let t = 1.0 - index as f32 / band.saturating_sub(1) as f32;
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn synthesis_mask(width: u32, height: u32, mode: &str, blend_percent: f32) -> RgbaImage {
    let x_band = synthesis_band(width, blend_percent);
    let y_band = synthesis_band(height, blend_percent);
    let mut mask = RgbaImage::from_pixel(width, height, Rgba([255, 255, 255, 255]));

    for y in 0..height {
        for x in 0..width {
            let in_horizontal_band = x < x_band || x >= width.saturating_sub(x_band);
            let in_vertical_band = y < y_band || y >= height.saturating_sub(y_band);
            let editable = match mode {
                "horizontal" => in_horizontal_band,
                "vertical" => in_vertical_band,
                "tile" => in_horizontal_band || in_vertical_band,
                _ => false,
            };

            if editable {
                mask.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
    }

    mask
}

fn temp_job_dir() -> Result<PathBuf, String> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock error: {error}"))?
        .as_millis();
    let dir = std::env::temp_dir().join(format!(
        "seamless-image-edit-{}-{millis}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(|error| format!("Unable to create temp folder: {error}"))?;
    Ok(dir)
}

fn mix2(a: Rgba<u8>, b: Rgba<u8>, weight_b: f32) -> Rgba<u8> {
    let weight_b = weight_b.clamp(0.0, 1.0);
    let weight_a = 1.0 - weight_b;
    let mut out = [0u8; 4];
    for channel in 0..4 {
        out[channel] =
            ((a.0[channel] as f32 * weight_a) + (b.0[channel] as f32 * weight_b)).round() as u8;
    }
    Rgba(out)
}

fn reconcile_horizontal_edges(image: &mut RgbaImage, blend_percent: f32) {
    let width = image.width();
    let height = image.height();
    let band = edge_band(width, blend_percent);
    for y in 0..height {
        for offset in 0..band {
            let left_x = offset;
            let right_x = width - 1 - offset;
            if left_x >= right_x {
                break;
            }

            let left = *image.get_pixel(left_x, y);
            let right = *image.get_pixel(right_x, y);
            let matched = mix2(left, right, 0.5);
            let weight = edge_weight(offset, band);
            image.put_pixel(left_x, y, mix2(left, matched, weight));
            image.put_pixel(right_x, y, mix2(right, matched, weight));
        }
    }
}

fn reconcile_vertical_edges(image: &mut RgbaImage, blend_percent: f32) {
    let width = image.width();
    let height = image.height();
    let band = edge_band(height, blend_percent);
    for x in 0..width {
        for offset in 0..band {
            let top_y = offset;
            let bottom_y = height - 1 - offset;
            if top_y >= bottom_y {
                break;
            }

            let top = *image.get_pixel(x, top_y);
            let bottom = *image.get_pixel(x, bottom_y);
            let matched = mix2(top, bottom, 0.5);
            let weight = edge_weight(offset, band);
            image.put_pixel(x, top_y, mix2(top, matched, weight));
            image.put_pixel(x, bottom_y, mix2(bottom, matched, weight));
        }
    }
}

fn mix_tile(
    base: Rgba<u8>,
    x_ref: Rgba<u8>,
    y_ref: Rgba<u8>,
    original: Rgba<u8>,
    wx: f32,
    wy: f32,
) -> Rgba<u8> {
    let wx = wx.clamp(0.0, 1.0);
    let wy = wy.clamp(0.0, 1.0);
    let weights = [
        (1.0 - wx) * (1.0 - wy),
        wx * (1.0 - wy),
        (1.0 - wx) * wy,
        wx * wy,
    ];
    let pixels = [base, x_ref, y_ref, original];
    let mut out = [0u8; 4];

    for channel in 0..4 {
        let value = pixels
            .iter()
            .zip(weights)
            .map(|(pixel, weight)| pixel.0[channel] as f32 * weight)
            .sum::<f32>();
        out[channel] = value.round().clamp(0.0, 255.0) as u8;
    }

    Rgba(out)
}

fn make_seamless(source: &RgbaImage, mode: &str, blend_percent: f32) -> RgbaImage {
    let width = source.width();
    let height = source.height();
    let shift_x = if matches!(mode, "horizontal" | "tile") {
        width / 2
    } else {
        0
    };
    let shift_y = if matches!(mode, "vertical" | "tile") {
        height / 2
    } else {
        0
    };
    let mut output = RgbaImage::new(width, height);

    for y in 0..height {
        let wy = if matches!(mode, "vertical" | "tile") {
            seam_weight(y, height, blend_percent)
        } else {
            0.0
        };

        for x in 0..width {
            let wx = if matches!(mode, "horizontal" | "tile") {
                seam_weight(x, width, blend_percent)
            } else {
                0.0
            };

            let pixel = match mode {
                "horizontal" => {
                    let wrapped = sample_shifted(source, x, y, shift_x, 0);
                    mix2(wrapped, *source.get_pixel(x, y), wx)
                }
                "vertical" => {
                    let wrapped = sample_shifted(source, x, y, 0, shift_y);
                    mix2(wrapped, *source.get_pixel(x, y), wy)
                }
                "tile" => {
                    let base = sample_shifted(source, x, y, shift_x, shift_y);
                    let x_ref = sample_shifted(source, x, y, 0, shift_y);
                    let y_ref = sample_shifted(source, x, y, shift_x, 0);
                    mix_tile(base, x_ref, y_ref, *source.get_pixel(x, y), wx, wy)
                }
                _ => *source.get_pixel(x, y),
            };

            output.put_pixel(x, y, pixel);
        }
    }

    if matches!(mode, "horizontal" | "tile") {
        reconcile_horizontal_edges(&mut output, blend_percent);
    }
    if matches!(mode, "vertical" | "tile") {
        reconcile_vertical_edges(&mut output, blend_percent);
    }

    output
}

fn make_content_aware_seamless(
    source: &RgbaImage,
    mode: &str,
    blend_percent: f32,
) -> Result<RgbaImage, String> {
    let temp_dir = temp_job_dir()?;
    let source_path = temp_dir.join("source.png");
    let mask_path = temp_dir.join("mask.png");
    let generated_path = temp_dir.join("generated.png");

    source
        .save(&source_path)
        .map_err(|error| format!("Unable to write synthesis source: {error}"))?;
    synthesis_mask(source.width(), source.height(), mode, blend_percent)
        .save(&mask_path)
        .map_err(|error| format!("Unable to write synthesis mask: {error}"))?;

    let example = texsynth::Example::builder(&source_path).set_sample_method(&mask_path);
    let session = texsynth::Session::builder()
        .inpaint_example(
            &mask_path,
            example,
            texsynth::Dims::new(source.width(), source.height()),
        )
        .tiling_mode(true)
        .seed(211)
        .nearest_neighbors(32)
        .random_sample_locations(32)
        .backtrack_percent(0.35)
        .backtrack_stages(3)
        .build()
        .map_err(|error| format!("Unable to build texture synthesis session: {error}"))?;

    session
        .run(None)
        .save(&generated_path)
        .map_err(|error| format!("Unable to save texture synthesis output: {error}"))?;

    let mut generated = image::open(&generated_path)
        .map_err(|error| format!("Unable to read texture synthesis output: {error}"))?
        .to_rgba8();
    let edge_polish = blend_percent.min(8.0);
    if matches!(mode, "horizontal" | "tile") {
        reconcile_horizontal_edges(&mut generated, edge_polish);
    }
    if matches!(mode, "vertical" | "tile") {
        reconcile_vertical_edges(&mut generated, edge_polish);
    }
    let _ = fs::remove_dir_all(&temp_dir);
    Ok(generated)
}

fn process_one(path: &Path, options: &SeamlessOptions) -> Result<PathBuf, String> {
    if !path.is_file() {
        return Err("Input path is not a file.".to_string());
    }

    let mode = normalized_mode(&options.mode)?;
    let (_, image_format) = normalized_format(&options.output_format)?;
    let image = image::open(path)
        .map_err(|error| format!("Unable to read image: {error}"))?
        .to_rgba8();
    if image.width() < 2 || image.height() < 2 {
        return Err("Image must be at least 2x2 pixels.".to_string());
    }

    let seamless = make_content_aware_seamless(&image, mode, options.blend_percent)
        .unwrap_or_else(|_| make_seamless(&image, mode, options.blend_percent));
    let output_path = output_path_for(path, options)?;
    seamless
        .save_with_format(&output_path, image_format)
        .map_err(|error| format!("Unable to save output: {error}"))?;
    Ok(output_path)
}

fn process_paths(
    app: AppHandle,
    paths: Vec<String>,
    options: SeamlessOptions,
) -> Result<Vec<ProcessResult>, String> {
    let mut results = Vec::new();
    for path in paths {
        let _ = app.emit(
            "seamless-worker-event",
            serde_json::json!({ "type": "image_start", "path": path }),
        );
        let input_path = PathBuf::from(&path);
        match process_one(&input_path, &options) {
            Ok(output_path) => {
                let output = output_path.display().to_string();
                let _ = app.emit(
                    "seamless-worker-event",
                    serde_json::json!({ "type": "image_done", "path": path, "output": output }),
                );
                results.push(ProcessResult {
                    input_path: path,
                    output_path: Some(output),
                    status: "done".to_string(),
                    message: "Saved".to_string(),
                });
            }
            Err(error) => {
                let _ = app.emit(
                    "seamless-worker-event",
                    serde_json::json!({ "type": "image_error", "path": path, "message": error }),
                );
                results.push(ProcessResult {
                    input_path: path,
                    output_path: None,
                    status: "error".to_string(),
                    message: error,
                });
            }
        }
    }

    let _ = app.emit(
        "seamless-worker-event",
        serde_json::json!({ "type": "done" }),
    );
    Ok(results)
}

#[tauri::command]
fn resolve_inputs(paths: Vec<String>, recursive: bool) -> Vec<String> {
    let mut output = Vec::new();
    for path in paths {
        collect_images(Path::new(&path), recursive, &mut output);
    }
    output.sort();
    output.dedup();
    output
}

#[tauri::command]
fn preview_image_data_url(path: String) -> Result<String, String> {
    let image_path = PathBuf::from(path);
    if !image_path.is_file() {
        return Err("Preview image was not found.".to_string());
    }
    let mime_type = image_mime_type(&image_path)?;
    let bytes =
        fs::read(&image_path).map_err(|error| format!("Unable to read preview image: {error}"))?;
    Ok(format!(
        "data:{mime_type};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

#[tauri::command]
fn open_containing_folder(app: AppHandle, path: String) -> Result<(), String> {
    let requested_path = PathBuf::from(path);
    let folder = if requested_path.is_dir() {
        requested_path
    } else {
        requested_path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "Unable to resolve containing folder.".to_string())?
    };
    if !folder.is_dir() {
        return Err(format!("Folder does not exist: {}", folder.display()));
    }
    app.opener()
        .open_path(folder.display().to_string(), None::<&str>)
        .map_err(|error| format!("Unable to open folder: {error}"))
}

#[tauri::command]
async fn start_seamless_job(
    app: AppHandle,
    paths: Vec<String>,
    options: SeamlessOptions,
) -> Result<Vec<ProcessResult>, String> {
    if paths.is_empty() {
        return Err("No input images were provided.".to_string());
    }

    tauri::async_runtime::spawn_blocking(move || process_paths(app, paths, options))
        .await
        .map_err(|error| format!("Seamless worker failed: {error}"))?
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            open_containing_folder,
            preview_image_data_url,
            resolve_inputs,
            start_seamless_job
        ])
        .run(tauri::generate_context!())
        .expect("error while running Tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> RgbaImage {
        let mut image = RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                image.put_pixel(
                    x,
                    y,
                    Rgba([(x * 17) as u8, (y * 19) as u8, (x + y) as u8, 255]),
                );
            }
        }
        image
    }

    #[test]
    fn horizontal_mode_preserves_wrapped_outer_edges() {
        let source = gradient(16, 12);
        let output = make_seamless(&source, "horizontal", 18.0);
        for y in 0..source.height() {
            assert_eq!(*output.get_pixel(0, y), *output.get_pixel(15, y));
        }
    }

    #[test]
    fn tile_mode_preserves_wrapped_corners_and_repairs_center() {
        let source = gradient(16, 12);
        let output = make_seamless(&source, "tile", 18.0);
        assert_eq!(*output.get_pixel(0, 0), *output.get_pixel(15, 0));
        assert_eq!(*output.get_pixel(0, 11), *output.get_pixel(15, 11));
        assert_eq!(*output.get_pixel(0, 0), *output.get_pixel(0, 11));
        assert_eq!(*output.get_pixel(15, 0), *output.get_pixel(15, 11));
        assert_ne!(*output.get_pixel(8, 6), sample_shifted(&source, 8, 6, 8, 6));
    }

    fn harsh_nonseamless_fixture(size: u32) -> RgbaImage {
        let mut image = RgbaImage::new(size, size);
        for y in 0..size {
            for x in 0..size {
                let checker = if ((x / 16) + (y / 16)) % 2 == 0 {
                    30
                } else {
                    95
                };
                let red = ((x as f32 / size as f32) * 190.0).round() as u8;
                let green = ((y as f32 / size as f32) * 180.0).round() as u8;
                image.put_pixel(x, y, Rgba([red.saturating_add(checker), green, 150, 255]));
            }
        }

        for i in 0..size {
            image.put_pixel(0, i, Rgba([255, 0, 0, 255]));
            image.put_pixel(size - 1, i, Rgba([0, 255, 255, 255]));
            image.put_pixel(i, 0, Rgba([255, 255, 0, 255]));
            image.put_pixel(i, size - 1, Rgba([0, 0, 255, 255]));
        }

        for i in 20..(size - 20) {
            image.put_pixel(i, i, Rgba([255, 255, 255, 255]));
            image.put_pixel(size - 1 - i, i, Rgba([0, 0, 0, 255]));
        }

        image
    }

    fn tile_2x2(source: &RgbaImage) -> RgbaImage {
        let mut tiled = RgbaImage::new(source.width() * 2, source.height() * 2);
        for tile_y in 0..2 {
            for tile_x in 0..2 {
                for y in 0..source.height() {
                    for x in 0..source.width() {
                        tiled.put_pixel(
                            tile_x * source.width() + x,
                            tile_y * source.height() + y,
                            *source.get_pixel(x, y),
                        );
                    }
                }
            }
        }
        tiled
    }

    fn paste(target: &mut RgbaImage, source: &RgbaImage, offset_x: u32, offset_y: u32) {
        for y in 0..source.height() {
            for x in 0..source.width() {
                target.put_pixel(offset_x + x, offset_y + y, *source.get_pixel(x, y));
            }
        }
    }

    fn seam_score(image: &RgbaImage) -> f32 {
        let width = image.width();
        let height = image.height();
        let horizontal = (0..height)
            .map(|y| channel_delta(*image.get_pixel(0, y), *image.get_pixel(width - 1, y)))
            .sum::<f32>()
            / height as f32;
        let vertical = (0..width)
            .map(|x| channel_delta(*image.get_pixel(x, 0), *image.get_pixel(x, height - 1)))
            .sum::<f32>()
            / width as f32;
        (horizontal + vertical) / 2.0
    }

    fn channel_delta(a: Rgba<u8>, b: Rgba<u8>) -> f32 {
        (0..3)
            .map(|channel| (a.0[channel] as f32 - b.0[channel] as f32).abs())
            .sum::<f32>()
            / 3.0
    }

    #[test]
    fn writes_visual_tile_fixture_and_reduces_edge_seams() {
        let source = harsh_nonseamless_fixture(160);
        let output =
            make_content_aware_seamless(&source, "tile", 22.0).expect("content-aware seamless");
        let source_score = seam_score(&source);
        let output_score = seam_score(&output);
        println!("visual seam score source={source_score:.2}, output={output_score:.2}");
        assert!(
            output_score <= 0.01,
            "expected opposite tile edges to match, source={source_score:.2}, output={output_score:.2}"
        );

        let output_dir = std::env::current_dir()
            .expect("current dir")
            .join("target")
            .join("visual-seam-test");
        std::fs::create_dir_all(&output_dir).expect("create visual seam test dir");
        source
            .save(output_dir.join("01-source-not-seamless.png"))
            .expect("save source fixture");
        output
            .save(output_dir.join("02-output-seamless.png"))
            .expect("save seamless fixture");
        let source_tiled = tile_2x2(&source);
        let output_tiled = tile_2x2(&output);
        source_tiled
            .save(output_dir.join("03-source-tiled-2x2.png"))
            .expect("save source tiled fixture");
        output_tiled
            .save(output_dir.join("04-output-tiled-2x2.png"))
            .expect("save output tiled fixture");

        let gutter = 12;
        let mut contact = RgbaImage::from_pixel(
            source_tiled.width() + output_tiled.width() + gutter,
            source_tiled.height().max(output_tiled.height()),
            Rgba([12, 16, 24, 255]),
        );
        paste(&mut contact, &source_tiled, 0, 0);
        paste(
            &mut contact,
            &output_tiled,
            source_tiled.width() + gutter,
            0,
        );
        contact
            .save(output_dir.join("05-before-after-tiled-contact.png"))
            .expect("save visual contact sheet");
    }
}
