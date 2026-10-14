# The file tree's icons

Lucide's glyphs, copied verbatim from [gpui-kit] v0.7.1's bundled set — the same
files `gpui_kit::assets::Assets` serves — for the nineteen that set does not
carry. `file-json.svg` and `file-video.svg` come from [lucide] itself, which has
them and that older set does not.

The tree draws them through `crate::file_icons`, which embeds each one with
`include_bytes!` and hands the bytes to `Icon::data`. That is why they are here
rather than in the asset source: `AllAssets` would embed the whole catalogue for
them. Do not edit these files; the licence is [ISC], in `LICENSE`.

[gpui-kit]: https://github.com/longbridge/gpui-kit
[lucide]: https://lucide.dev
[ISC]: https://github.com/lucide-icons/lucide/blob/main/LICENSE
