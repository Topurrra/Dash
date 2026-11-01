# Third-party notices

Dash is licensed under the GNU General Public License v3.0 or later (see
`LICENSE`). It includes or ships the components below under their own
licenses. Each project's site has the full license text.

## Shipped with the Windows installer

| Component | Used for | License |
|---|---|---|
| [PDFium](https://pdfium.googlesource.com/pdfium/) | Reading PDF text | BSD-3-Clause, Apache-2.0 |
| [Tesseract OCR](https://github.com/tesseract-ocr/tesseract) | Reading text in images | Apache-2.0 |
| [tessdata_best](https://github.com/tesseract-ocr/tessdata_best) (eng, rus, kat) | OCR language models | Apache-2.0 |
| [Leptonica](http://www.leptonica.org/) | Image handling for Tesseract | BSD-2-Clause |
| libarchive, bzip2, giflib, libjpeg-turbo, libcurl, OpenSSL, xz (liblzma), libpng, libwebp, lz4, OpenJPEG, libtiff, zlib, zstd | Tesseract's image and compression libraries | Each under its own permissive license (BSD-style, MIT, Zlib, curl, Apache-2.0, libpng, libtiff, IJG) |
| Microsoft Visual C++ runtime | Needed by Tesseract | Microsoft Visual C++ Redistributable license |
| [EmbeddingGemma 2](https://huggingface.co/onnx-community/embeddinggemma-2-ONNX) | Search by meaning | Gemma Terms of Use (below) |

### Gemma

Gemma is provided under and subject to the Gemma Terms of Use found at
[ai.google.dev/gemma/terms](https://ai.google.dev/gemma/terms). Use of the
model is also subject to the
[Gemma Prohibited Use Policy](https://ai.google.dev/gemma/prohibited_use_policy).
These terms apply to the model files only, not to Dash's code.

## Rust libraries

Dash is built with about 600 Rust crates, all under licenses compatible with
the GPL-3.0: MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, Unicode-3.0, CC0,
Unlicense, BSL-1.0, CDLA-Permissive-2.0 and MPL-2.0. Notable ones:
[GPUI](https://github.com/zed-industries/zed) (Apache-2.0),
[Tantivy](https://github.com/quickwit-oss/tantivy) (MIT),
[ONNX Runtime](https://github.com/microsoft/onnxruntime) via `ort` (MIT),
[tokenizers](https://github.com/huggingface/tokenizers) (Apache-2.0),
[symphonia](https://github.com/pdeljanov/Symphonia) (MPL-2.0),
[rodio](https://github.com/RustAudio/rodio), [lofty](https://github.com/Serial-ATA/lofty-rs),
[redb](https://github.com/cberner/redb) (MIT or Apache-2.0).
`cargo metadata` lists every crate with its license.
