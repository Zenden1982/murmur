# Иконки бренда → шрифт

SVG-иконки из пакета бренда (`murmur-brand/icons`, линии stroke 2) превращаются в шрифт с теми же кодами символов, что у phosphor,
поэтому интерфейс подхватывает их без правок раскладки (`theme::install` ставит их раньше phosphor).

1. `cargo run --release --manifest-path scripts/icons/icontool/Cargo.toml -- <папка с SVG> > outlines.tsv` — линии → залитые контуры (usvg + tiny-skia).
2. `python -I scripts/icons/mkfont.py <egui-phosphor/res/> <egui-phosphor/src/variants/codepoints.rs> outlines.tsv <куда>` —
   `murmur-icons.ttf` (обычные) и `murmur-icons-fill.ttf` (заливка), метрики и сетка — как у phosphor. Соответствие phosphor → иконка — в начале скрипта.
3. Положить оба файла в `crates/client/assets/fonts/`.
