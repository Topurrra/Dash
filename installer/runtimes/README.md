# Installer runtimes

Native files the Windows installer ships next to `dash.exe`. They're not in
git (binaries, and one file is 123 MB); put them here before running
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
└── embedding-runtime/                   Search by meaning (Model2Vec potion-retrieval-32M, MIT)
    ├── tokenizer.json
    ├── model.safetensors
    └── config.json
```

## Where to get them

| Folder | Source |
|---|---|
| `pdfium-runtime` | A Windows x64 `pdfium.dll` build, e.g. [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) |
| `tesseract-runtime` | `vcpkg install tesseract:x64-windows`, then copy `installed\x64-windows\tools\tesseract\*.exe` and `*.dll` |
| `msvc-runtime` | Visual Studio's `VC\Redist\MSVC\<version>\x64\Microsoft.VC143.CRT` |
| `tessdata-runtime` | [tessdata_best](https://github.com/tesseract-ocr/tessdata_best): `eng`, `rus`, `kat` |
| `embedding-runtime` | Hugging Face [`minishlab/potion-retrieval-32M`](https://huggingface.co/minishlab/potion-retrieval-32M), revision `6fc8051fab2a1e0ee76689cf08c853792ac285e7` |

Model files, pinned revision:

```bash
base=https://huggingface.co/minishlab/potion-retrieval-32M/resolve/6fc8051fab2a1e0ee76689cf08c853792ac285e7
curl -L -o embedding-runtime/tokenizer.json "$base/tokenizer.json"
curl -L -o embedding-runtime/model.safetensors "$base/model.safetensors"
curl -L -o embedding-runtime/config.json "$base/config.json"
```

| File | SHA-256 |
|---|---|
| `tokenizer.json` | `7d75cbc54318138807c401b0f0c9721117c628b39de8e8e0edb6cb17e0ee7d18` |
| `model.safetensors` | `07609e5bd33aad37900b3fd62f4ec96f6daec88ca4d46b9d8b928bfababf6ea0` |
| `config.json` | `63c00d90824c832c04ec1d02b6a983fb90489bf049f29fbff15ba481b8a432ee` |

Dash still runs without any of these: PDFs fall back to a simpler reader, and
OCR and search by meaning say they aren't installed.
