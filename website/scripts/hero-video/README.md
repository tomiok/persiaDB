# Landing-page hero video

`render.py` draws the 16-second, 1920×1080 loop with Pillow and writes raw RGB24
frames to stdout. Run the commands below from this directory. They write the
files the site serves, under `website/public/`:

- `persia-hero.mp4`
- `persia-hero.webm`
- `poster.png`

Pillow and ffmpeg are required. The variable font ships at
`fonts/JetBrainsMono[wght].ttf` (SIL Open Font License, see `fonts/OFL.txt`).
Set `HERO_FONT` to point at a different JetBrains Mono variable font.

Render the MP4:

```bash
python3 render.py | ffmpeg -y -f rawvideo -pix_fmt rgb24 -s 1920x1080 -r 30 -i - -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart ../../public/persia-hero.mp4
```

WebM, from that MP4:

```bash
ffmpeg -y -i ../../public/persia-hero.mp4 -an -c:v libvpx-vp9 -crf 36 -b:v 0 ../../public/persia-hero.webm
```

Poster still. A timestamp argument writes `still_<seconds>.png` in this
directory; move the frame you want onto the public poster:

```bash
python3 render.py 6.8
mv still_6.8.png ../../public/poster.png
```
