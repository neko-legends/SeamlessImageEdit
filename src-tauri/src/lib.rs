use base64::{engine::general_purpose, Engine as _};
use image::{ImageFormat, Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;

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

    output
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

    let seamless = make_seamless(&image, mode, options.blend_percent);
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
            assert_eq!(*output.get_pixel(0, y), sample_shifted(&source, 0, y, 8, 0));
            assert_eq!(
                *output.get_pixel(15, y),
                sample_shifted(&source, 15, y, 8, 0)
            );
        }
    }

    #[test]
    fn tile_mode_preserves_wrapped_corners_and_repairs_center() {
        let source = gradient(16, 12);
        let output = make_seamless(&source, "tile", 18.0);
        assert_eq!(*output.get_pixel(0, 0), sample_shifted(&source, 0, 0, 8, 6));
        assert_eq!(
            *output.get_pixel(15, 0),
            sample_shifted(&source, 15, 0, 8, 6)
        );
        assert_eq!(
            *output.get_pixel(0, 11),
            sample_shifted(&source, 0, 11, 8, 6)
        );
        assert_eq!(
            *output.get_pixel(15, 11),
            sample_shifted(&source, 15, 11, 8, 6)
        );
        assert_ne!(*output.get_pixel(8, 6), sample_shifted(&source, 8, 6, 8, 6));
    }
}
