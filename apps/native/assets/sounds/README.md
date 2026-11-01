# Bundled sounds

`needs-you.ogg` is the Needs you sound's Default choice, built into the client. It is a two-tone chime: 0.35 s of an 880 Hz sine, then 0.35 s of a 1320 Hz sine, each with a short fade in and out, mono, 44.1 kHz, Ogg Vorbis.

The project made it itself with ffmpeg, from the repo root:

```
ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=880:sample_rate=44100:duration=0.35" -f lavfi -i "sine=frequency=1320:sample_rate=44100:duration=0.35" -filter_complex "[0]afade=t=in:d=0.01,afade=t=out:st=0.3:d=0.05[a];[1]afade=t=in:d=0.01,afade=t=out:st=0.25:d=0.1[b];[a][b]concat=n=2:v=0:a=1,volume=0.5[out]" -map "[out]" -ac 1 -c:a libvorbis -q:a 2 -map_metadata -1 -fflags +bitexact apps/native/assets/sounds/needs-you.ogg
```

Licence: CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/). No third-party material is in it.
