# Bundled fonts

The native client embeds these static TTFs with `include_bytes!` in `apps/native/src/fonts.rs` and registers them with the text system at startup. The terminal families ship Regular (400), Medium (500), SemiBold (600) and Bold (700) upright, plus Italic and Bold Italic where the family has them; Cascadia Code has no static Medium, so it ships SemiBold only and a Medium weight draws its nearest face. Inter (the default), Geist and Schibsted Grotesk are the proportional families the interface can draw in, each at 400, 500, 600 and 700. Each family is licensed under the SIL Open Font License 1.1; its `OFL.txt` sits beside the fonts.

| Family | Folder | Faces | Version | Source |
|---|---|---|---|---|
| Geist Mono | `GeistMono/` | Regular, Medium, SemiBold, Bold, Italic, Bold Italic | 1.7.2 | https://github.com/vercel/geist-font/releases/download/v1.7.2/geist-font-v1.7.2.zip (`GeistMono/ttf/`, `OFL.txt`) |
| Fira Code | `FiraCode/` | Regular, Medium, SemiBold, Bold | 6.2 | https://github.com/tonsky/FiraCode/releases/download/6.2/Fira_Code_v6.2.zip (`ttf/`); `OFL.txt` is the repo's `LICENSE` at tag `6.2` |
| JetBrains Mono | `JetBrainsMono/` | Regular, Medium, SemiBold, Bold, Italic, Bold Italic | 2.304 | https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip (`fonts/ttf/`, `OFL.txt`) |
| Cascadia Code | `CascadiaCode/` | Regular, SemiBold, Bold, Italic, Bold Italic | 2407.24 | https://github.com/microsoft/cascadia-code/releases/download/v2407.24/CascadiaCode-2407.24.zip (`ttf/static/`); `OFL.txt` is the repo's `LICENSE` at tag `v2407.24` |
| Geist | `Geist/` | Regular, Medium, SemiBold, Bold | 1.7.2 | https://github.com/vercel/geist-font/releases/download/v1.7.2/geist-font-v1.7.2.zip (`geist-font/Geist/ttf/`, `geist-font/OFL.txt`) |
| Inter | `Inter/` | Regular, Medium, SemiBold, Bold | 4.1 | https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip (`extras/ttf/`); `OFL.txt` is the archive's `LICENSE.txt` |
| Schibsted Grotesk | `SchibstedGrotesk/` | Regular, Medium, SemiBold, Bold | 1.100 | https://github.com/schibsted/schibsted-grotesk/releases/download/1.100/schibsted-grotesk-fonts.zip (`fonts/ttf/`); `OFL.txt` is the repo's `OFL.txt` at tag `1.100` |

Fira Code has no italic upstream, so italic text in Fira Code is drawn with its nearest upright face (DirectWrite may slant it).

Geist, Inter and Schibsted Grotesk are proportional, so no terminal font picker lists them: terminal text draws in a monospaced family.

To update a family, replace its TTFs from the project's latest release and update its row here.
