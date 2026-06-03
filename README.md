# Seamless Image Edit

Local desktop tool for turning source images into horizontally seamless, vertically seamless, or fully tileable outputs.

## Features

- Drag and drop images or folders
- Open individual images or scan folders recursively
- Make horizontal seams, vertical seams, or all four tile edges seamless
- Save as WebP by default or PNG
- Save beside the source image by default, or choose a separate output folder
- 2x2 tile preview to inspect repeat edges

## Algorithm

The main seam repair path uses Embark Studios' open-source `texture-synthesis` crate. The app masks the border band that needs to become tileable, asks the synthesis engine to inpaint that band with tiling mode enabled, then runs a small final edge polish so opposite outer pixels match exactly. Horizontal mode repairs left/right borders, vertical mode repairs top/bottom borders, and tile mode repairs all four borders.

If texture synthesis cannot complete for a file, the backend falls back to a deterministic offset-and-blend repair so the batch can keep moving.

Test fixtures are written to `src-tauri\target\visual-seam-test` during `cargo test`. They include a deliberately harsh non-seamless square, a 2x2 repeat before repair, and a 2x2 repeat after repair for visual inspection.

## Development

```powershell
npm install
npm run dev
```

## Headless CLI

The desktop binary can also process images without opening the GUI:

```powershell
src-tauri\target\debug\seamless-image-edit.exe --headless `
  --mode horizontal `
  --format webp `
  --output-dir D:\out `
  --suffix _seamless `
  --overwrite `
  D:\textures\road.png
```

Modes are `horizontal`, `vertical`, or `tile`. Use `--recursive` for folders and
`--same-folder` to save outputs beside each source image.

## Desktop Build

```powershell
npm run tauri:build
```

The Windows installer is written under `src-tauri\target\release\bundle\nsis`.

## License

MIT
