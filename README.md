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

The editor offsets the source image by half the width and/or height so the original outer edges meet in the middle. It then repairs only the center blend band, leaving the new outer edges as wrapped pixels. Horizontal mode repairs left/right edges, vertical mode repairs top/bottom edges, and tile mode uses separate X/Y references so all four edges stay tileable.

## Development

```powershell
npm install
npm run dev
```

## Desktop Build

```powershell
npm run tauri:build
```

The Windows installer is written under `src-tauri\target\release\bundle\nsis`.

## License

MIT

