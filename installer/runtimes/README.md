# Installer runtimes

Native files the Windows installer ships next to `dash.exe`. They're not in
git (binaries, and one file is 174 MB); put them here before running
`installer/build.ps1`.

```
runtimes/
├── pdfium-runtime/pdfium.dll            PDF text (pdfium-render)
├── tesseract-runtime/                   OCR (Tesseract 5.5 + leptonica 1.87)
│   ├── tesseract.exe
│   └── *.dll                            tesseract55, leptonica-1.87.0, archive, bz2, gif,
│                                        jpeg62, libcrypto-3-x64, libcurl, liblzma, libpng16,
│                                        libsharpyuv, libwebp, libwebpmux, lz4, openjp2,
│                                        tiff, z, zstd
├── msvc-runtime/*.dll                   Visual C++ runtime Tesseract needs: vcruntime140(_1),
│                                        msvcp140(_1, _2, _atomic_wait, _codecvt_ids), concrt140
├── tessdata-runtime/*.traineddata       OCR languages: eng, rus, kat
└── embedding-runtime/                   Search by meaning (EmbeddingGemma 2, q4 ONNX)
    ├── tokenizer.json
    └── onnx/model_q4.onnx, onnx/model_q4.onnx_data
```

## Where to get them

| Folder | Source |
|---|---|
| `pdfium-runtime` | A Windows x64 `pdfium.dll` build, e.g. [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) |
| `tesseract-runtime` | `vcpkg install tesseract:x64-windows`, then copy `installed\x64-windows\tools\tesseract\*.exe` and `*.dll` |
| `msvc-runtime` | Visual Studio's `VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT` |
| `tessdata-runtime` | [tessdata_best](https://github.com/tesseract-ocr/tessdata_best): `eng`, `rus`, `kat` |
| `embedding-runtime` | Hugging Face [`onnx-community/embeddinggemma-2-ONNX`](https://huggingface.co/onnx-community/embeddinggemma-2-ONNX), revision `daa72c51243991dfcaf9f9137d2c573d8f7790c0` |

Model files, pinned revision:

```bash
base=https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve/daa72c51243991dfcaf9f9137d2c573d8f7790c0
curl -L -o embedding-runtime/tokenizer.json "$base/tokenizer.json"
curl -L -o embedding-runtime/onnx/model_q4.onnx "$base/onnx/model_q4.onnx"
curl -L -o embedding-runtime/onnx/model_q4.onnx_data "$base/onnx/model_q4.onnx_data"
```

| File | SHA-256 |
|---|---|
| `tokenizer.json` | `4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4` |
| `onnx/model_q4.onnx` | `f9eeba97acddf139b8ee2ddf04bc30dceafa88de93fadf74d7644e0d61a477a9` |
| `onnx/model_q4.onnx_data` | `c3975f2d1ab7a1878ae31a7d7a9b7804a827aff3800b60dfceafce21cac3df49` |

Dash still runs without any of these: PDFs fall back to a simpler reader, and
OCR and search by meaning say they aren't installed.
