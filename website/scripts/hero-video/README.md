# Landing-page hero video

`render.py` draws the 16-second, 1920×1080 loop with Pillow and writes raw RGB24
frames to stdout. The site serves the committed files
`website/public/persia-hero.mp4`, `website/public/persia-hero.webm`, and
`website/public/poster.png`.

The script needs Pillow and JetBrains Mono at the path set in `render.py`.

Render the MP4:

```bash
python3 render.py | ffmpeg -f rawvideo -pix_fmt rgb24 -s 1920x1080 -r 30 -i - -c:v libx264 -crf 26 -pix_fmt yuv420p -movflags +faststart persia-hero.mp4
```

Passing timestamps writes stills (`python3 render.py 6.8` saves `still_6.8.png`).
