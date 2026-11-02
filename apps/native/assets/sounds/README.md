# Bundled sounds

The toast sounds built into the client, which each Settings sound row offers from its list (`BundledSound` in `apps/native/src/sound.rs`). `mallet-rising-fifth.ogg` is the Needs you default and `mallet-falling-fifth.ogg` the Finished default.

Every file is mono, 44.1 kHz Ogg Vorbis, level-matched to a mean of -20 dB with a limiter at -1.5 dB:

```
ffmpeg -i <clip> -af volumedetect -f null -    # read mean_volume
ffmpeg -i <clip> -af "volume=<-20 minus mean>dB,alimiter=limit=0.84:level=false" -ar 44100 -ac 1 -c:a libvorbis -q:a 5 <out>
```

## Kenney Interface Sounds

From [Interface Sounds 1.0](https://kenney.nl/assets/interface-sounds) by Kenney, licence CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/).

| File | Pack file |
|---|---|
| `question-1.ogg` | `question_001.ogg` |
| `question-2.ogg` | `question_002.ogg` |
| `question-3.ogg` | `question_004.ogg` |
| `confirm-1.ogg` | `confirmation_001.ogg` |
| `confirm-2.ogg` | `confirmation_002.ogg` |
| `confirm-3.ogg` | `confirmation_004.ogg` |
| `glass-bright.ogg` | `glass_004.ogg` |
| `glass-short.ogg` | `glass_001.ogg` |
| `bong.ogg` | `bong_001.ogg` |

## Made by the project

Licence CC0 1.0 (https://creativecommons.org/publicdomain/zero/1.0/). No third-party material is in these.

The mallet and bell sounds are two notes rendered with ffmpeg's `aevalsrc`, the second delayed with `adelay` and mixed with `amix=normalize=0`, then faded out over the last 0.15 s and scaled by `volume=0.8`.

A mallet note at frequency `F`, 0.7 s long, the second note 110 ms later (90 ms for the third):

```
min(t*200,1)*(0.6*exp(-7*t)*sin(2*PI*F*t)+0.15*exp(-14*t)*sin(2*PI*F*4*t))
```

A bell note at frequency `F`, 1.1 s long, the second note 140 ms later:

```
0.5*exp(-3.5*t)*sin(2*PI*F*t)+0.25*exp(-5*t)*sin(2*PI*F*2.0*t)+0.12*exp(-8*t)*sin(2*PI*F*2.76*t)+0.06*exp(-12*t)*sin(2*PI*F*5.4*t)
```

| File | First note, Hz | Second note, Hz |
|---|---|---|
| `mallet-rising-fifth.ogg` | 659.3 | 987.8 |
| `mallet-falling-fifth.ogg` | 987.8 | 659.3 |
| `mallet-rising-third.ogg` | 783.99 | 987.77 |
| `bell-rising-fifth.ogg` | 880 | 1318.5 |
| `bell-falling-fifth.ogg` | 1318.5 | 880 |

`classic-rising.ogg` and `classic-falling.ogg` are the client's first sounds: 0.35 s of an 880 Hz sine then 0.35 s of a 1320 Hz sine, and the same in reverse, each with a short fade in and out.
